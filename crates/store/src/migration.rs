//! Schema creation, legacy detection, migration, and canonical schema validation.

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

const KNOWN_LEGACY_MIGRATIONS: [&str; 13] = [
    INITIAL_SCHEMA_MIGRATION,
    DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION,
    RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION,
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

#[derive(Debug, Clone)]
struct LegacyThreadRow {
    thread_id: String,
    model: Option<String>,
    cwd: Option<String>,
    status: ThreadStatus,
}

#[derive(Debug, Clone)]
struct LegacyTurnRow {
    turn_id: String,
    thread_id: String,
    turn_sequence: i64,
    status: TurnStatus,
    agent_loop_status: String,
}

#[derive(Debug, Clone)]
struct LegacyItemRow {
    item_id: String,
    turn_id: String,
    item_sequence: i64,
    kind: ItemKind,
    payload: Value,
    status: ItemStatus,
    redacted: bool,
}

#[derive(Debug, Clone)]
struct LegacyData {
    threads: Vec<LegacyThreadRow>,
    turns: Vec<LegacyTurnRow>,
    items: Vec<LegacyItemRow>,
}

pub(crate) fn initialize_or_validate_schema(connection: &Connection) -> StoreResult<()> {
    let tables = user_tables(connection)?;
    if tables.is_empty() {
        create_v12_schema(connection)?;
        return Ok(());
    }

    let version = detect_schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return validate_v12_schema(connection);
    }

    migrate_legacy_schema(connection, version)?;
    // The migration transaction already performed the complete current-schema data
    // validation before commit.  Recheck only the committed structure here;
    // callers opening an already-v11 store use the full validator above.
    validate_v12_structure(connection)
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
// trace/artifact 表已随 trace 全链删除（旧库迁移时直接丢弃）。
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

// 已发布 v11 指纹：旧库校验仍要求 approval 三表与 threads policy 列；
// 当前 schema（CURRENT_SCHEMA_SQL）不再创建它们，旧库迁移时直接丢弃。
const V11_SCHEMA_SQL: &str = r#"
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
    check(status in ('active', 'archived')),
sandbox_mode text not null default 'workspace-write'
    check(sandbox_mode in ('read-only', 'workspace-write')),
approval_policy text not null default 'on-request'
    check(approval_policy in ('on-request', 'never'))
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
create table trace_events(
event_id text primary key,
run_id text not null,
session_id text not null default '',
payload text not null
);
create table approvals(
request_id text primary key,
thread_id text not null,
turn_id text not null,
payload text not null,
decision_outcome text
    check(decision_outcome in ('allow', 'deny') or decision_outcome is null),
decision_reason text,
foreign key(thread_id) references threads(thread_id),
foreign key(turn_id) references turns(turn_id)
);
create table approval_decisions(
decision_id text primary key,
request_id text not null,
outcome text not null check(outcome in ('allow', 'deny')),
reason text not null,
payload text not null,
foreign key(request_id) references approvals(request_id)
);
create table artifact_refs(
artifact_id text primary key,
run_id text not null,
item_id text,
kind text not null,
uri text not null,
content_digest text not null,
summary text not null,
metadata text not null,
redacted integer not null check(redacted in (0, 1))
);
create table pending_tool_calls(
request_id text primary key,
thread_id text not null,
turn_id text not null,
tool_call_id text not null,
payload text not null,
execution_state text not null default 'pending'
    check(execution_state in ('pending', 'executing')),
foreign key(request_id) references approvals(request_id),
foreign key(thread_id) references threads(thread_id),
foreign key(turn_id) references turns(turn_id)
);
"#;

const V11_INDEX_SQL: &str = r#"
create unique index turns_thread_sequence_unique on turns(thread_id, turn_sequence);
create unique index items_turn_sequence_unique on items(turn_id, item_sequence);
create index turns_history_lookup on turns(thread_id, status, turn_sequence);
create index items_history_lookup on items(turn_id, status, kind, item_sequence);
create unique index approval_decisions_request_unique on approval_decisions(request_id);
create index trace_run_lookup on trace_events(run_id, event_id);
create index approvals_pending_lookup on approvals(decision_outcome, request_id);
create index approvals_thread_lookup on approvals(thread_id, decision_outcome, request_id);
create index approvals_turn_lookup on approvals(turn_id, decision_outcome, request_id);
create index pending_tool_calls_turn_state on pending_tool_calls(turn_id, execution_state, request_id);
"#;

// 当前 schema 的索引面：V11 指纹保留 approval/trace 索引供旧库校验，当前库不再创建。
const CURRENT_INDEX_SQL: &str = r#"
create unique index turns_thread_sequence_unique on turns(thread_id, turn_sequence);
create unique index items_turn_sequence_unique on items(turn_id, item_sequence);
create index turns_history_lookup on turns(thread_id, status, turn_sequence);
create index items_history_lookup on items(turn_id, status, kind, item_sequence);
"#;

// Recovery tables are introduced with the current schema and must not be part
// of the released v11 fingerprint used to validate legacy databases.
const V13_RECOVERY_INDEX_SQL: &str = r#"
create unique index turn_inputs_item_unique on turn_inputs(item_id);
create index turn_inputs_pending on turn_inputs(turn_id, delivery_state, delivery, item_id);
"#;

const V12_TRACE_INDEX_SQL: &str = r#"
create unique index trace_span_phase_unique
    on trace_events(run_id, span_id, span_phase)
    where span_id is not null;
create index trace_span_parent_lookup
    on trace_events(run_id, parent_span_id, span_id)
    where parent_span_id is not null;
"#;

const V12_TRACE_TRIGGER_SQL: &str = r#"
create trigger trace_span_lifecycle_insert
before insert on trace_events
when json_extract(new.payload, '$.span_id') is not null
 and (
     (json_extract(new.payload, '$.parent_span_id') is not null and not exists(
         select 1 from trace_events
         where run_id = new.run_id
           and span_id = json_extract(new.payload, '$.parent_span_id')
     ))
     or (json_extract(new.payload, '$.span_phase') = 'start' and exists(
         select 1 from trace_events
         where run_id = new.run_id
           and span_id = json_extract(new.payload, '$.span_id')
           and span_phase = 'start'
     ))
     or (json_extract(new.payload, '$.span_phase') = 'end' and (
         not exists(
             select 1 from trace_events
             where run_id = new.run_id
               and span_id = json_extract(new.payload, '$.span_id')
               and span_phase = 'start'
         )
         or exists(
             select 1 from trace_events
             where run_id = new.run_id
               and span_id = json_extract(new.payload, '$.span_id')
               and span_phase = 'end'
         )
         or exists(
             select 1 from trace_events
             where run_id = new.run_id
               and span_id = json_extract(new.payload, '$.span_id')
               and span_phase = 'start'
               and (parent_span_id is not json_extract(new.payload, '$.parent_span_id')
                    or span_kind is not json_extract(new.payload, '$.span_kind'))
         )
     ))
 )
begin
    select raise(abort, 'invalid trace span lifecycle');
end;

create trigger trace_span_projection_insert
after insert on trace_events
begin
    update trace_events
       set span_id = json_extract(new.payload, '$.span_id'),
           parent_span_id = json_extract(new.payload, '$.parent_span_id'),
           span_kind = json_extract(new.payload, '$.span_kind'),
           span_phase = json_extract(new.payload, '$.span_phase'),
           span_status = json_extract(new.payload, '$.span_status'),
           duration_ms = json_extract(new.payload, '$.duration_ms'),
           time_to_first_token_ms = json_extract(new.payload, '$.time_to_first_token_ms'),
           span_projection = json_extract(new.payload, '$.span_projection'),
           metric_samples = coalesce(json_extract(new.payload, '$.metric_samples'), '[]')
     where event_id = new.event_id;
end;
"#;

fn v12_index_sql() -> String {
    format!("{CURRENT_INDEX_SQL}{V13_RECOVERY_INDEX_SQL}")
}

// Reconstruct the released v12 shape so an existing v12 store can be upgraded
// transactionally to the current v13 schema. This legacy shape keeps the typed
// trace span columns and recovery tables for fingerprint validation of old stores.
fn canonical_v12_legacy_schema_sql(suffix: &str) -> String {
    let mut sql = legacy_v11_schema_sql(suffix);
    sql = sql.replace("schema_version = 11", "schema_version = 12");
    let old_trace = format!(
        "create table trace_events{suffix}(\n\
event_id text primary key,\n\
run_id text not null,\n\
session_id text not null default '',\n\
payload text not null\n\
);"
    );
    let new_trace = format!(
        "create table trace_events{suffix}(\n\
event_id text primary key,\n\
run_id text not null,\n\
session_id text not null default '',\n\
payload text not null,\n\
span_id text\n    check(span_id is null or length(trim(span_id)) > 0),\n\
parent_span_id text\n    check(parent_span_id is null or length(trim(parent_span_id)) > 0),\n\
  span_kind text\n    check(span_kind in ('task', 'turn', 'prompt_assembly', 'provider_attempt', 'tool_call',\n                        'policy_decision', 'approval_wait', 'sandbox_execution',\n                        'verification', 'final_review')\n          or span_kind is null),\n\
span_phase text\n    check(span_phase in ('start', 'end') or span_phase is null),\n\
  span_status text\n    check(span_status in ('unset', 'ok', 'error', 'cancelled') or span_status is null),\n\
duration_ms integer\n    check(duration_ms >= 0 or duration_ms is null),\n\
  time_to_first_token_ms integer\n    check(time_to_first_token_ms >= 0 or time_to_first_token_ms is null),\n\
  span_projection text\n    check(span_projection is null or json_valid(span_projection)),\n\
  metric_samples text not null default '[]'\n    check(json_valid(metric_samples) and json_type(metric_samples) = 'array'),\n\
  check((span_id is null and parent_span_id is null and span_kind is null\n       and span_phase is null and span_status is null and duration_ms is null\n       and time_to_first_token_ms is null and span_projection is null)\n      or (span_id is not null and span_kind is not null and span_phase is not null)),\n\
  check((span_phase = 'start' and span_status is null and duration_ms is null\n       and time_to_first_token_ms is null and metric_samples = '[]')\n      or (span_phase = 'end' and span_status is not null and duration_ms is not null)\n      or span_phase is null),\n\
check(time_to_first_token_ms is null or span_kind = 'provider_attempt'),\n\
check(time_to_first_token_ms is null or duration_ms is null\n      or time_to_first_token_ms <= duration_ms),\n\
check(parent_span_id is null or parent_span_id <> span_id)\n\
);"
    );
    sql.replace(&old_trace, &new_trace)
}

// Build the current schema shape from CURRENT_SCHEMA_SQL: v13 turn states,
// pause_requested column, and the durable turn_inputs delivery table.
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
    let table_suffix = suffix;
    sql.push_str(&format!(
        "\ncreate table turn_inputs{table_suffix}(\n\
input_id text primary key,\n\
turn_id text not null,\n\
item_id text not null,\n\
delivery text not null check(delivery in ('steer', 'follow_up')),\n\
delivery_state text not null check(delivery_state in ('pending', 'consumed')),\n\
created_at text not null default current_timestamp,\n\
consumed_at text,\n\
check((delivery_state = 'pending' and consumed_at is null)\n\
   or (delivery_state = 'consumed' and consumed_at is not null)),\n\
foreign key(turn_id) references turns(turn_id),\n\
foreign key(item_id) references items(item_id)\n\
);\n"
    ));
    sql
}

