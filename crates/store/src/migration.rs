//! Session index schema creation and structural validation.
//!
//! SQLite 只保存会话元数据；会话正文唯一权威是 JSONL rollout 文件。

use super::*;
use std::collections::BTreeSet;

pub(crate) const EXPECTED_MIGRATIONS: &[&str] = &["0001_session_index", "0002_session_status_idle"];

pub(crate) const CURRENT_SCHEMA_SQL: &str = r#"
create table schema_meta(
schema_version integer not null check(schema_version = 2)
);
create table schema_migrations(
migration_id text primary key,
applied_at text not null default current_timestamp
);
create table session_index(
session_id text primary key,
rollout_path text not null,
cwd text not null,
title text,
model text,
status text not null
    check(status in ('idle', 'active', 'completed', 'failed', 'interrupted')),
created_at text not null,
updated_at text not null,
token_usage text not null
);
"#;

/// 0001 → 0002：SQLite 无法修改列级 CHECK，重建 `session_index` 以接受
/// `idle`（尚无 turn），并把 `schema_meta` 的版本 CHECK 抬到 2。行数据原样保留。
const UPGRADE_1_TO_2_SQL: &str = r#"
create table session_index_upgrade(
session_id text primary key,
rollout_path text not null,
cwd text not null,
title text,
model text,
status text not null
    check(status in ('idle', 'active', 'completed', 'failed', 'interrupted')),
created_at text not null,
updated_at text not null,
token_usage text not null
);
insert into session_index_upgrade
    select session_id, rollout_path, cwd, title, model, status,
           created_at, updated_at, token_usage
    from session_index;
drop table session_index;
alter table session_index_upgrade rename to session_index;
create table schema_meta_upgrade(schema_version integer not null check(schema_version = 2));
drop table schema_meta;
alter table schema_meta_upgrade rename to schema_meta;
insert into schema_meta(schema_version) values(2);
insert into schema_migrations(migration_id) values('0002_session_status_idle');
"#;

pub(crate) const CURRENT_INDEX_SQL: &str = r#"
create index session_index_updated_at on session_index(updated_at);
create index session_index_cwd on session_index(cwd);
"#;

pub(crate) fn initialize_or_validate_schema(connection: &Connection) -> StoreResult<()> {
    let tables = user_tables(connection)?;
    if tables.is_empty() {
        create_current_schema(connection)?;
        return Ok(());
    }
    let version = schema_meta_version(connection)?;
    match version {
        Some(SCHEMA_VERSION) => {}
        Some(1) => {
            let transaction = rusqlite::Transaction::new_unchecked(
                connection,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            transaction.execute_batch(UPGRADE_1_TO_2_SQL)?;
            transaction.execute_batch(CURRENT_INDEX_SQL)?;
            validate_current_schema(&transaction)?;
            transaction.commit()?;
        }
        found => {
            return Err(StoreError::UnsupportedSchema {
                found: found.unwrap_or(0),
                supported: SCHEMA_VERSION,
            });
        }
    }
    validate_current_schema(connection)
}

fn create_current_schema(connection: &Connection) -> StoreResult<()> {
    let transaction =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(CURRENT_SCHEMA_SQL)?;
    transaction.execute_batch(CURRENT_INDEX_SQL)?;
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
    validate_current_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

/// 校验当前 schema 的表、列和索引结构（防止旧/损坏库被当作索引使用）。
pub(crate) fn validate_current_schema(connection: &Connection) -> StoreResult<()> {
    let tables = user_tables(connection)?;
    let expected: BTreeSet<String> = ["schema_meta", "schema_migrations", "session_index"]
        .into_iter()
        .map(str::to_string)
        .collect();
    if tables != expected {
        return Err(StoreError::InvalidState(format!(
            "session index has unexpected tables: {tables:?}"
        )));
    }
    let columns = table_columns(connection, "session_index")?;
    let expected_columns: BTreeSet<String> = [
        "session_id",
        "rollout_path",
        "cwd",
        "title",
        "model",
        "status",
        "created_at",
        "updated_at",
        "token_usage",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if columns != expected_columns {
        return Err(StoreError::InvalidState(format!(
            "session_index has unexpected columns: {columns:?}"
        )));
    }
    let indexes = table_indexes(connection, "session_index")?;
    let expected_indexes: BTreeSet<String> = ["session_index_cwd", "session_index_updated_at"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let indexes: BTreeSet<String> = indexes
        .into_iter()
        .filter(|name| !name.starts_with("sqlite_autoindex_"))
        .collect();
    if indexes != expected_indexes {
        return Err(StoreError::InvalidState(format!(
            "session_index has unexpected indexes: {indexes:?}"
        )));
    }
    Ok(())
}

pub(crate) fn user_tables(connection: &Connection) -> StoreResult<BTreeSet<String>> {
    let mut statement = connection.prepare(
        "select name from sqlite_master
         where type = 'table' and name not like 'sqlite_%'
         order by name",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(names)
}

fn schema_meta_version(connection: &Connection) -> StoreResult<Option<u32>> {
    let Ok(version) = connection.query_row(
        "select schema_version from schema_meta limit 1",
        [],
        |row| row.get::<_, u32>(0),
    ) else {
        return Ok(None);
    };
    Ok(Some(version))
}

fn table_columns(connection: &Connection, table: &str) -> StoreResult<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("pragma table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(names)
}

fn table_indexes(connection: &Connection, table: &str) -> StoreResult<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("pragma index_list({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut indexes = BTreeSet::new();
    for name in rows {
        indexes.insert(name?);
    }
    Ok(indexes)
}
