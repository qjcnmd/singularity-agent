//! Schema creation and canonical schema validation.

use super::support::*;
use super::*;

const EXPECTED_MIGRATIONS: [&str; 12] = [
    INITIAL_SCHEMA_MIGRATION,
    DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION,
    PENDING_TOOL_CALL_SCHEMA_MIGRATION,
    STORE_HARDENING_SCHEMA_MIGRATION,
    CONVERSATION_HISTORY_SCHEMA_MIGRATION,
    PENDING_EXECUTION_STATE_SCHEMA_MIGRATION,
    APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION,
    THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION,
    STABLE_ENUM_TEXT_SCHEMA_MIGRATION,
    TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION,
    TYPED_TRACE_SPAN_SCHEMA_MIGRATION,
    TURN_RESUME_CHECKPOINT_SCHEMA_MIGRATION,
];

impl SessionStore {
    /// 返回已应用 migration id 的持久化顺序。
    pub fn applied_migrations(&self) -> StoreResult<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("select migration_id from schema_migrations order by migration_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut migrations = Vec::new();
        for row in rows {
            migrations.push(row?);
        }
        Ok(migrations)
    }
}

pub(crate) fn initialize_or_validate_schema(connection: &Connection) -> StoreResult<()> {
    let tables = user_tables(connection)?;
    if tables.is_empty() {
        create_v12_schema(connection)?;
        return Ok(());
    }
    let version = detect_schema_version(connection)?;
    if version != SCHEMA_VERSION {
        // 旧版本（v1–v12）schema 已不再支持迁移；除当前版本外的任何版本都 fail closed。
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    validate_v12_schema(connection)
}

fn create_v12_schema(connection: &Connection) -> StoreResult<()> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    transaction.execute_batch(&canonical_v12_schema_sql(""))?;
    transaction.execute_batch(&v12_index_sql())?;
    for migration in EXPECTED_MIGRATIONS {
        transaction.execute(
            "insert into schema_migrations(migration_id) values(?1)",
            params![migration],
        )?;
    }
    transaction.execute(
        "insert into schema_meta(schema_version) values(?1)",
        params![SCHEMA_VERSION],
    )?;
    validate_v12_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

// 当前 schema 的 threads 定义：policy 快照列已随 approval/sandbox 消费面删除；
// trace/artifact 表已随 trace 全链删除。
const CURRENT_SCHEMA_SQL: &str = r#"
create table schema_meta(
schema_version integer not null check(schema_version = 11)
);
create table schema_migrations(
migration_id text primary key,
applied_at text not null default current_timestamp
);
create table threads(
thread_id text primary key,
model text,
cwd text,
status text not null default 'active'
    check(status in ('active', 'archived'))
);
create table turns(
turn_id text primary key,
thread_id text not null,
turn_sequence integer not null check(turn_sequence > 0),
status text not null
    check(status in ('running', 'completed', 'blocked', 'failed', 'interrupted')),
agent_loop_status text not null,
foreign key(thread_id) references threads(thread_id)
);
create table items(
item_id text primary key,
turn_id text not null,
item_sequence integer not null check(item_sequence > 0),
kind text not null
    check(kind in ('userMessage', 'agentMessage', 'reasoning', 'commandExecution', 'fileChange')),
payload text not null,
status text not null check(status in ('started', 'completed')),
redacted integer not null check(redacted in (0, 1)),
foreign key(turn_id) references turns(turn_id)
);
"#;

// 当前 schema 的索引面。
const CURRENT_INDEX_SQL: &str = r#"
create unique index turns_thread_sequence_unique on turns(thread_id, turn_sequence);
create unique index items_turn_sequence_unique on items(turn_id, item_sequence);
create index turns_history_lookup on turns(thread_id, status, turn_sequence);
create index items_history_lookup on items(turn_id, status, kind, item_sequence);
"#;

fn v12_index_sql() -> String {
    CURRENT_INDEX_SQL.to_string()
}

// Build the current schema shape from CURRENT_SCHEMA_SQL: v13 turn states and
// the pause_requested column.
fn canonical_v12_schema_sql(suffix: &str) -> String {
    let mut sql = canonical_v11_schema_sql(suffix);
    sql = sql.replace("schema_version = 11", "schema_version = 13");
    sql = sql.replace(
        "status in ('running', 'completed', 'blocked', 'failed', 'interrupted')",
        "status in ('running', 'paused', 'suspended', 'completed', 'blocked', 'failed', 'interrupted')",
    );
    sql = sql.replace(
        "agent_loop_status text not null,\nforeign key(thread_id)",
        "agent_loop_status text not null,\npause_requested integer not null default 0 check(pause_requested in (0, 1)),\nforeign key(thread_id)",
    );
    sql
}

fn canonical_v11_schema_sql(suffix: &str) -> String {
    if suffix.is_empty() {
        return CURRENT_SCHEMA_SQL.to_string();
    }
    let mut sql = CURRENT_SCHEMA_SQL.to_string();
    for table in [
        "schema_meta",
        "schema_migrations",
        "threads",
        "turns",
        "items",
    ] {
        sql = sql.replace(table, &format!("{table}{suffix}"));
    }
    sql
}

fn user_tables(connection: &Connection) -> StoreResult<BTreeSet<String>> {
    let mut statement = connection.prepare(
        "select name from sqlite_master
     where type = 'table' and name not like 'sqlite_%' order by name",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(StoreError::Sqlite)
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    connection
        .query_row(
            "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
            params![table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::Sqlite)
}

fn schema_meta_version(connection: &Connection) -> StoreResult<Option<u32>> {
    if !table_exists(connection, "schema_meta")? {
        return Ok(None);
    }
    let versions = connection
        .prepare("select schema_version from schema_meta order by rowid")?
        .query_map([], |row| row.get::<_, u64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    match versions.as_slice() {
        [version] => u32::try_from(*version)
            .map(Some)
            .map_err(|_| StoreError::InvalidState("schema version is out of range".to_string())),
        [] => Err(StoreError::InvalidState(
            "schema_meta must contain exactly one schema version".to_string(),
        )),
        _ => Err(StoreError::InvalidState(
            "schema_meta contains multiple schema versions".to_string(),
        )),
    }
}

fn read_migration_markers(connection: &Connection) -> StoreResult<BTreeSet<String>> {
    if !table_exists(connection, "schema_migrations")? {
        return Ok(BTreeSet::new());
    }
    let mut statement = connection.prepare("select migration_id from schema_migrations")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut markers = BTreeSet::new();
    for row in rows {
        let marker = row?;
        if marker.trim().is_empty() {
            return Err(StoreError::InvalidState(
                "schema migration marker must not be empty".to_string(),
            ));
        }
        if !markers.insert(marker.clone()) {
            return Err(StoreError::InvalidState(format!(
                "duplicate schema migration marker {marker}"
            )));
        }
    }
    Ok(markers)
}

fn detect_schema_version(connection: &Connection) -> StoreResult<u32> {
    schema_meta_version(connection)?.ok_or_else(|| {
        StoreError::InvalidState(
            "store schema is missing schema_meta; legacy schemas are no longer supported"
                .to_string(),
        )
    })
}

fn validate_current_markers(connection: &Connection) -> StoreResult<()> {
    let markers = read_migration_markers(connection)?;
    let expected = EXPECTED_MIGRATIONS
        .iter()
        .map(|migration| (*migration).to_string())
        .collect::<BTreeSet<_>>();
    if markers != expected {
        return Err(StoreError::InvalidState(
            "current schema migration markers are incomplete or unknown".to_string(),
        ));
    }
    Ok(())
}

// --- 当前 schema 的行级校验 ---

// 行级校验只关心跨行不变量需要的字段（id 与 sequence）；枚举、payload 与
// redaction 标志在读取时校验但不在校验结果中保留。
struct ThreadRow {
    thread_id: String,
}

struct TurnRow {
    turn_id: String,
    thread_id: String,
    turn_sequence: i64,
}

struct ItemRow {
    item_id: String,
    turn_id: String,
    item_sequence: i64,
}

struct StoredRows {
    threads: Vec<ThreadRow>,
    turns: Vec<TurnRow>,
    items: Vec<ItemRow>,
}

fn require_core_tables(connection: &Connection) -> StoreResult<()> {
    for table in ["threads", "turns", "items"] {
        if !table_exists(connection, table)? {
            return Err(StoreError::InvalidState(format!(
                "current schema is missing required table {table}"
            )));
        }
    }
    Ok(())
}

fn read_thread_rows(connection: &Connection) -> StoreResult<Vec<ThreadRow>> {
    let mut statement =
        connection.prepare("select thread_id, status from threads order by rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut threads = Vec::new();
    for row in rows {
        let (thread_id, status) = row?;
        ThreadStatus::from_db_text(&status)
            .ok_or_else(|| unknown_db_enum(ThreadStatus::LABEL, &status))?;
        threads.push(ThreadRow { thread_id });
    }
    Ok(threads)
}

fn read_turn_rows(connection: &Connection) -> StoreResult<Vec<TurnRow>> {
    let mut statement = connection
        .prepare("select turn_id, thread_id, turn_sequence, status from turns order by rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (turn_id, thread_id, turn_sequence, status) = row?;
        TurnStatus::from_db_text(&status)
            .ok_or_else(|| unknown_db_enum(TurnStatus::LABEL, &status))?;
        turns.push(TurnRow {
            turn_id,
            thread_id,
            turn_sequence,
        });
    }
    Ok(turns)
}

fn read_item_rows(connection: &Connection) -> StoreResult<Vec<ItemRow>> {
    let mut statement = connection.prepare(
        "select item_id, turn_id, item_sequence, kind, payload, status, redacted from items order by rowid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (item_id, turn_id, item_sequence, kind, payload, status, redacted) = row?;
        let kind =
            ItemKind::from_db_text(&kind).ok_or_else(|| unknown_db_enum(ItemKind::LABEL, &kind))?;
        ItemStatus::from_db_text(&status)
            .ok_or_else(|| unknown_db_enum(ItemStatus::LABEL, &status))?;
        let payload: Value = serde_json::from_str(&payload)?;
        let (_, detected_redaction) = sanitize_item_payload(&kind, payload)?;
        if !matches!(redacted, 0 | 1) {
            return Err(StoreError::InvalidState(
                "item redaction flag is invalid".to_string(),
            ));
        }
        let _ = detected_redaction;
        items.push(ItemRow {
            item_id,
            turn_id,
            item_sequence,
        });
    }
    Ok(items)
}

fn validate_row_sequences(data: &StoredRows) -> StoreResult<()> {
    let mut thread_ids = BTreeSet::new();
    for thread in &data.threads {
        if thread.thread_id.trim().is_empty() || !thread_ids.insert(thread.thread_id.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "duplicate or empty thread {}",
                thread.thread_id
            )));
        }
    }
    let mut turn_ids = BTreeSet::new();
    let mut turn_sequences = BTreeSet::new();
    for turn in &data.turns {
        if turn.turn_id.trim().is_empty() || turn.thread_id.trim().is_empty() {
            return Err(StoreError::InvalidState(
                "turn id and thread binding must not be empty".to_string(),
            ));
        }
        if !thread_ids.contains(turn.thread_id.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "turn {} references a missing thread",
                turn.turn_id
            )));
        }
        if turn.turn_sequence <= 0
            || !turn_sequences.insert((turn.thread_id.as_str(), turn.turn_sequence))
        {
            return Err(StoreError::InvalidState(format!(
                "turn {} has an invalid or duplicate sequence",
                turn.turn_id
            )));
        }
        if !turn_ids.insert(turn.turn_id.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "duplicate turn {}",
                turn.turn_id
            )));
        }
    }
    let mut item_ids = BTreeSet::new();
    let mut item_sequences = BTreeSet::new();
    for item in &data.items {
        if item.item_id.trim().is_empty() || item.turn_id.trim().is_empty() {
            return Err(StoreError::InvalidState(
                "item id and turn binding must not be empty".to_string(),
            ));
        }
        if !turn_ids.contains(item.turn_id.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "item {} references a missing turn",
                item.item_id
            )));
        }
        if item.item_sequence <= 0
            || !item_sequences.insert((item.turn_id.as_str(), item.item_sequence))
        {
            return Err(StoreError::InvalidState(format!(
                "item {} has an invalid or duplicate sequence",
                item.item_id
            )));
        }
        if !item_ids.insert(item.item_id.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "duplicate item {}",
                item.item_id
            )));
        }
    }
    Ok(())
}