#[derive(Debug, Clone, Copy)]
enum LegacyTraceLayout {
    SessionBeforePayload,
    SessionAfterPayload,
}

#[derive(Debug, Clone, Copy)]
enum LegacyHistoryIndexes {
    UniqueOnly,
    Full,
}

#[derive(Debug, Clone, Copy)]
enum LegacyV7PendingConstraint {
    FreshFourStateCheck,
    UpgradedWithoutCheck,
}

// Reconstruct one exact schema shape emitted by a released v1-v9 store.
// Product upgrades left three intentional variants behind: the v2 trace
// column could be appended to a v1 table, v6 initially shipped only its
// uniqueness indexes, and v7 appended execution_state without a CHECK when
// upgrading an existing pending table.  No other structural drift is valid.
fn legacy_reference_schema_sql(
    version: u32,
    include_retired_sidecar: bool,
    trace_layout: LegacyTraceLayout,
    history_indexes: LegacyHistoryIndexes,
    v7_pending_constraint: LegacyV7PendingConstraint,
) -> String {
    if version == 10 {
        let mut sql = V11_SCHEMA_SQL.replace("schema_version = 11", "schema_version = 10");
        sql.push_str(V11_INDEX_SQL);
        return sql;
    }
    if version == 11 {
        let mut sql = V11_SCHEMA_SQL.to_string();
        sql.push_str(V11_INDEX_SQL);
        return sql;
    }
    if version == 12 {
        let mut sql = canonical_v12_legacy_schema_sql("");
        sql.push_str(&format!(
            "{V11_INDEX_SQL}{V12_TRACE_INDEX_SQL}{V12_TRACE_TRIGGER_SQL}"
        ));
        return sql;
    }
    let mut sql = String::new();
    if version >= 5 {
        sql.push_str("create table schema_meta(schema_version integer not null);");
    }
    if version >= 2 {
        sql.push_str(
            "create table schema_migrations(
                migration_id text primary key,
                applied_at text not null default current_timestamp
            );",
        );
    }
    if version >= 9 {
        sql.push_str(
            "create table threads(
                thread_id text primary key,
                model text,
                cwd text,
                status text not null,
                sandbox_mode text not null default '\"workspace-write\"',
                approval_policy text not null default '\"on-request\"'
            );",
        );
    } else {
        sql.push_str(
            "create table threads(
                thread_id text primary key,
                model text,
                cwd text,
                status text not null
            );",
        );
    }
    if version >= 6 {
        sql.push_str(
            "create table turns(
                turn_id text primary key,
                thread_id text not null,
                turn_sequence integer not null check(turn_sequence > 0),
                status text not null,
                agent_loop_status text not null,
                foreign key(thread_id) references threads(thread_id)
            );
            create table items(
                item_id text primary key,
                turn_id text not null,
                item_sequence integer not null check(item_sequence > 0),
                kind text not null,
                payload text not null,
                status text not null,
                redacted integer not null check(redacted in (0, 1)),
                foreign key(turn_id) references turns(turn_id)
            );",
        );
    } else if version >= 5 {
        sql.push_str(
            "create table turns(
                turn_id text primary key,
                thread_id text not null,
                status text not null,
                agent_loop_status text not null,
                foreign key(thread_id) references threads(thread_id)
            );
            create table items(
                item_id text primary key,
                turn_id text not null,
                kind text not null,
                payload text not null,
                status text not null,
                foreign key(turn_id) references turns(turn_id)
            );",
        );
    } else {
        sql.push_str(
            "create table turns(
                turn_id text primary key,
                thread_id text not null,
                status text not null,
                agent_loop_status text not null
            );
            create table items(
                item_id text primary key,
                turn_id text not null,
                kind text not null,
                payload text not null,
                status text not null
            );",
        );
    }
    if version == 1 {
        sql.push_str(
            "create table trace_events(
                event_id text primary key,
                run_id text not null,
                payload text not null
            );",
        );
    } else {
        match trace_layout {
            LegacyTraceLayout::SessionBeforePayload => sql.push_str(
                "create table trace_events(
                    event_id text primary key,
                    run_id text not null,
                    session_id text not null default '',
                    payload text not null
                );",
            ),
            LegacyTraceLayout::SessionAfterPayload => sql.push_str(
                "create table trace_events(
                    event_id text primary key,
                    run_id text not null,
                    payload text not null,
                    session_id text not null default ''
                );",
            ),
        }
    }
    sql.push_str(
        "create table approvals(
            request_id text primary key,
            payload text not null,
            decision_outcome text,
            decision_reason text
        );",
    );
    if version >= 2 {
        if version >= 5 {
            sql.push_str(
                "create table approval_decisions(
                    decision_id text primary key,
                    request_id text not null,
                    outcome text not null,
                    reason text not null,
                    payload text not null,
                    foreign key(request_id) references approvals(request_id)
                );",
            );
        } else {
            sql.push_str(
                "create table approval_decisions(
                    decision_id text primary key,
                    request_id text not null,
                    outcome text not null,
                    reason text not null,
                    payload text not null
                );",
            );
        }
        sql.push_str(
            "create table artifact_refs(
                artifact_id text primary key,
                run_id text not null,
                item_id text,
                kind text not null,
                uri text not null,
                content_digest text not null,
                summary text not null,
                metadata text not null,
                redacted integer not null
            );",
        );
    }
    if include_retired_sidecar {
        sql.push_str(
            "create table active_sidecar_runs(
                turn_id text primary key,
                thread_id text not null,
                run_id text not null,
                session_id text not null,
                task_id text not null,
                status text not null,
                created_at text not null default current_timestamp,
                updated_at text not null default current_timestamp
            );",
        );
    }
    match version {
        4 => sql.push_str(
            "create table pending_tool_calls(
                request_id text primary key,
                turn_id text not null,
                payload text not null
            );",
        ),
        5 | 6 => sql.push_str(
            "create table pending_tool_calls(
                request_id text primary key,
                thread_id text not null,
                turn_id text not null,
                tool_call_id text not null,
                payload text not null,
                foreign key(request_id) references approvals(request_id),
                foreign key(thread_id) references threads(thread_id),
                foreign key(turn_id) references turns(turn_id)
            );",
        ),
        7 => {
            let state = match v7_pending_constraint {
                LegacyV7PendingConstraint::FreshFourStateCheck => {
                    "execution_state text not null default 'pending' check(execution_state in ('pending', 'approved', 'executing', 'outcome_recorded'))"
                }
                LegacyV7PendingConstraint::UpgradedWithoutCheck => {
                    "execution_state text not null default 'pending'"
                }
            };
            sql.push_str(&format!(
                "create table pending_tool_calls(
                    request_id text primary key,
                    thread_id text not null,
                    turn_id text not null,
                    tool_call_id text not null,
                    payload text not null,
                    {state},
                    foreign key(request_id) references approvals(request_id),
                    foreign key(thread_id) references threads(thread_id),
                    foreign key(turn_id) references turns(turn_id)
                );"
            ));
        }
        8 | 9 => sql.push_str(
            "create table pending_tool_calls(
                request_id text primary key,
                thread_id text not null,
                turn_id text not null,
                tool_call_id text not null,
                payload text not null,
                execution_state text not null default 'pending'
                    check(execution_state in ('pending', 'executing')),
                foreign key(request_id) references approvals(request_id),
                foreign key(thread_id) references threads(thread_id),
                foreign key(turn_id) references turns(turn_id)
            );",
        ),
        _ => {}
    }
    if version >= 6 {
        sql.push_str(
            "create unique index turns_thread_sequence_unique
                on turns(thread_id, turn_sequence);
             create unique index items_turn_sequence_unique
                on items(turn_id, item_sequence);",
        );
        if version >= 7 || matches!(history_indexes, LegacyHistoryIndexes::Full) {
            sql.push_str(
                "create index turns_history_lookup
                    on turns(thread_id, status, turn_sequence);
                 create index items_history_lookup
                    on items(turn_id, status, kind, item_sequence);",
            );
        }
    }
    sql
}

fn legacy_v11_schema_sql(suffix: &str) -> String {
    if suffix.is_empty() {
        return V11_SCHEMA_SQL.to_string();
    }
    let mut sql = V11_SCHEMA_SQL.to_string();
    for table in [
        "schema_meta",
        "schema_migrations",
        "approval_decisions",
        "pending_tool_calls",
        "trace_events",
        "artifact_refs",
        "threads",
        "turns",
        "items",
        "approvals",
    ] {
        sql = sql.replace(table, &format!("{table}{suffix}"));
    }
    sql
}

fn canonical_v11_schema_sql(suffix: &str) -> String {
    if suffix.is_empty() {
        return CURRENT_SCHEMA_SQL.to_string();
    }
    let mut sql = CURRENT_SCHEMA_SQL.to_string();
    for table in ["schema_meta", "schema_migrations", "threads", "turns", "items"] {
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

fn table_columns(connection: &Connection, table: &str) -> StoreResult<BTreeSet<String>> {
    let query = format!("pragma table_info({table})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
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

fn migration_number(migration: &str) -> Option<u32> {
    migration
        .get(0..4)
        .and_then(|prefix| prefix.parse::<u32>().ok())
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
    if let Some(version) = schema_meta_version(connection)? {
        return Ok(version);
    }
    let markers = read_migration_markers(connection)?;
    if markers
        .iter()
        .any(|marker| !KNOWN_LEGACY_MIGRATIONS.contains(&marker.as_str()))
    {
        return Err(StoreError::InvalidState(
            "schema contains an unknown migration marker".to_string(),
        ));
    }
    if markers.contains(STABLE_ENUM_TEXT_SCHEMA_MIGRATION) {
        return Err(StoreError::InvalidState(
            "v10 migration marker requires schema_meta version 10".to_string(),
        ));
    }
    if let Some(version) = markers
        .iter()
        .filter_map(|marker| migration_number(marker))
        .max()
    {
        return Ok(version.min(THREAD_POLICY_SCHEMA_VERSION));
    }
    if table_has_column(connection, "threads", "approval_policy")?
        || table_has_column(connection, "threads", "sandbox_mode")?
    {
        return Ok(THREAD_POLICY_SCHEMA_VERSION);
    }
    if table_has_column(connection, "items", "item_sequence")?
        || table_has_column(connection, "turns", "turn_sequence")?
    {
        return Ok(6);
    }
    if table_has_column(connection, "pending_tool_calls", "execution_state")? {
        return Ok(7);
    }
    if table_has_column(connection, "trace_events", "session_id")? {
        return Ok(2);
    }
    Ok(1)
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> StoreResult<bool> {
    Ok(table_columns(connection, table)?.contains(column))
}

fn validate_legacy_markers(connection: &Connection, version: u32) -> StoreResult<()> {
    if version == 0 || version > SCHEMA_VERSION {
        return Err(StoreError::InvalidState(
            "legacy schema version is outside the supported range".to_string(),
        ));
    }
    let mut markers = read_migration_markers(connection)?;
    // The resume migration marker may be present on a database whose legacy table shape is still
    // being converted. Ignore that future marker during the preflight compatibility check; the
    // migration transaction will recreate the canonical v13 marker set atomically.
    if version < SCHEMA_VERSION {
        markers.remove(TURN_RESUME_CHECKPOINT_SCHEMA_MIGRATION);
    }
    if version == SCHEMA_VERSION {
        let expected = EXPECTED_MIGRATIONS
            .iter()
            .map(|migration| (*migration).to_string())
            .collect::<BTreeSet<_>>();
        if markers != expected {
            return Err(StoreError::InvalidState(
                "current schema migration markers are incomplete or unknown".to_string(),
            ));
        }
        return Ok(());
    }
    for marker in &markers {
        if !KNOWN_LEGACY_MIGRATIONS.contains(&marker.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "unknown migration marker {marker}"
            )));
        }
        if version < 10 && marker == STABLE_ENUM_TEXT_SCHEMA_MIGRATION {
            return Err(StoreError::InvalidState(
                "v10 migration marker is present on a legacy schema".to_string(),
            ));
        }
        if migration_number(marker).is_some_and(|number| number > version) {
            return Err(StoreError::InvalidState(format!(
                "migration marker {marker} is ahead of schema version {version}"
            )));
        }
    }
    let has_sidecar_table = table_exists(connection, "active_sidecar_runs")?;
    let has_sidecar_marker = markers.contains(RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION);
    if has_sidecar_table != has_sidecar_marker {
        return Err(StoreError::InvalidState(
            "retired active sidecar schema is incomplete".to_string(),
        ));
    }
    let has_migrations_table = table_exists(connection, "schema_migrations")?;
    if version == 1 {
        if has_migrations_table || !markers.is_empty() {
            return Err(StoreError::InvalidState(
                "v1 schema must not contain migration markers".to_string(),
            ));
        }
    } else {
        if !has_migrations_table {
            return Err(StoreError::InvalidState(format!(
                "schema version {version} is missing schema_migrations"
            )));
        }
        let mut expected = BTreeSet::new();
        for (number, marker) in [
            (1, INITIAL_SCHEMA_MIGRATION),
            (2, DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION),
            (4, PENDING_TOOL_CALL_SCHEMA_MIGRATION),
            (5, STORE_HARDENING_SCHEMA_MIGRATION),
            (6, CONVERSATION_HISTORY_SCHEMA_MIGRATION),
            (7, PENDING_EXECUTION_STATE_SCHEMA_MIGRATION),
            (8, APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION),
            (9, THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION),
            (10, STABLE_ENUM_TEXT_SCHEMA_MIGRATION),
            (11, TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION),
            (12, TYPED_TRACE_SPAN_SCHEMA_MIGRATION),
        ] {
            if number <= version {
                expected.insert(marker.to_string());
            }
        }
        if has_sidecar_marker {
            expected.insert(RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION.to_string());
        }
        if markers != expected {
            return Err(StoreError::InvalidState(format!(
                "schema version {version} migration markers are incomplete or unknown"
            )));
        }
    }
    let has_turn_sequence = table_has_column(connection, "turns", "turn_sequence")?;
    let has_item_sequence = table_has_column(connection, "items", "item_sequence")?;
    let has_item_redacted = table_has_column(connection, "items", "redacted")?;
    if version < 6 && (has_turn_sequence || has_item_sequence || has_item_redacted) {
        return Err(StoreError::InvalidState(
            "conversation history columns exist before their migration marker".to_string(),
        ));
    }
    if version >= 6 && !(has_turn_sequence && has_item_sequence && has_item_redacted) {
        return Err(StoreError::InvalidState(
            "conversation history schema is incomplete".to_string(),
        ));
    }
    let has_sandbox = table_has_column(connection, "threads", "sandbox_mode")?;
    let has_policy = table_has_column(connection, "threads", "approval_policy")?;
    if version < 9 && (has_sandbox || has_policy) {
        return Err(StoreError::InvalidState(
            "thread policy columns exist before their migration marker".to_string(),
        ));
    }
    if version >= 9 && !(has_sandbox && has_policy) {
        return Err(StoreError::InvalidState(
            "thread policy schema is incomplete".to_string(),
        ));
    }
    Ok(())
}

fn require_legacy_tables(connection: &Connection) -> StoreResult<()> {
    for table in ["threads", "turns", "items"] {
        if !table_exists(connection, table)? {
            return Err(StoreError::InvalidState(format!(
                "legacy schema is missing required table {table}"
            )));
        }
    }
    Ok(())
}

fn decode_legacy_enum<T: DbEnum>(value: &str, legacy: bool) -> StoreResult<T> {
    if legacy {
        decode_legacy_db_enum(value)
    } else {
        T::from_db_text(value).ok_or_else(|| unknown_db_enum(T::LABEL, value))
    }
}

fn read_legacy_threads(connection: &Connection, legacy: bool) -> StoreResult<Vec<LegacyThreadRow>> {
    let mut statement = connection.prepare(
        "select thread_id, model, cwd, status from threads order by rowid",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut threads = Vec::new();
    for row in rows {
        let (thread_id, model, cwd, status) = row?;
        threads.push(LegacyThreadRow {
            thread_id,
            model,
            cwd,
            status: decode_legacy_enum::<ThreadStatus>(&status, legacy)?,
        });
    }
    Ok(threads)
}

fn read_legacy_turns(connection: &Connection, legacy: bool) -> StoreResult<Vec<LegacyTurnRow>> {
    let has_sequence = table_has_column(connection, "turns", "turn_sequence")?;
    let query = if has_sequence {
        "select turn_id, thread_id, turn_sequence, status, agent_loop_status from turns order by rowid"
    } else {
        "select turn_id, thread_id, null, status, agent_loop_status from turns order by rowid"
    };
    let mut statement = connection.prepare(query)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut next_by_thread = BTreeMap::<String, i64>::new();
    let mut turns = Vec::new();
    for row in rows {
        let (turn_id, thread_id, sequence, status, agent_loop_status) = row?;
        let turn_sequence = match sequence {
            Some(sequence) => sequence,
            None => {
                let next = next_by_thread.entry(thread_id.clone()).or_insert(0);
                *next = next.checked_add(1).ok_or_else(|| {
                    StoreError::InvalidState("turn sequence overflow".to_string())
                })?;
                *next
            }
        };
        turns.push(LegacyTurnRow {
            turn_id,
            thread_id,
            turn_sequence,
            status: decode_legacy_enum::<TurnStatus>(&status, legacy)?,
            agent_loop_status,
        });
    }
    Ok(turns)
}

fn read_legacy_items(connection: &Connection, legacy: bool) -> StoreResult<Vec<LegacyItemRow>> {
    let has_sequence = table_has_column(connection, "items", "item_sequence")?;
    let has_redacted = table_has_column(connection, "items", "redacted")?;
    let query = match (has_sequence, has_redacted) {
        (true, true) => {
            "select item_id, turn_id, item_sequence, kind, payload, status, redacted from items order by rowid"
        }
        (false, false) => {
            "select item_id, turn_id, null, kind, payload, status, null from items order by rowid"
        }
        _ => {
            return Err(StoreError::InvalidState(
                "conversation item schema is partially migrated".to_string(),
            ));
        }
    };
    let mut statement = connection.prepare(query)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut next_by_turn = BTreeMap::<String, i64>::new();
    let mut items = Vec::new();
    for row in rows {
        let (item_id, turn_id, sequence, kind, payload, status, redacted) = row?;
        let item_sequence = match sequence {
            Some(sequence) => sequence,
            None => {
                let next = next_by_turn.entry(turn_id.clone()).or_insert(0);
                *next = next.checked_add(1).ok_or_else(|| {
                    StoreError::InvalidState("item sequence overflow".to_string())
                })?;
                *next
            }
        };
        let kind = decode_legacy_enum::<ItemKind>(&kind, legacy)?;
        let status = decode_legacy_enum::<ItemStatus>(&status, legacy)?;
        let payload: Value = serde_json::from_str(&payload)?;
        let (payload, detected_redaction) = sanitize_item_payload(&kind, payload)?;
        let redacted = match redacted {
            Some(value) if value == 0 || value == 1 => value != 0,
            Some(_) => {
                return Err(StoreError::InvalidState(
                    "item redaction flag is invalid".to_string(),
                ));
            }
            None => false,
        } || detected_redaction;
        items.push(LegacyItemRow {
            item_id,
            turn_id,
            item_sequence,
            kind,
            payload,
            status,
            redacted,
        });
    }
    Ok(items)
}
fn validate_legacy_sequences(data: &LegacyData, version: u32) -> StoreResult<()> {
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
    if version >= 6 {
        let has_sequence_columns = table_has_column_from_data_marker(version);
        if !has_sequence_columns {
            return Err(StoreError::InvalidState(
                "conversation history sequence marker is inconsistent".to_string(),
            ));
        }
    }
    Ok(())
}

// Version six and later always carry explicit history sequences; kept as a
// named predicate so the migration contract is visible at the validation seam.
const fn table_has_column_from_data_marker(version: u32) -> bool {
    version >= 6
}

fn read_legacy_data(connection: &Connection, version: u32) -> StoreResult<LegacyData> {
    require_legacy_tables(connection)?;
    if version < SCHEMA_VERSION {
        validate_legacy_schema_fingerprint(connection, version)?;
    }
    validate_legacy_markers(connection, version)?;
    fail_closed_on_foreign_key_violations(connection, "legacy preflight")?;
    let legacy = version < SCHEMA_VERSION;
    let threads = read_legacy_threads(connection, legacy)?;
    let turns = read_legacy_turns(connection, legacy)?;
    let items = read_legacy_items(connection, legacy)?;
    let data = LegacyData {
        threads,
        turns,
        items,
    };
    validate_legacy_sequences(&data, version)?;
    Ok(data)
}

fn migrate_legacy_schema(connection: &Connection, version: u32) -> StoreResult<()> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    // This is deliberately the first schema/data write boundary. Enum and
    // schema inputs are validated before any write.
    // Foreign keys remain enabled: replacement rows are inserted parent-first
    // and legacy tables are removed child-first within this transaction.
    let data = read_legacy_data(&transaction, version)?;
    write_v12_tables(&transaction, &data)?;
    // Validate the fully rebuilt schema while the old database is still
    // recoverable by the transaction. A post-commit validation cannot
    // protect source tables from a malformed final schema or row.
    validate_v12_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn write_v12_tables(connection: &Connection, data: &LegacyData) -> StoreResult<()> {
    connection.execute_batch(&canonical_v12_schema_sql("_v12"))?;
    for thread in &data.threads {
        connection.execute(
            "insert into threads_v12(thread_id, model, cwd, status)
         values(?1, ?2, ?3, ?4)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                thread.status.to_db_text(),
            ],
        )?;
    }
    for turn in &data.turns {
        connection.execute(
            "insert into turns_v12(turn_id, thread_id, turn_sequence, status, agent_loop_status)
         values(?1, ?2, ?3, ?4, ?5)",
            params![
                turn.turn_id,
                turn.thread_id,
                turn.turn_sequence,
                turn.status.to_db_text(),
                turn.agent_loop_status,
            ],
        )?;
    }
    for item in &data.items {
        connection.execute(
            "insert into items_v12(item_id, turn_id, item_sequence, kind, payload, status, redacted)
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                item.item_id,
                item.turn_id,
                item.item_sequence,
                item.kind.to_db_text(),
                serde_json::to_string(&item.payload)?,
                item.status.to_db_text(),
                item.redacted,
            ],
        )?;
    }
    for migration in EXPECTED_MIGRATIONS {
        connection.execute(
            "insert into schema_migrations_v12(migration_id) values(?1)",
            params![migration],
        )?;
    }
    connection.execute(
        "insert into schema_meta_v12(schema_version) values(?1)",
        params![SCHEMA_VERSION],
    )?;

    // Existing foreign-key tables are replaced only after all transformed rows
    // have been accepted by the new schema. Legacy trace/artifact/approval rows
    // are deliberately dropped: their product surfaces no longer exist.
    connection.execute_batch(
        r#"
    drop table if exists active_sidecar_runs;
    drop table if exists pending_tool_calls;
    drop table if exists turn_checkpoints;
    drop table if exists tool_executions;
    drop table if exists turn_inputs;
    drop table if exists approval_decisions;
    drop table if exists approvals;
    drop table if exists trace_events;
    drop table if exists items;
    drop table if exists turns;
    drop table if exists threads;
    drop table if exists artifact_refs;
    drop table if exists schema_migrations;
    drop table if exists schema_meta;
    alter table schema_meta_v12 rename to schema_meta;
    alter table schema_migrations_v12 rename to schema_migrations;
    alter table threads_v12 rename to threads;
    alter table turns_v12 rename to turns;
    alter table items_v12 rename to items;
    alter table turn_inputs_v12 rename to turn_inputs;
    "#,
    )?;
    connection.execute_batch(&v12_index_sql())?;
    Ok(())
}

fn validate_v12_schema(connection: &Connection) -> StoreResult<()> {
    validate_v12_structure(connection)?;
    read_legacy_data(connection, SCHEMA_VERSION)?;
    fail_closed_on_foreign_key_violations(connection, "current schema validation")?;
    Ok(())
}

// Validate the immutable v12 interface without scanning or decoding every
// stored row.  Trusted reopen uses this after the owning process initialized
// the database; row payloads remain validated at each read or transaction.
pub(crate) fn validate_v12_structure(connection: &Connection) -> StoreResult<()> {
    if schema_meta_version(connection)? != Some(SCHEMA_VERSION) {
        return Err(StoreError::InvalidState(
            "current schema_meta version is missing or inconsistent".to_string(),
        ));
    }
    validate_legacy_markers(connection, SCHEMA_VERSION)?;
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

fn validate_canonical_v11_fingerprint(connection: &Connection) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(V11_SCHEMA_SQL)?;
    reference.execute_batch(V11_INDEX_SQL)?;
    let expected = schema_fingerprint(&reference)?;
    let actual = schema_fingerprint(connection)?;
    if actual != expected {
        return Err(StoreError::InvalidState(
            "v11 schema fingerprint is not canonical".to_string(),
        ));
    }
    Ok(())
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

fn validate_legacy_schema_fingerprint(connection: &Connection, version: u32) -> StoreResult<()> {
    if version == 11 {
        return validate_canonical_v11_fingerprint(connection);
    }
    if version == 12 {
        let reference = Connection::open_in_memory()?;
        reference.execute_batch(&legacy_reference_schema_sql(
            12,
            false,
            LegacyTraceLayout::SessionBeforePayload,
            LegacyHistoryIndexes::Full,
            LegacyV7PendingConstraint::FreshFourStateCheck,
        ))?;
        if schema_fingerprint(connection)? == schema_fingerprint(&reference)? {
            return Ok(());
        }
        return Err(StoreError::InvalidState(
            "v12 schema fingerprint is not a released legacy contract".to_string(),
        ));
    }
    let actual = schema_fingerprint(connection)?;
    let sidecar_options: &[bool] = match version {
        3 | 4 => &[true],
        5..=9 => &[false, true],
        _ => &[false],
    };
    let trace_options: &[LegacyTraceLayout] = if version >= 2 {
        &[
            LegacyTraceLayout::SessionBeforePayload,
            LegacyTraceLayout::SessionAfterPayload,
        ]
    } else {
        &[LegacyTraceLayout::SessionBeforePayload]
    };
    let history_options: &[LegacyHistoryIndexes] = if version == 6 {
        &[LegacyHistoryIndexes::UniqueOnly, LegacyHistoryIndexes::Full]
    } else {
        &[LegacyHistoryIndexes::Full]
    };
    let pending_options: &[LegacyV7PendingConstraint] = if version == 7 {
        &[
            LegacyV7PendingConstraint::FreshFourStateCheck,
            LegacyV7PendingConstraint::UpgradedWithoutCheck,
        ]
    } else {
        &[LegacyV7PendingConstraint::FreshFourStateCheck]
    };

    for &include_sidecar in sidecar_options {
        for &trace_layout in trace_options {
            for &history_indexes in history_options {
                for &pending_constraint in pending_options {
                    let reference = Connection::open_in_memory()?;
                    reference.execute_batch(&legacy_reference_schema_sql(
                        version,
                        include_sidecar,
                        trace_layout,
                        history_indexes,
                        pending_constraint,
                    ))?;
                    if actual == schema_fingerprint(&reference)? {
                        return Ok(());
                    }
                }
            }
        }
    }
    Err(StoreError::InvalidState(format!(
        "v{version} schema fingerprint is not a released legacy contract"
    )))
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
// Migration-only decoder: accept current plain text or the historical JSON string scalar.
pub(crate) fn decode_legacy_db_enum<T: DbEnum>(value: &str) -> StoreResult<T> {
    if let Some(decoded) = T::from_db_text(value) {
        return Ok(decoded);
    }
    let scalar = serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    scalar
        .as_deref()
        .and_then(T::from_db_text)
        .ok_or_else(|| unknown_db_enum(T::LABEL, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Force SQLITE_FULL after preflight on the same connection. The write
    // transaction must restore every released-v9 object and row.
    #[test]
    fn migration_write_failure_rolls_back_legacy_schema_and_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy-v9.sqlite3");
        let connection = Connection::open(path).expect("open legacy db");
        connection
            .execute_batch(&legacy_reference_schema_sql(
                9,
                false,
                LegacyTraceLayout::SessionBeforePayload,
                LegacyHistoryIndexes::Full,
                LegacyV7PendingConstraint::FreshFourStateCheck,
            ))
            .expect("create v9 schema");
        connection
            .execute("insert into schema_meta(schema_version) values(9)", [])
            .expect("insert schema version");
        for migration in EXPECTED_MIGRATIONS.iter().copied().filter(|migration| {
            !matches!(
                *migration,
                STABLE_ENUM_TEXT_SCHEMA_MIGRATION
                    | TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION
                    | TYPED_TRACE_SPAN_SCHEMA_MIGRATION
            )
        }) {
            connection
                .execute(
                    "insert into schema_migrations(migration_id) values(?1)",
                    [migration],
                )
                .expect("insert migration marker");
        }
        connection
            .execute(
                "insert into threads(
                     thread_id, model, cwd, status, sandbox_mode, approval_policy
                 ) values('thread_fault', null, null, ?1, ?2, ?3)",
                params![
                    serde_json::to_string(&ThreadStatus::Active).expect("thread status"),
                    serde_json::to_string(&serde_json::json!("workspace-write"))
                        .expect("sandbox mode"),
                    serde_json::to_string(&serde_json::json!("on-request")).expect("approval policy"),
                ],
            )
            .expect("insert thread");
        connection
            .execute(
                "insert into turns(
                     turn_id, thread_id, turn_sequence, status, agent_loop_status
                 ) values('turn_fault', 'thread_fault', 1, ?1, 'completed')",
                [serde_json::to_string(&TurnStatus::Completed).expect("turn status")],
            )
            .expect("insert turn");
        let payload = serde_json::to_string(&serde_json::json!([{
            "type": "text",
            "text": "x".repeat(4096),
        }]))
        .expect("large payload");
        for sequence in 1..=256_i64 {
            connection
                .execute(
                    "insert into items(
                         item_id, turn_id, item_sequence, kind, payload, status, redacted
                     ) values(?1, 'turn_fault', ?2, ?3, ?4, ?5, 0)",
                    params![
                        format!("item_fault_{sequence}"),
                        sequence,
                        serde_json::to_string(&ItemKind::UserMessage).expect("item kind"),
                        payload,
                        serde_json::to_string(&ItemStatus::Completed).expect("item status"),
                    ],
                )
                .expect("insert item");
        }
        connection.execute_batch("vacuum;").expect("compact db");
        let page_count: i64 = connection
            .query_row("pragma page_count", [], |row| row.get(0))
            .expect("page count");
        let max_page_count = format!("pragma max_page_count = {page_count}");
        assert_eq!(
            connection
                .query_row(&max_page_count, [], |row| row.get::<_, i64>(0))
                .expect("set page limit"),
            page_count
        );

        let error = migrate_legacy_schema(&connection, 9)
            .expect_err("page limit must fail the schema write");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
        assert_eq!(
            connection
                .query_row("select schema_version from schema_meta", [], |row| {
                    row.get::<_, u32>(0)
                })
                .expect("legacy schema version"),
            9
        );
        assert_eq!(
            connection
                .query_row("select count(*) from items", [], |row| row.get::<_, u32>(0))
                .expect("legacy item count"),
            256
        );
        assert_eq!(
            connection
                .query_row(
                    "select count(*) from sqlite_schema
                     where type = 'table' and name like '%_v11'",
                    [],
                    |row| row.get::<_, u32>(0),
                )
                .expect("temporary table count"),
            0
        );
    }
}