fn read_current_rows(connection: &Connection) -> StoreResult<StoredRows> {
    require_core_tables(connection)?;
    let threads = read_thread_rows(connection)?;
    let turns = read_turn_rows(connection)?;
    let items = read_item_rows(connection)?;
    let data = StoredRows {
        threads,
        turns,
        items,
    };
    validate_row_sequences(&data)?;
    Ok(data)
}

fn validate_v12_schema(connection: &Connection) -> StoreResult<()> {
    validate_v12_structure(connection)?;
    read_current_rows(connection)?;
    fail_closed_on_foreign_key_violations(connection, "current schema validation")?;
    Ok(())
}

// Validate the immutable current-schema shape without scanning or decoding every
// stored row.  Trusted reopen uses this after the owning process initialized
// the database; row payloads remain validated at each read or transaction.
pub(crate) fn validate_v12_structure(connection: &Connection) -> StoreResult<()> {
    if schema_meta_version(connection)? != Some(SCHEMA_VERSION) {
        return Err(StoreError::InvalidState(
            "current schema_meta version is missing or inconsistent".to_string(),
        ));
    }
    validate_current_markers(connection)?;
    validate_canonical_v12_fingerprint(connection)
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SchemaFingerprint {
    objects: Vec<SchemaObjectFingerprint>,
    tables: Vec<TableFingerprint>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SchemaObjectFingerprint {
    kind: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TableFingerprint {
    name: String,
    columns: Vec<ColumnFingerprint>,
    indexes: Vec<IndexFingerprint>,
    foreign_keys: Vec<ForeignKeyFingerprint>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ColumnFingerprint {
    cid: i64,
    name: String,
    type_name: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IndexFingerprint {
    // SQLite assigns implementation-detail names to PRIMARY KEY and UNIQUE
    // autoindexes. Their origin and complete xinfo remain part of the contract.
    explicit_name: Option<String>,
    unique: bool,
    origin: String,
    partial: bool,
    sql: Option<String>,
    columns: Vec<IndexColumnFingerprint>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IndexColumnFingerprint {
    sequence: i64,
    column_id: i64,
    name: Option<String>,
    descending: bool,
    collation: Option<String>,
    key: bool,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyFingerprint {
    id: i64,
    sequence: i64,
    parent_table: String,
    from_column: String,
    to_column: Option<String>,
    on_update: String,
    on_delete: String,
    match_name: String,
}

fn validate_canonical_v12_fingerprint(connection: &Connection) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(&canonical_v12_schema_sql(""))?;
    reference.execute_batch(&v12_index_sql())?;
    let expected = schema_fingerprint(&reference)?;
    let actual = schema_fingerprint(connection)?;
    if actual != expected {
        return Err(StoreError::InvalidState(
            "current schema fingerprint is not canonical".to_string(),
        ));
    }
    Ok(())
}

fn schema_fingerprint(connection: &Connection) -> StoreResult<SchemaFingerprint> {
    let mut object_statement = connection.prepare(
        "select type, name, tbl_name, sql from sqlite_schema
         where name not like 'sqlite_%' order by type, name",
    )?;
    let object_rows = object_statement.query_map([], |row| {
        Ok(SchemaObjectFingerprint {
            kind: normalized_identifier(row.get::<_, String>(0)?),
            name: normalized_identifier(row.get::<_, String>(1)?),
            table_name: normalized_identifier(row.get::<_, String>(2)?),
            sql: row
                .get::<_, Option<String>>(3)?
                .map(|sql| normalize_sql(&sql)),
        })
    })?;
    let mut objects = object_rows.collect::<Result<Vec<_>, _>>()?;
    objects.sort();

    let table_names = objects
        .iter()
        .filter(|object| object.kind == "table")
        .map(|object| object.name.clone())
        .collect::<Vec<_>>();
    let mut tables = Vec::with_capacity(table_names.len());
    for table_name in table_names {
        tables.push(TableFingerprint {
            columns: table_fingerprint_columns(connection, &table_name)?,
            indexes: table_fingerprint_indexes(connection, &table_name)?,
            foreign_keys: table_fingerprint_foreign_keys(connection, &table_name)?,
            name: table_name,
        });
    }
    tables.sort();
    Ok(SchemaFingerprint { objects, tables })
}

fn table_fingerprint_columns(
    connection: &Connection,
    table_name: &str,
) -> StoreResult<Vec<ColumnFingerprint>> {
    let mut statement = connection.prepare(
        "select cid, name, type, \"notnull\", dflt_value, pk, hidden
         from pragma_table_xinfo(?1) order by cid",
    )?;
    let rows = statement.query_map(params![table_name], |row| {
        Ok(ColumnFingerprint {
            cid: row.get(0)?,
            name: normalized_identifier(row.get::<_, String>(1)?),
            type_name: normalize_sql(&row.get::<_, String>(2)?),
            not_null: row.get::<_, i64>(3)? != 0,
            default: row
                .get::<_, Option<String>>(4)?
                .map(|value| normalize_sql(&value)),
            primary_key: row.get(5)?,
            hidden: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::Sqlite)
}

fn table_fingerprint_indexes(
    connection: &Connection,
    table_name: &str,
) -> StoreResult<Vec<IndexFingerprint>> {
    let mut statement = connection
        .prepare("select name, \"unique\", origin, partial from pragma_index_list(?1)")?;
    let rows = statement.query_map(params![table_name], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)? != 0,
            normalized_identifier(row.get::<_, String>(2)?),
            row.get::<_, i64>(3)? != 0,
        ))
    })?;
    let metadata = rows.collect::<Result<Vec<_>, _>>()?;
    let mut indexes = Vec::with_capacity(metadata.len());
    for (name, unique, origin, partial) in metadata {
        let mut xinfo_statement = connection.prepare(
            "select seqno, cid, name, \"desc\", coll, \"key\"
             from pragma_index_xinfo(?1) order by seqno",
        )?;
        let xinfo_rows = xinfo_statement.query_map(params![&name], |row| {
            Ok(IndexColumnFingerprint {
                sequence: row.get(0)?,
                column_id: row.get(1)?,
                name: row.get::<_, Option<String>>(2)?.map(normalized_identifier),
                descending: row.get::<_, i64>(3)? != 0,
                collation: row.get::<_, Option<String>>(4)?.map(normalized_identifier),
                key: row.get::<_, i64>(5)? != 0,
            })
        })?;
        let columns = xinfo_rows.collect::<Result<Vec<_>, _>>()?;
        let sql = connection
            .query_row(
                "select sql from sqlite_schema where type = 'index' and name = ?1",
                params![&name],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(|value| normalize_sql(&value));
        indexes.push(IndexFingerprint {
            explicit_name: (origin == "c").then(|| normalized_identifier(&name)),
            unique,
            origin,
            partial,
            sql,
            columns,
        });
    }
    indexes.sort();
    Ok(indexes)
}

fn table_fingerprint_foreign_keys(
    connection: &Connection,
    table_name: &str,
) -> StoreResult<Vec<ForeignKeyFingerprint>> {
    let mut statement = connection.prepare(
        "select id, seq, \"table\", \"from\", \"to\", on_update, on_delete, \"match\"
         from pragma_foreign_key_list(?1) order by id, seq",
    )?;
    let rows = statement.query_map(params![table_name], |row| {
        Ok(ForeignKeyFingerprint {
            id: row.get(0)?,
            sequence: row.get(1)?,
            parent_table: normalized_identifier(row.get::<_, String>(2)?),
            from_column: normalized_identifier(row.get::<_, String>(3)?),
            to_column: row.get::<_, Option<String>>(4)?.map(normalized_identifier),
            on_update: normalized_identifier(row.get::<_, String>(5)?),
            on_delete: normalized_identifier(row.get::<_, String>(6)?),
            match_name: normalized_identifier(row.get::<_, String>(7)?),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::Sqlite)
}

fn normalized_identifier(value: impl AsRef<str>) -> String {
    value.as_ref().to_ascii_lowercase()
}

// Normalize SQL syntax and identifier quoting only. Single-quoted literal
// contents remain byte-for-byte significant, including case and whitespace.
fn normalize_sql(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    let mut in_literal = false;
    while let Some(character) = characters.next() {
        if in_literal {
            normalized.push(character);
            if character == '\'' {
                if characters.peek() == Some(&'\'') {
                    normalized.push(characters.next().expect("peeked escaped quote"));
                } else {
                    in_literal = false;
                }
            }
            continue;
        }
        match character {
            '\'' => {
                in_literal = true;
                normalized.push(character);
            }
            '"' | '`' | '[' | ']' => {}
            _ if character.is_whitespace() => {}
            _ => normalized.extend(character.to_lowercase()),
        }
    }
    normalized
}
