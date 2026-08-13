//! 验证 SessionStore 的 schema、绑定、恢复、历史与事务不变量。

use schemars::schema_for;
use singularity_protocol::{ItemKind, ThreadStatus, TurnInputDelivery, TurnStatus};
use singularity_store::{
    CommitTurnOutcomeParams, ConversationRole, SessionStore, SessionStoreDescriptor, StoreError,
    TurnOutcomeAuthority,
};
use std::sync::{Arc, Barrier};

// 原子 start：turn 与 user input 在同一事务创建；重复 start 在已有非终态 turn 时被拒绝。
#[test]
fn turn_start_with_input_is_atomic_and_single_owner() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let input = serde_json::json!([{"type": "text", "text": "hello"}]);
    let (turn, item) = store
        .create_turn_with_input(&thread.thread_id, "running", input.clone())
        .expect("started turn");

    assert_eq!(turn.status, TurnStatus::Running);
    assert_eq!(item.kind, ItemKind::UserMessage);
    assert_eq!(
        store
            .get_turn_user_input(&turn.turn_id)
            .expect("user input"),
        input
    );
    // 同一 thread 已有非终态 turn：重复 start 必须拒绝，而不是幂等覆盖。
    assert!(matches!(
        store.create_turn_with_input(&thread.thread_id, "running", input),
        Err(StoreError::WorkspaceHasNonterminalTurn { .. })
    ));
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Running
    );
}

// 终态提交在 SQLite 重开后仍然持久化。
#[test]
fn turn_outcome_persists_across_store_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "hello"}]),
        )
        .expect("started turn");
    drop(store);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("done"),
            },
        )
        .expect("terminal outcome");
    assert_eq!(committed.turn.status, TurnStatus::Completed);
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].content, "hello");
    assert_eq!(history.messages[1].content, "done");
}

#[test]
fn sqlite_store_persists_threads_turns_and_items() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 13);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string(),
            "0004_pending_tool_calls".to_string(),
            "0005_store_hardening".to_string(),
            "0006_conversation_history".to_string(),
            "0007_pending_execution_state".to_string(),
            "0008_approval_execution_recovery".to_string(),
            "0009_thread_policy_snapshot".to_string(),
            "0010_stable_enum_text".to_string(),
            "0011_typed_permission_resources".to_string(),
            "0012_typed_trace_spans".to_string(),
            "0013_turn_resume_checkpoints".to_string()
        ]
    );
    assert_eq!(
        schema_for!(SessionStoreDescriptor)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "SessionStoreDescriptor"
    );

    let thread = store
        .create_thread(Some("gpt-test"), Some("C:/repo"))
        .expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let item = store
        .append_item(
            &turn.turn_id,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "hello"}]),
        )
        .expect("item");

    let connection =
        rusqlite::Connection::open(dir.path().join("sessions.sqlite3")).expect("open sqlite");
    let thread_binding: (String, String, String) = connection
        .query_row(
            "select thread_id, model, cwd from threads where thread_id = ?1",
            [&thread.thread_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("thread binding projection");
    assert_eq!(
        thread_binding,
        (
            thread.thread_id.clone(),
            "gpt-test".to_string(),
            "C:/repo".to_string()
        )
    );

    assert_eq!(item.kind, ItemKind::UserMessage);
}

// 验证 schema 元数据和 SQLite WAL pragma 在新库中正确写入。
#[test]
fn sqlite_store_writes_schema_meta_and_uses_wal_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");

    let store = SessionStore::open(&db_path).expect("open store");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let schema_version: u32 = connection
        .query_row("select schema_version from schema_meta", [], |row| {
            row.get(0)
        })
        .expect("schema version");
    let journal_mode: String = connection
        .query_row("pragma journal_mode", [], |row| row.get(0))
        .expect("journal mode");

    assert_eq!(schema_version, 13);
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
}

// 验证未来 schema 版本会被 fail closed 拒绝打开。
#[test]
fn sqlite_store_rejects_future_schema_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "
            create table schema_meta(schema_version integer not null);
            insert into schema_meta(schema_version) values(999);
            ",
        )
        .expect("future schema");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::UnsupportedSchema {
            found: 999,
            supported: 13
        })
    ));
}

// 重建 released v12 形状（含 approval 三表、threads policy 列、typed trace 表与
// trace 索引/触发器），迁移必须保留 thread/turn/item 行并丢弃旧 trace/approval/checkpoint 表。
#[test]
fn v12_to_v13_migration_drops_legacy_tables_and_preserves_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v12.sqlite3");
    let store = SessionStore::open(&db_path).expect("open v13 store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .append_item(
            &turn.turn_id,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "preserved"}]),
        )
        .expect("item");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open v13 sqlite");
    connection
        .execute_batch(
            r#"
pragma foreign_keys = off;
drop table turn_inputs;
alter table threads add column sandbox_mode text not null default 'workspace-write'
    check(sandbox_mode in ('read-only', 'workspace-write'));
alter table threads add column approval_policy text not null default 'on-request'
    check(approval_policy in ('on-request', 'never'));
create table trace_events(
    event_id text primary key,
    run_id text not null,
    session_id text not null default '',
    payload text not null,
    span_id text check(span_id is null or length(trim(span_id)) > 0),
    parent_span_id text check(parent_span_id is null or length(trim(parent_span_id)) > 0),
    span_kind text
        check(span_kind in ('task', 'turn', 'prompt_assembly', 'provider_attempt', 'tool_call',
                            'policy_decision', 'approval_wait', 'sandbox_execution',
                            'verification', 'final_review') or span_kind is null),
    span_phase text check(span_phase in ('start', 'end') or span_phase is null),
    span_status text check(span_status in ('unset', 'ok', 'error', 'cancelled') or span_status is null),
    duration_ms integer check(duration_ms >= 0 or duration_ms is null),
    time_to_first_token_ms integer
        check(time_to_first_token_ms >= 0 or time_to_first_token_ms is null),
    span_projection text check(span_projection is null or json_valid(span_projection)),
    metric_samples text not null default '[]'
        check(json_valid(metric_samples) and json_type(metric_samples) = 'array'),
    check((span_id is null and parent_span_id is null and span_kind is null
           and span_phase is null and span_status is null and duration_ms is null
           and time_to_first_token_ms is null and span_projection is null)
          or (span_id is not null and span_kind is not null and span_phase is not null)),
    check((span_phase = 'start' and span_status is null and duration_ms is null
           and time_to_first_token_ms is null and metric_samples = '[]')
          or (span_phase = 'end' and span_status is not null and duration_ms is not null)
          or span_phase is null),
    check(time_to_first_token_ms is null or span_kind = 'provider_attempt'),
    check(time_to_first_token_ms is null or duration_ms is null
          or time_to_first_token_ms <= duration_ms),
    check(parent_span_id is null or parent_span_id <> span_id)
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
insert into trace_events(event_id, run_id, session_id, payload)
values('v12_legacy_trace', 'v12_thread', 'v12_turn', '{"safe":"legacy trace"}');
create unique index approval_decisions_request_unique on approval_decisions(request_id);
create index trace_run_lookup on trace_events(run_id, event_id);
create index approvals_pending_lookup on approvals(decision_outcome, request_id);
create index approvals_thread_lookup on approvals(thread_id, decision_outcome, request_id);
create index approvals_turn_lookup on approvals(turn_id, decision_outcome, request_id);
create index pending_tool_calls_turn_state on pending_tool_calls(turn_id, execution_state, request_id);
create unique index trace_span_phase_unique
    on trace_events(run_id, span_id, span_phase)
    where span_id is not null;
create index trace_span_parent_lookup
    on trace_events(run_id, parent_span_id, span_id)
    where parent_span_id is not null;
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
create table turns_v12(
    turn_id text primary key,
    thread_id text not null,
    turn_sequence integer not null check(turn_sequence > 0),
    status text not null
        check(status in ('running', 'completed', 'blocked', 'failed', 'interrupted')),
    agent_loop_status text not null,
    foreign key(thread_id) references threads(thread_id)
);
insert into turns_v12(turn_id, thread_id, turn_sequence, status, agent_loop_status)
select turn_id, thread_id, turn_sequence, status, agent_loop_status from turns;
drop table turns;
alter table turns_v12 rename to turns;
create unique index turns_thread_sequence_unique on turns(thread_id, turn_sequence);
create index turns_history_lookup on turns(thread_id, status, turn_sequence);
create table schema_meta_v12(
    schema_version integer not null check(schema_version = 12)
);
insert into schema_meta_v12(schema_version) values(12);
drop table schema_meta;
alter table schema_meta_v12 rename to schema_meta;
delete from schema_migrations where migration_id = '0013_turn_resume_checkpoints';
pragma foreign_keys = on;
"#,
        )
        .expect("downgrade to released v12 shape");
    drop(connection);

    let migrated = SessionStore::open(&db_path).expect("migrate v12 store");
    assert_eq!(migrated.descriptor().schema_version, 13);
    assert_eq!(
        migrated
            .get_thread(&thread.thread_id)
            .expect("thread")
            .thread_id,
        thread.thread_id
    );
    assert_eq!(
        migrated.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Running
    );
    let connection = rusqlite::Connection::open(&db_path).expect("open migrated sqlite");
    let item_payload: String = connection
        .query_row(
            "select payload from items where turn_id = ?1",
            [&turn.turn_id],
            |row| row.get(0),
        )
        .expect("preserved item");
    assert!(item_payload.contains("preserved"));
    for table in [
        "trace_events",
        "artifact_refs",
        "approvals",
        "approval_decisions",
        "pending_tool_calls",
        "turn_checkpoints",
        "tool_executions",
    ] {
        let exists: bool = connection
            .query_row(
                "select exists(select 1 from sqlite_schema where type = 'table' and name = ?1)",
                [table],
                |row| row.get(0),
            )
            .expect("legacy table lookup");
        assert!(!exists, "legacy table {table} must be dropped by migration");
    }
    let turn_inputs: bool = connection
        .query_row(
            "select exists(select 1 from sqlite_schema where type = 'table' and name = 'turn_inputs')",
            [],
            |row| row.get(0),
        )
        .expect("turn_inputs lookup");
    assert!(turn_inputs, "turn_inputs must exist in migrated schema");
    let has_pause_requested: bool = connection
        .query_row(
            "select exists(
                 select 1 from pragma_table_xinfo('turns')
                 where name = 'pause_requested'
             )",
            [],
            |row| row.get(0),
        )
        .expect("pause_requested lookup");
    assert!(
        has_pause_requested,
        "migrated turns must keep pause_requested"
    );
}

#[test]
fn turn_input_is_idempotent_ordered_at_boundaries() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
        )
        .expect("started turn");
    let first = serde_json::json!([{"type": "text", "text": "first"}]);
    let second = serde_json::json!([{"type": "text", "text": "second"}]);

    store
        .append_turn_input(&turn.turn_id, "input-1", TurnInputDelivery::Steer, &first)
        .expect("first input");
    store
        .append_turn_input(&turn.turn_id, "input-1", TurnInputDelivery::Steer, &first)
        .expect("same idempotent input");
    assert!(matches!(
        store.append_turn_input(&turn.turn_id, "input-1", TurnInputDelivery::Steer, &second,),
        Err(StoreError::InvalidState(_))
    ));
    store
        .append_turn_input(
            &turn.turn_id,
            "input-2",
            TurnInputDelivery::FollowUp,
            &second,
        )
        .expect("second input");

    let steer_boundary = store
        .turn_boundary_state(&turn.turn_id, false)
        .expect("steer boundary");
    assert_eq!(
        steer_boundary
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec!["input-1"]
    );
    let finalization_boundary = store
        .turn_boundary_state(&turn.turn_id, true)
        .expect("finalization boundary");
    assert_eq!(
        finalization_boundary
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec!["input-1", "input-2"]
    );
    let input_ids = finalization_boundary
        .inputs
        .iter()
        .map(|input| input.input_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(input_ids, vec!["input-1".to_string(), "input-2".to_string()]);
    assert!(
        store
            .turn_boundary_state(&turn.turn_id, true)
            .expect("boundary still pending")
            .inputs
            .len()
            == 2
    );
}

#[test]
fn accepted_turn_input_blocks_terminal_commit_and_stays_idempotent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
        )
        .expect("started turn");
    let input = serde_json::json!([{"type": "text", "text": "accepted"}]);
    store
        .append_turn_input(
            &turn.turn_id,
            "accepted-input",
            TurnInputDelivery::Steer,
            &input,
        )
        .expect("accepted input");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("done"),
            },
        ),
        Err(StoreError::TurnBoundaryPending { .. })
    ));

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let counts_before_retry: (u64, u64) = connection
        .query_row(
            "select (select count(*) from items where turn_id = ?1),
                    (select count(*) from turn_inputs where turn_id = ?1)",
            [&turn.turn_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("counts before retry");

    let retried = store
        .append_turn_input(
            &turn.turn_id,
            "accepted-input",
            TurnInputDelivery::Steer,
            &input,
        )
        .expect("same accepted input remains idempotent");
    assert_eq!(retried.turn_id, turn.turn_id);
    assert_eq!(retried.status, TurnStatus::Running);
    assert!(matches!(
        store.append_turn_input(
            &turn.turn_id,
            "accepted-input",
            TurnInputDelivery::Steer,
            &serde_json::json!([{"type": "text", "text": "different"}]),
        ),
        Err(StoreError::InvalidState(message))
            if message == "turn input idempotency key was reused with different content"
    ));

    let counts_after_retry: (u64, u64) = connection
        .query_row(
            "select (select count(*) from items where turn_id = ?1),
                    (select count(*) from turn_inputs where turn_id = ?1)",
            [&turn.turn_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("counts after retry");
    assert_eq!(counts_after_retry, counts_before_retry);
    let delivery_state: String = connection
        .query_row(
            "select delivery_state from turn_inputs where input_id = 'accepted-input'",
            [],
            |row| row.get(0),
        )
        .expect("delivery state");
    assert_eq!(delivery_state, "pending");
    assert!(
        store
            .append_turn_input(
                &turn.turn_id,
                "new-input",
                TurnInputDelivery::Steer,
                &input,
            )
            .is_ok()
    );
}

#[test]
fn pending_input_blocks_terminal_commit_and_pause_remains_resumable() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
        )
        .expect("started turn");
    store
        .append_turn_input(
            &turn.turn_id,
            "follow-up",
            TurnInputDelivery::FollowUp,
            &serde_json::json!([{"type": "text", "text": "more work"}]),
        )
        .expect("follow-up");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
            },
        ),
        Err(StoreError::TurnBoundaryPending { .. })
    ));
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("running turn").status,
        TurnStatus::Running
    );

    store
        .request_turn_pause(&turn.turn_id)
        .expect("pause requested");
    let boundary = store
        .turn_boundary_state(&turn.turn_id, true)
        .expect("pause boundary");
    assert!(boundary.pause_requested);
    assert_eq!(boundary.inputs.len(), 1);
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("running turn").status,
        TurnStatus::Running
    );
    assert!(
        store
            .append_turn_input(
                &turn.turn_id,
                "paused-input",
                TurnInputDelivery::Steer,
                &serde_json::json!([{"type": "text", "text": "after pause"}]),
            )
            .is_ok()
    );
}
#[test]
fn every_supported_legacy_schema_migrates() {
    for schema_version in 1..=9 {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        create_legacy_enum_database(&db_path, schema_version);
        remove_legacy_pending_approval(&db_path, schema_version);

        let store = SessionStore::open(&db_path).expect("migrate legacy schema");
        assert_eq!(store.descriptor().schema_version, 13);
        assert_eq!(
            store.applied_migrations().expect("migration markers"),
            vec![
                "0001_initial_session_store".to_string(),
                "0002_durable_ledger".to_string(),
                "0004_pending_tool_calls".to_string(),
                "0005_store_hardening".to_string(),
                "0006_conversation_history".to_string(),
                "0007_pending_execution_state".to_string(),
                "0008_approval_execution_recovery".to_string(),
                "0009_thread_policy_snapshot".to_string(),
                "0010_stable_enum_text".to_string(),
                "0011_typed_permission_resources".to_string(),
                "0012_typed_trace_spans".to_string(),
                "0013_turn_resume_checkpoints".to_string(),
            ]
        );
        let connection = rusqlite::Connection::open(&db_path).expect("reopen migrated db");
        let status: String = connection
            .query_row(
                "select status from threads where thread_id = 'thread_legacy'",
                [],
                |row| row.get(0),
            )
            .expect("thread status");
        assert_eq!(status, ThreadStatus::Active.as_storage_text());
        let item_kind: String = connection
            .query_row(
                "select kind from items where item_id = 'item_legacy_completed'",
                [],
                |row| row.get(0),
            )
            .expect("item kind");
        assert_eq!(item_kind, ItemKind::UserMessage.as_storage_text());
        for table in [
            "trace_events",
            "approvals",
            "approval_decisions",
            "artifact_refs",
            "pending_tool_calls",
        ] {
            let exists: bool = connection
                .query_row(
                    "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("legacy table lookup");
            assert!(!exists, "legacy table {table} must be dropped by migration");
        }
        assert!(!connection
            .query_row(
                "select exists(select 1 from sqlite_master where type = 'table' and name = 'active_sidecar_runs')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("sidecar absence"));
    }
}

// 任何旧 schema 的 pending checkpoint 都不属于当前 AgentLoop codec；迁移不能伪造状态。
#[test]
fn released_legacy_schema_variants_migrate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let v2_path = dir.path().join("v2-appended-trace.sqlite3");
    create_legacy_enum_database(&v2_path, 2);
    remove_legacy_pending_approval(&v2_path, 2);
    let connection = rusqlite::Connection::open(&v2_path).expect("open v2 db");
    connection
        .execute_batch(
            "create table trace_events_upgraded(
                 event_id text primary key,
                 run_id text not null,
                 payload text not null
             );
             insert into trace_events_upgraded(event_id, run_id, payload)
             select event_id, run_id, payload from trace_events;
             drop table trace_events;
             alter table trace_events_upgraded rename to trace_events;
             alter table trace_events add column session_id text not null default '';
             update trace_events set session_id = run_id;",
        )
        .expect("recreate upgraded v2 trace table");
    drop(connection);
    assert_eq!(
        SessionStore::open(&v2_path)
            .expect("migrate upgraded v2")
            .descriptor()
            .schema_version,
        13
    );

    let v5_path = dir.path().join("v5-retired-sidecar.sqlite3");
    create_legacy_enum_database(&v5_path, 5);
    remove_legacy_pending_approval(&v5_path, 5);
    let connection = rusqlite::Connection::open(&v5_path).expect("open v5 db");
    connection
        .execute_batch(
            "create table active_sidecar_runs(
                 turn_id text primary key,
                 thread_id text not null,
                 run_id text not null,
                 session_id text not null,
                 task_id text not null,
                 status text not null,
                 created_at text not null default current_timestamp,
                 updated_at text not null default current_timestamp
             );
             insert into schema_migrations(migration_id)
             values('0003_active_sidecar_runs');",
        )
        .expect("restore retired v5 sidecar shape");
    drop(connection);
    assert_eq!(
        SessionStore::open(&v5_path)
            .expect("migrate upgraded v5")
            .descriptor()
            .schema_version,
        13
    );

    let v6_path = dir.path().join("v6-initial-indexes.sqlite3");
    create_legacy_enum_database(&v6_path, 6);
    remove_legacy_pending_approval(&v6_path, 6);
    let connection = rusqlite::Connection::open(&v6_path).expect("open v6 db");
    connection
        .execute_batch(
            "drop index turns_history_lookup;
             drop index items_history_lookup;",
        )
        .expect("restore initial v6 index set");
    drop(connection);
    assert_eq!(
        SessionStore::open(&v6_path)
            .expect("migrate initial v6")
            .descriptor()
            .schema_version,
        13
    );

    let v7_path = dir.path().join("v7-appended-state.sqlite3");
    create_legacy_enum_database(&v7_path, 7);
    remove_legacy_pending_approval(&v7_path, 7);
    let connection = rusqlite::Connection::open(&v7_path).expect("open v7 db");
    connection
        .execute_batch(
            "create table pending_tool_calls_upgraded(
                 request_id text primary key,
                 thread_id text not null,
                 turn_id text not null,
                 tool_call_id text not null,
                 payload text not null,
                 execution_state text not null default 'pending',
                 foreign key(request_id) references approvals(request_id),
                 foreign key(thread_id) references threads(thread_id),
                 foreign key(turn_id) references turns(turn_id)
             );
             insert into pending_tool_calls_upgraded(
                 request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) select request_id, thread_id, turn_id, tool_call_id, payload, execution_state
               from pending_tool_calls;
             drop table pending_tool_calls;
             alter table pending_tool_calls_upgraded rename to pending_tool_calls;",
        )
        .expect("restore upgraded v7 pending table");
    drop(connection);
    assert_eq!(
        SessionStore::open(&v7_path)
            .expect("migrate upgraded v7")
            .descriptor()
            .schema_version,
        13
    );
}

// 旧 execution state 不能绕过 current-only checkpoint codec。
#[test]
fn invalid_legacy_enum_is_rejected_without_mutating_any_supported_version() {
    for schema_version in 1..=9 {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        create_legacy_enum_database(&db_path, schema_version);
        let before = sqlite_snapshot(&db_path);
        let connection = rusqlite::Connection::open(&db_path).expect("open legacy db");
        connection
            .execute(
                "update threads set status = ?1 where thread_id = 'thread_legacy'",
                [r#""unknown""#],
            )
            .expect("inject
 unknown enum");
        drop(connection);
        let before_invalid = sqlite_snapshot(&db_path);

        assert!(matches!(
            SessionStore::open(&db_path),
            Err(StoreError::InvalidState(message)) if message.contains("thread status")
        ));
        assert_ne!(before, before_invalid);
        assert_eq!(sqlite_snapshot(&db_path), before_invalid);
        assert!(!has_v11_temporary_tables(&db_path));
    }
}

// 完整 legacy fingerprint 在任何迁移写入前拒绝额外对象。
#[test]
fn unexpected_legacy_object_is_rejected_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_legacy_enum_database(&db_path, 9);
    let connection = rusqlite::Connection::open(&db_path).expect("open legacy db");
    connection
        .execute_batch(
            "create table unexpected_legacy_table(value text);
             insert into unexpected_legacy_table(value) values('must survive rollback');",
        )
        .expect("inject unexpected legacy table");
    drop(connection);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("schema fingerprint is not a released legacy contract")
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
    assert!(!has_v11_temporary_tables(&db_path));
    let connection = rusqlite::Connection::open(&db_path).expect("reopen legacy db");
    let version: u32 = connection
        .query_row("select schema_version from schema_meta", [], |row| {
            row.get(0)
        })
        .expect("legacy schema version");
    assert_eq!(version, 9);
    let value: String = connection
        .query_row("select value from unexpected_legacy_table", [], |row| {
            row.get(0)
        })
        .expect("unexpected table row");
    assert_eq!(value, "must survive rollback");
}
// trusted reopen 仍拒绝已初始化数据库的 marker/结构分裂。
#[test]
fn trusted_reopen_validates_v11_markers_before_serving_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "delete from schema_migrations where migration_id = '0010_stable_enum_text'",
            [],
        )
        .expect("remove marker");
    drop(connection);

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("migration markers")
    ));
}

#[cfg(unix)]
#[test]
fn trusted_reopen_rejects_a_hard_linked_store_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let hard_link = dir.path().join("sessions-alias.sqlite3");
    std::fs::hard_link(&db_path, &hard_link).expect("hard link store");

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("hard links")
    ));
}

#[cfg(unix)]
#[test]
fn trusted_reopen_rejects_path_identity_replacement() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let replacement = dir.path().join("replacement.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let replacement_store = SessionStore::open(&replacement).expect("open replacement");
    drop(replacement_store);
    let original = dir.path().join("original.sqlite3");
    std::fs::rename(&db_path, &original).expect("move original");
    std::fs::rename(&replacement, &db_path).expect("replace store path");

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("identity")
    ));
}

#[cfg(unix)]
#[test]
fn store_open_rejects_final_and_parent_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("temp dir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("real dir");
    let real_db = real_dir.join("sessions.sqlite3");
    drop(SessionStore::open(&real_db).expect("create real store"));

    let file_link = dir.path().join("sessions-link.sqlite3");
    symlink(&real_db, &file_link).expect("file symlink");
    assert!(matches!(
        SessionStore::open(&file_link),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));

    let directory_link = dir.path().join("real-link");
    symlink(&real_dir, &directory_link).expect("directory symlink");
    assert!(matches!(
        SessionStore::open(directory_link.join("sessions.sqlite3")),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));
}

#[cfg(windows)]
#[test]
fn trusted_reopen_keeps_windows_store_path_non_replaceable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let replacement = dir.path().join("replacement.sqlite3");
    let replacement_store = SessionStore::open(&replacement).expect("open replacement");
    drop(replacement_store);

    assert!(std::fs::rename(&db_path, dir.path().join("original.sqlite3")).is_err());
    store
        .trusted_reopen()
        .expect("protected store remains usable");
}

#[cfg(windows)]
#[test]
fn store_open_rejects_parent_reparse_point_when_creation_is_available() {
    use std::os::windows::fs::symlink_dir;

    let dir = tempfile::tempdir().expect("temp dir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("real dir");
    let real_db = real_dir.join("sessions.sqlite3");
    drop(SessionStore::open(&real_db).expect("create real store"));
    let directory_link = dir.path().join("real-link");
    if let Err(error) = symlink_dir(&real_dir, &directory_link) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("create directory symlink: {error}");
    }

    assert!(matches!(
        SessionStore::open(directory_link.join("sessions.sqlite3")),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));
}
// 验证已标记 v11 库缺少 migration marker 时不能被认领。
#[test]
fn v11_missing_migration_marker_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("create current store");
    store
        .create_thread(Some("gpt-test"), Some("C:/repo"))
        .expect("create thread");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "delete from schema_migrations where migration_id = '0010_stable_enum_text';",
        )
        .expect("remove migration marker");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("migration markers")
    ));
}

// 验证 marker 或列只完成一半时，store 不会猜测并继续运行。
#[test]
fn v11_structure_rejects_weak_check_index_and_trigger_without_mutation() {
    for corruption in ["check", "index", "trigger"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("create current store");
        store.create_thread(None, None).expect("thread");
        drop(store);

        let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
        match corruption {
            "check" => {
                let original_sql: String = connection
                    .query_row(
                        "select sql from sqlite_master where type = 'table' and name = 'threads'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("thread schema");
                let weakened_sql = original_sql.replace(
                    "check(status in ('active', 'archived'))",
                    "check(status in ('active', 'archived', 'future'))",
                );
                assert_ne!(weakened_sql, original_sql);
                connection
                    .pragma_update(None, "writable_schema", "on")
                    .expect("enable writable schema");
                connection
                    .execute(
                        "update sqlite_master set sql = ?1 where type = 'table' and name = 'threads'",
                        [&weakened_sql],
                    )
                    .expect("weaken check");
                connection
                    .pragma_update(None, "writable_schema", "off")
                    .expect("disable writable schema");
            }
            "index" => {
                connection
                    .execute("drop index turns_history_lookup", [])
                    .expect("drop canonical index");
            }
            "trigger" => {
                connection
                    .execute_batch(
                        "create trigger unexpected_v11_trigger after insert on threads
                         begin select 1; end;",
                    )
                    .expect("create unexpected trigger");
            }
            _ => unreachable!(),
        }
        drop(connection);
        let before = sqlite_snapshot(&db_path);

        let error = match SessionStore::open(&db_path) {
            Ok(_) => panic!("corrupt v11 accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, StoreError::InvalidState(_)));
        assert_eq!(sqlite_snapshot(&db_path), before);
    }
}

// 验证迁移重建的 schema 保留外键绑定与状态检查。
#[test]
fn migrated_schema_rebuilds_foreign_key_tables() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_legacy_enum_database(&db_path, 4);

    let store = SessionStore::open(&db_path).expect("migrate store");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("reopen sqlite");
    connection
        .execute_batch("pragma foreign_keys = on;")
        .expect("enable foreign keys");
    let turn_parents = foreign_key_parents(&connection, "turns");
    assert!(turn_parents.contains(&"threads".to_string()));
    assert!(connection
        .execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status)
             values('orphan_turn', 'missing_thread', 1, 'running', 'running')",
            [],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into turns(
                 turn_id, thread_id, turn_sequence, status, agent_loop_status
             ) values('bad_turn', 'thread_legacy', 1, 'unknown_status', 'running')",
            [],
        )
        .is_err());
}

// 验证新 schema 拒绝孤儿 thread/turn。
#[test]
fn missing_thread_and_turn_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    assert!(matches!(
        store.create_turn("missing_thread", "running"),
        Err(StoreError::NotFound(message)) if message == "thread missing_thread"
    ));
    assert!(matches!(
        store.append_item("missing_turn", ItemKind::UserMessage, serde_json::json!({})),
        Err(StoreError::NotFound(message)) if message == "turn missing_turn"
    ));
}

// 验证终态 turn 状态与 agent_loop_status 不可被后续更新覆盖。
#[test]
fn terminal_turn_status_is_not_overwritten_by_later_updates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    store
        .update_turn_state(&turn.turn_id, TurnStatus::Completed, "completed")
        .expect("complete turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Completed, "completed")
        .expect("same terminal status is idempotent");

    assert!(matches!(
        store.update_turn_state(&turn.turn_id, TurnStatus::Running, "running"),
        Err(StoreError::InvalidState(message)) if message == "terminal turn status cannot be overwritten"
    ));
    assert!(matches!(
        store.update_turn_state(&turn.turn_id, TurnStatus::Completed, "failed"),
        Err(StoreError::InvalidState(message)) if message == "terminal turn agent_loop_status cannot be overwritten"
    ));
    assert_eq!(
        store
            .get_turn(&turn.turn_id)
            .expect("turn")
            .agent_loop_status,
        "completed"
    );
}
// 验证取消请求与晚到 completion 之间保持原子边界。
#[test]
fn cancellation_request_is_atomic_and_rejects_late_completion() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    let cancel_requested = store
        .request_turn_cancellation(&turn.turn_id)
        .expect("request cancellation");

    assert_eq!(cancel_requested.status, TurnStatus::Running);
    assert_eq!(cancel_requested.agent_loop_status, "cancel_requested");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("too late"),
            },
        ),
        Err(StoreError::InvalidState(message))
            if message == "cancel-requested turn can only finalize as interrupted"
    ));
    let interrupted = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "cancelled",
                assistant_item_id: None,
                assistant_delta: None,
            },
        )
        .expect("finalize cancellation");
    assert_eq!(interrupted.turn.status, TurnStatus::Interrupted);
}

// 验证 monitor 基础设施故障优先于 cancel_requested。
#[test]
fn infrastructure_failure_authority_rejects_non_failed_outcomes() {
    for (status, agent_loop_status) in [
        (TurnStatus::Interrupted, "cancelled"),
        (TurnStatus::Completed, "completed"),
    ] {
        let is_completed = status == TurnStatus::Completed;
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        store
            .request_turn_cancellation(&turn.turn_id)
            .expect("cancel request");

        let assistant_item_id = SessionStore::allocate_assistant_item_id();
        let result = store.commit_turn_outcome_with_authority(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status,
                agent_loop_status,
                assistant_item_id: is_completed.then_some(&assistant_item_id),
                assistant_delta: is_completed.then_some("late result"),
            },
            TurnOutcomeAuthority::InfrastructureFailure,
        );
        assert!(matches!(
            result,
            Err(StoreError::InvalidState(message))
                if message == "infrastructure failure can only finalize as failed"
        ));
        let current = store.get_turn(&turn.turn_id).expect("unchanged turn");
        assert_eq!(current.status, TurnStatus::Running);
        assert_eq!(current.agent_loop_status, "cancel_requested");
    }
}

// 验证 paused/suspended 无 owner turn 被 interrupt 时当场终态化。
#[test]
fn paused_and_suspended_interrupt_terminalizes_without_restart() {
    for (status, agent_loop_status) in [
        (TurnStatus::Paused, "paused"),
        (TurnStatus::Suspended, "suspended"),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        store
            .update_turn_state(&turn.turn_id, status, agent_loop_status)
            .expect("ownerless state");
        let interrupted = store
            .request_turn_cancellation(&turn.turn_id)
            .expect("interrupt");
        assert_eq!(interrupted.status, TurnStatus::Interrupted);
        assert_eq!(interrupted.agent_loop_status, "cancelled");
        // 未重启：当前 Store 会话即可看到终态（同一进程收敛）。
        let persisted = store.get_turn(&turn.turn_id).expect("turn");
        assert_eq!(persisted.status, TurnStatus::Interrupted);
        assert_eq!(persisted.agent_loop_status, "cancelled");
    }
}
// 验证 delete_thread 级联删除绑定行（turn_inputs/items/turns/threads）。
#[test]
fn thread_delete_removes_bound_turn_inputs_items_and_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, item) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "delete me"}]),
        )
        .expect("turn");
    store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "interrupted",
                assistant_item_id: None,
                assistant_delta: None,
            },
        )
        .expect("terminal turn");

    // 删除前放入一条真实绑定的 turn_input 行。
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into turn_inputs(input_id, turn_id, item_id, delivery, delivery_state, consumed_at)
             values(?1, ?2, ?3, 'steer', 'consumed', current_timestamp)",
            rusqlite::params!["input_delete", turn.turn_id, item.item_id],
        )
        .expect("turn input");
    let actual: i64 = connection
        .query_row(
            "select count(*) from turn_inputs where turn_id = ?1",
            [&turn.turn_id],
            |row| row.get(0),
        )
        .expect("turn input fixture count");
    assert_eq!(actual, 1, "turn_inputs fixture");
    drop(connection);

    store
        .delete_thread(&thread.thread_id)
        .expect("delete thread");

    let connection = rusqlite::Connection::open(&db_path).expect("reopen sqlite");
    for table in ["turn_inputs", "items", "turns", "threads"] {
        let count: i64 = connection
            .query_row(&format!("select count(*) from {table}"), [], |row| {
                row.get(0)
            })
            .expect("deleted table count");
        assert_eq!(count, 0, "{table} rows remain after thread deletion");
    }
}

// 读取指定表的外键父表名称，供迁移断言复用。
fn foreign_key_parents(connection: &rusqlite::Connection, table: &str) -> Vec<String> {
    let query = format!("pragma foreign_key_list({table})");
    let mut statement = connection.prepare(&query).expect("foreign key list");
    statement
        .query_map([], |row| row.get::<_, String>(2))
        .expect("query foreign keys")
        .map(|row| row.expect("foreign key row"))
        .collect()
}

// 验证 turn user input 可供 turn/resume 读取。
#[test]
fn turn_user_input_can_be_read_for_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let payload = serde_json::json!([{"type": "text", "text": "resume this turn"}]);
    let (turn, _) = store
        .create_turn_with_input(&thread.thread_id, "blocked", payload.clone())
        .expect("turn");

    assert_eq!(
        store
            .get_turn_user_input(&turn.turn_id)
            .expect("turn user input"),
        payload
    );
    assert!(matches!(
        store.get_turn_user_input("missing_turn"),
        Err(StoreError::NotFound(message)) if message == "turn user input missing_turn"
    ));
}

// 验证终态 turn 与 assistant item 在同一事务提交。
#[test]
fn terminal_turn_state_and_assistant_item_commit_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "user"}]),
        )
        .expect("turn");
    let assistant_item_id = SessionStore::allocate_assistant_item_id();

    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("assistant"),
            },
        )
        .expect("commit terminal outcome");

    assert_eq!(committed.turn.status, TurnStatus::Completed);
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .map(|item| item.item_id.as_str()),
        Some(assistant_item_id.as_str())
    );
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .and_then(|item| item.payload["delta"].as_str()),
        Some("assistant")
    );
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(
        history
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant"]
    );
}

#[test]
fn terminal_assistant_item_id_and_delta_must_be_paired() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    for (assistant_item_id, assistant_delta) in [
        (None, Some("assistant")),
        (Some(SessionStore::allocate_assistant_item_id()), None),
    ] {
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        let error = store
            .commit_turn_outcome(
                &turn.turn_id,
                CommitTurnOutcomeParams {
                    status: TurnStatus::Completed,
                    agent_loop_status: "completed",
                    assistant_item_id: assistant_item_id.as_ref(),
                    assistant_delta,
                },
            )
            .expect_err("unpaired assistant item must fail closed");
        assert!(matches!(
            error,
            StoreError::InvalidState(message)
                if message == "completed turn outcome requires a preallocated item ID and non-empty assistant message"
        ));
        assert_eq!(
            store.get_turn(&turn.turn_id).expect("turn").status,
            TurnStatus::Running
        );
    }
}

#[test]
fn preallocated_assistant_item_id_cannot_be_reused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let first_thread = store.create_thread(None, None).expect("first thread");
    let second_thread = store.create_thread(None, None).expect("second thread");
    let first_turn = store
        .create_turn(&first_thread.thread_id, "running")
        .expect("first turn");
    let second_turn = store
        .create_turn(&second_thread.thread_id, "running")
        .expect("second turn");
    let item_id = SessionStore::allocate_assistant_item_id();
    let commit = |turn: &singularity_protocol::Turn| {
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&item_id),
                assistant_delta: Some("assistant"),
            },
        )
    };

    commit(&first_turn).expect("first commit");
    let error = commit(&second_turn).expect_err("duplicate item ID must fail closed");

    assert!(matches!(
        error,
        StoreError::InvalidState(message)
            if message == "preallocated assistant item ID is already in use"
    ));
    assert_eq!(
        store
            .get_turn(&second_turn.turn_id)
            .expect("second turn")
            .status,
        TurnStatus::Running
    );
}

// 验证 v5 数据库重开时补齐稳定的 turn/item sequence。
#[test]
fn v5_reopen_backfills_stable_explicit_turn_and_item_sequences() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_v5_history_database(&db_path);

    let store = SessionStore::open(&db_path).expect("migrate v5 store");
    assert!(
        store
            .applied_migrations()
            .expect("migrations")
            .contains(&"0006_conversation_history".to_string())
    );
    drop(store);

    let first = read_history_sequences(&db_path);
    assert_eq!(
        first,
        (
            vec![
                ("turn_a_1".to_string(), 1),
                ("turn_a_2".to_string(), 2),
                ("turn_b_1".to_string(), 1),
            ],
            vec![
                ("item_a_1_user".to_string(), 1, false),
                ("item_a_1_agent".to_string(), 2, false),
                ("item_a_2_user".to_string(), 1, true),
                ("item_b_1_user".to_string(), 1, false),
            ],
        )
    );

    let connection = rusqlite::Connection::open(&db_path).expect("open migrated sqlite");
    let migrated_payload: String = connection
        .query_row(
            "select payload from items where item_id = 'item_a_2_user'",
            [],
            |row| row.get(0),
        )
        .expect("migrated payload");
    assert!(!migrated_payload.contains("legacy-secret"));
    assert!(migrated_payload.contains("[redacted sensitive user input]"));
    drop(connection);

    let reopened = SessionStore::open(&db_path).expect("reopen migrated store");
    drop(reopened);
    assert_eq!(read_history_sequences(&db_path), first);
    for path in [
        db_path.clone(),
        std::path::PathBuf::from(format!("{}-wal", db_path.display())),
    ] {
        if path.exists() {
            let bytes = std::fs::read(&path).expect("read sqlite bytes");
            assert!(
                !bytes
                    .windows(b"legacy-secret".len())
                    .any(|window| window == b"legacy-secret"),
                "legacy secret remained in {}",
                path.display()
            );
        }
    }
}
// 验证并发连接会串行化 v5 history migration。
#[test]
fn concurrent_connections_serialize_the_v5_history_migration() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_v5_history_database(&db_path);
    const WORKERS: usize = 8;
    let barrier = Arc::new(Barrier::new(WORKERS));
    let handles = (0..WORKERS)
        .map(|_| {
            let db_path = db_path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                SessionStore::open(db_path)
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let store = handle
            .join()
            .expect("migration worker joins")
            .expect("migration succeeds");
        drop(store);
    }

    let store = SessionStore::open(&db_path).expect("reopen migrated store");
    assert!(
        store
            .applied_migrations()
            .expect("migrations")
            .contains(&"0006_conversation_history".to_string())
    );
}

// 验证已标记 v11 数据库缺少 migration marker 时 fail closed 且不改数据。
#[test]
fn v11_missing_migration_marker_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (_, item) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "safe"}]),
        )
        .expect("turn");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "delete from schema_migrations where migration_id = '0006_conversation_history'",
            [],
        )
        .expect("delete migration marker");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("migration markers")
    ));
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let payload: String = connection
        .query_row(
            "select payload from items where item_id = ?1",
            [&item.item_id],
            |row| row.get(0),
        )
        .expect("read unchanged item");
    assert!(payload.contains("safe"));
}
// 验证已完成 history 可持久化、排序并按 turn 分页。
#[test]
fn completed_history_is_durable_ordered_and_paged_by_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn_1 =
        append_completed_conversation(&store, &thread.thread_id, "user one", "assistant one");
    let turn_2 =
        append_completed_conversation(&store, &thread.thread_id, "user two", "assistant two");
    let turn_3 =
        append_completed_conversation(&store, &thread.thread_id, "user three", "assistant three");
    drop(store);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let full = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("full history");
    assert_eq!(
        full.messages
            .iter()
            .map(|message| (
                message.turn_id.as_str(),
                message.role.clone(),
                message.content.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (turn_1.as_str(), ConversationRole::User, "user one"),
            (
                turn_1.as_str(),
                ConversationRole::Assistant,
                "assistant one"
            ),
            (turn_2.as_str(), ConversationRole::User, "user two"),
            (
                turn_2.as_str(),
                ConversationRole::Assistant,
                "assistant two"
            ),
            (turn_3.as_str(), ConversationRole::User, "user three"),
            (
                turn_3.as_str(),
                ConversationRole::Assistant,
                "assistant three"
            ),
        ]
    );
    assert_eq!(full.next_before_turn_sequence, None);

    let latest = store
        .read_thread_history(&thread.thread_id, None, 2)
        .expect("latest page");
    assert_eq!(
        latest
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            turn_2.as_str(),
            turn_2.as_str(),
            turn_3.as_str(),
            turn_3.as_str()
        ]
    );
    assert_eq!(latest.next_before_turn_sequence, Some(2));

    let earlier = store
        .read_thread_history(&thread.thread_id, latest.next_before_turn_sequence, 2)
        .expect("earlier page");
    assert_eq!(
        earlier
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![turn_1.as_str(), turn_1.as_str()]
    );
    assert_eq!(earlier.next_before_turn_sequence, None);

    let before_third = store
        .read_thread_history_before_turn(&thread.thread_id, &turn_3, 10)
        .expect("history before third turn");
    assert_eq!(
        before_third
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            turn_1.as_str(),
            turn_1.as_str(),
            turn_2.as_str(),
            turn_2.as_str()
        ]
    );
}
// 验证 history 排除非 completed turn 与非 conversation item。
#[test]
fn history_excludes_non_completed_turns_and_non_conversation_items() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let completed =
        append_completed_conversation(&store, &thread.thread_id, "safe user", "safe assistant");

    for status in [
        TurnStatus::Blocked,
        TurnStatus::Failed,
        TurnStatus::Interrupted,
    ] {
        let started = store
            .create_turn_with_input_and_history(
                &thread.thread_id,
                "running",
                serde_json::json!([{"type": "text", "text": "must not replay"}]),
                10,
            )
            .expect("start non-completed turn");
        store
            .append_item(
                &started.turn.turn_id,
                ItemKind::AgentMessage,
                serde_json::json!({"delta": "must not replay"}),
            )
            .expect("assistant item");
        store
            .update_turn_status(&started.turn.turn_id, status.clone())
            .expect("terminal status");
        let history = store
            .read_thread_history(&thread.thread_id, None, 20)
            .expect("history excludes non-completed turn");
        assert_eq!(history.messages.len(), 2);
        if status == TurnStatus::Blocked {
            store
                .update_turn_status(&started.turn.turn_id, TurnStatus::Interrupted)
                .expect("release blocked fixture");
        }
    }

    let incomplete = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "incomplete completed turn"}]),
            10,
        )
        .expect("start incomplete turn");
    store
        .update_turn_status(&incomplete.turn.turn_id, TurnStatus::Completed)
        .expect("mark incomplete turn completed");
    let malformed = append_completed_conversation(
        &store,
        &thread.thread_id,
        "malformed user",
        "malformed assistant",
    );
    store
        .append_item(
            &malformed,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "orphan trailing user"}]),
        )
        .expect("orphan trailing user");

    store
        .append_item(
            &completed,
            ItemKind::Reasoning,
            serde_json::json!({"summary": "private reasoning"}),
        )
        .expect("reasoning");
    store
        .append_item(
            &completed,
            ItemKind::CommandExecution,
            serde_json::json!({"command": "secret tool metadata"}),
        )
        .expect("tool item");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('started_agent_item', ?1, 5, ?2, '{\"delta\":\"not completed\"}', ?3, 0)",
            rusqlite::params![
                completed,
                ItemKind::AgentMessage.as_storage_text(),
                singularity_protocol::ItemStatus::Started.as_storage_text(),
            ],
        )
        .expect("started item");
    drop(connection);

    let history = store
        .read_thread_history(&thread.thread_id, None, 20)
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].content, "safe user");
    assert_eq!(history.messages[1].content, "safe assistant");
}

// 带追加输入（steer/follow-up）的 completed turn 应完整进入下一轮历史。
#[test]
fn completed_turn_with_follow_up_messages_is_included_in_history() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn =
        append_completed_conversation(&store, &thread.thread_id, "user one", "assistant one");
    store
        .append_item(
            &turn,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "follow-up user"}]),
        )
        .expect("follow-up user item");
    store
        .append_item(
            &turn,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": "assistant two"}),
        )
        .expect("second assistant item");

    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            10,
        )
        .expect("start next turn");

    assert_eq!(started.history.messages.len(), 4);
    assert_eq!(
        started
            .history
            .messages
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            ConversationRole::User,
            ConversationRole::Assistant,
            ConversationRole::User,
            ConversationRole::Assistant,
        ]
    );
}
#[test]
fn history_decodes_all_selected_item_status_and_kind_values_before_projection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = append_completed_conversation(&store, &thread.thread_id, "user", "assistant");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch("pragma ignore_check_constraints = on;")
        .expect("ignore checks");
    connection
        .execute(
            "update items set kind = 'unknown_kind' where turn_id = ?1 and kind = 'userMessage'",
            rusqlite::params![turn],
        )
        .expect("tamper item kind");
    drop(connection);

    assert!(matches!(
        store.read_thread_history(&thread.thread_id, None, 10),
        Err(StoreError::InvalidState(message)) if message.contains("item kind")
    ));
}

// 验证 user 与 assistant 文本存储和 history 投影都会脱敏。
#[test]
fn item_storage_and_history_redact_sensitive_user_and_assistant_text() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let raw_user_secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
    let raw_assistant_secret = "Bearer abcdefghijklmnopqrstuvwxyz123456";
    let raw_tool_secret = "token=abcdefghijklmnopqrstuvwxyz123456";
    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": raw_user_secret}]),
            10,
        )
        .expect("start turn");
    store
        .append_item(
            &started.turn.turn_id,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": raw_assistant_secret}),
        )
        .expect("assistant item");
    store
        .append_item(
            &started.turn.turn_id,
            ItemKind::Reasoning,
            serde_json::json!({"token": raw_tool_secret}),
        )
        .expect("sensitive non-conversation item");
    store
        .update_turn_status(&started.turn.turn_id, TurnStatus::Completed)
        .expect("complete turn");

    let stored_input = store
        .get_turn_user_input(&started.turn.turn_id)
        .expect("stored input");
    assert_eq!(
        stored_input,
        serde_json::json!([{"type": "text", "text": "[redacted sensitive user input]"}])
    );
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(
        history.messages[0].content,
        "[redacted sensitive user input]"
    );
    assert!(history.messages[0].redacted);
    assert_eq!(
        history.messages[1].content,
        "[redacted sensitive assistant output]"
    );
    assert!(history.messages[1].redacted);

    drop(store);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let stored_items = connection
        .prepare("select payload, redacted from items order by item_sequence")
        .expect("prepare items")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .expect("query items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect items");
    assert!(stored_items.iter().all(|(_, redacted)| *redacted));
    let serialized = serde_json::to_string(&stored_items).expect("serialize stored items");
    assert!(!serialized.contains(raw_user_secret));
    assert!(!serialized.contains(raw_assistant_secret));
    assert!(!serialized.contains(raw_tool_secret));
    assert_eq!(stored_items[2].0, "{\"redacted\":true}");
}

// 验证 malformed conversation payload 会 fail closed。
#[test]
fn malformed_conversation_payload_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn_id = append_completed_conversation(&store, &thread.thread_id, "user", "assistant");

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "update items set payload = '{\"delta\":42}' where turn_id = ?1 and kind = ?2",
            rusqlite::params![turn_id, ItemKind::AgentMessage.as_storage_text()],
        )
        .expect("tamper payload");

    assert!(matches!(
        store.read_thread_history(&thread.thread_id, None, 10),
        Err(StoreError::InvalidState(_))
    ));
}

// 验证 archived thread 不能启动新 turn。
#[test]
fn archived_thread_cannot_start_a_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    store
        .update_thread_status(&thread.thread_id, ThreadStatus::Archived)
        .expect("archive thread");

    assert!(matches!(
        store.create_turn(&thread.thread_id, "running"),
        Err(StoreError::InvalidState(_))
    ));
    assert!(matches!(
        store.create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "cannot start"}]),
            10,
        ),
        Err(StoreError::InvalidState(_))
    ));
}
// 验证 turn 与 item 的唯一 sequence 索引拒绝重复值。
#[test]
fn turn_and_item_sequence_unique_indexes_reject_duplicates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .append_item(
            &turn.turn_id,
            ItemKind::Reasoning,
            serde_json::json!({"summary": "safe"}),
        )
        .expect("item");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let indexes = connection
        .prepare("select name from sqlite_master where type = 'index' order by name")
        .expect("prepare indexes")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query indexes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect indexes");
    assert!(indexes.contains(&"turns_history_lookup".to_string()));
    assert!(indexes.contains(&"items_history_lookup".to_string()));
    assert!(connection
        .execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values('duplicate_turn', ?1, 1, 'running', 'running')",
            [&thread.thread_id],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('duplicate_item', ?1, 1, 'reasoning', '{}', 'completed', 0)",
            [&turn.turn_id],
        )
        .is_err());
}

// 验证并发连接只允许一个 turn，并分配唯一 item sequence。
#[test]
fn concurrent_connections_admit_one_turn_and_allocate_unique_item_sequences() {
    const WORKERS: usize = 12;

    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let shared_turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("shared turn");
    store
        .update_turn_state(&shared_turn.turn_id, TurnStatus::Completed, "completed")
        .expect("complete shared turn");
    drop(store);

    let stores = (0..WORKERS)
        .map(|_| SessionStore::open(&db_path).expect("open concurrent store"))
        .collect::<Vec<_>>();
    let turn_barrier = Arc::new(Barrier::new(WORKERS));
    let item_barrier = Arc::new(Barrier::new(WORKERS));
    let handles = stores
        .into_iter()
        .enumerate()
        .map(|(worker, store)| {
            let thread_id = thread.thread_id.clone();
            let shared_turn_id = shared_turn.turn_id.clone();
            let turn_barrier = Arc::clone(&turn_barrier);
            let item_barrier = Arc::clone(&item_barrier);
            std::thread::spawn(move || {
                turn_barrier.wait();
                let turn = store.create_turn(&thread_id, "running");
                item_barrier.wait();
                let item = store.append_item(
                    &shared_turn_id,
                    ItemKind::Reasoning,
                    serde_json::json!({"worker": worker}),
                );
                (turn, item)
            })
        })
        .collect::<Vec<_>>();

    let mut admitted_turns = 0;
    for handle in handles {
        let (turn, item) = handle.join().expect("worker joins");
        match turn {
            Ok(_) => admitted_turns += 1,
            Err(StoreError::WorkspaceHasNonterminalTurn { .. }) => {}
            Err(error) => panic!("unexpected concurrent turn error: {error}"),
        }
        item.expect("concurrent item allocation");
    }
    assert_eq!(admitted_turns, 1);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let turn_sequences = connection
        .prepare("select turn_sequence from turns where thread_id = ?1 order by turn_sequence")
        .expect("prepare turns")
        .query_map([&thread.thread_id], |row| row.get::<_, u64>(0))
        .expect("query turns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect turn sequences");
    assert_eq!(turn_sequences, vec![1, 2]);

    let item_sequences = connection
        .prepare("select item_sequence from items where turn_id = ?1 order by item_sequence")
        .expect("prepare items")
        .query_map([&shared_turn.turn_id], |row| row.get::<_, u64>(0))
        .expect("query items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect item sequences");
    assert_eq!(
        item_sequences,
        (1..=u64::try_from(WORKERS).expect("worker count")).collect::<Vec<_>>()
    );
}
// 验证共享 workspace 的执行 guard 串行化并在 owner 丢失后释放。
#[test]
fn workspace_execution_guard_serializes_shared_workspace_and_releases_after_owner_loss() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let workspace = dir.path().join("workspace");
    let other_workspace = dir.path().join("other-workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&other_workspace).expect("other workspace");
    let owner = SessionStore::open(&db_path).expect("owner store");
    let observer = SessionStore::open(&db_path).expect("observer store");
    let thread = owner
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    let same_workspace_thread = owner
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("same workspace thread");
    let other_workspace_thread = owner
        .create_thread(None, Some(&other_workspace.to_string_lossy()))
        .expect("other workspace thread");

    let guard = owner
        .try_begin_workspace_execution(&thread.thread_id)
        .expect("acquire owner guard")
        .expect("owner guard available");
    let running = owner
        .create_turn(&thread.thread_id, "running")
        .expect("start owned turn");
    assert!(
        observer
            .try_begin_workspace_execution(&same_workspace_thread.thread_id)
            .expect("contended guard check")
            .is_none()
    );
    assert!(matches!(
        observer.create_turn(&same_workspace_thread.thread_id, "running"),
        Err(StoreError::WorkspaceHasNonterminalTurn { .. })
    ));
    let other_guard = observer
        .try_begin_workspace_execution(&other_workspace_thread.thread_id)
        .expect("other workspace guard")
        .expect("other workspace remains independent");
    drop(other_guard);

    drop(guard);
    let recovered_guard = observer
        .try_begin_workspace_execution(&same_workspace_thread.thread_id)
        .expect("recover released owner")
        .expect("released guard can be reacquired");
    let recovered = observer.get_turn(&running.turn_id).expect("recovered turn");
    assert_eq!(recovered.status, TurnStatus::Interrupted);
    assert_eq!(recovered.agent_loop_status, "interrupted");
    observer
        .create_turn(&same_workspace_thread.thread_id, "running")
        .expect("new workspace turn after recovery");
    drop(recovered_guard);
}

// 验证 atomic turn start 返回同一边界之前的历史。
#[test]
fn started_turn_returns_prior_history_from_the_same_atomic_start_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let prior_turn =
        append_completed_conversation(&store, &thread.thread_id, "prior user", "prior assistant");

    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            10,
        )
        .expect("start turn");

    assert_eq!(started.history.messages.len(), 2);
    assert!(
        started
            .history
            .messages
            .iter()
            .all(|message| message.turn_id == prior_turn)
    );
    assert_eq!(started.item.turn_id, started.turn.turn_id);
}

// 追加一条 completed conversation，供 history 测试构造数据。
fn append_completed_conversation(
    store: &SessionStore,
    thread_id: &str,
    user: &str,
    assistant: &str,
) -> String {
    let (turn, _) = store
        .create_turn_with_input(
            thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": user}]),
        )
        .expect("start turn");
    store
        .append_item(
            &turn.turn_id,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": assistant}),
        )
        .expect("assistant item");
    store
        .update_turn_status(&turn.turn_id, TurnStatus::Completed)
        .expect("complete turn");
    turn.turn_id
}
// 创建用于 v5 migration 测试的历史数据库。
fn create_v5_history_database(path: &std::path::Path) {
    let connection = rusqlite::Connection::open(path).expect("open v5 sqlite");
    connection
        .execute_batch(
            r#"
            create table schema_meta(schema_version integer not null);
            insert into schema_meta(schema_version) values(5);
            create table schema_migrations(
                migration_id text primary key,
                applied_at text not null default current_timestamp
            );
            insert into schema_migrations(migration_id) values
                ('0001_initial_session_store'),
                ('0002_durable_ledger'),
                ('0004_pending_tool_calls'),
                ('0005_store_hardening');
            create table threads(
                thread_id text primary key,
                model text,
                cwd text,
                status text not null
            );
            create table turns(
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
            );
            create table trace_events(
                event_id text primary key,
                run_id text not null,
                session_id text not null default '',
                payload text not null
            );
            create table approvals(
                request_id text primary key,
                payload text not null,
                decision_outcome text,
                decision_reason text
            );
            create table approval_decisions(
                decision_id text primary key,
                request_id text not null,
                outcome text not null,
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
                redacted integer not null
            );
            create table pending_tool_calls(
                request_id text primary key,
                thread_id text not null,
                turn_id text not null,
                tool_call_id text not null,
                payload text not null,
                foreign key(request_id) references approvals(request_id),
                foreign key(thread_id) references threads(thread_id),
                foreign key(turn_id) references turns(turn_id)
            );
            insert into threads(thread_id, model, cwd, status) values
                ('thread_a', null, null, '"active"'),
                ('thread_b', null, null, '"active"');
            insert into turns(turn_id, thread_id, status, agent_loop_status) values
                ('turn_a_1', 'thread_a', '"completed"', 'completed'),
                ('turn_b_1', 'thread_b', '"completed"', 'completed'),
                ('turn_a_2', 'thread_a', '"completed"', 'completed');
            insert into items(item_id, turn_id, kind, payload, status) values
                ('item_a_1_user', 'turn_a_1', '"userMessage"', '[{"type":"text","text":"a1"}]', '"completed"'),
                ('item_b_1_user', 'turn_b_1', '"userMessage"', '[{"type":"text","text":"b1"}]', '"completed"'),
                ('item_a_1_agent', 'turn_a_1', '"agentMessage"', '{"delta":"a1 reply"}', '"completed"'),
                ('item_a_2_user', 'turn_a_2', '"userMessage"', '[{"type":"text","text":"SINGULARITY_API_KEY=legacy-secret"}]', '"completed"');
            "#,
        )
        .expect("create v5 schema");
}

fn remove_legacy_pending_approval(path: &std::path::Path, schema_version: u32) {
    let connection = rusqlite::Connection::open(path).expect("open legacy database");
    if schema_version >= 4 {
        connection
            .execute(
                "delete from pending_tool_calls where request_id = 'approval_pending'",
                [],
            )
            .expect("remove legacy pending checkpoint");
    }
    connection
        .execute(
            "delete from approvals where request_id = 'approval_pending'",
            [],
        )
        .expect("remove legacy pending approval");
}
// 创建真实历史版本的最小数据库：v1/v2 仍没有 schema_meta，v3/v4
// 还携带已删除的 active_sidecar_runs，v5 以后才使用 schema_meta；当前
// v11 migration 会读取并丢弃这个已移除的运行时 sidecar，而不把它带入现行 schema。
fn create_legacy_enum_database(path: &std::path::Path, schema_version: u32) {
    assert!((1..=9).contains(&schema_version));
    let has_migrations = schema_version >= 2;
    let has_schema_meta = schema_version >= 5;
    let has_history = schema_version >= 6;
    let has_execution_state = schema_version >= 7;
    let has_execution_recovery = schema_version >= 8;
    let has_policy_snapshot = schema_version >= 9;
    let has_trace_session_id = schema_version >= 2;
    let has_approval_decisions = schema_version >= 2;
    let has_artifacts = schema_version >= 2;
    let has_sidecar = (3..=4).contains(&schema_version);
    let has_pending_tool_calls = schema_version >= 4;
    let has_pending_bindings = schema_version >= 5;

    let connection = rusqlite::Connection::open(path).expect("open legacy enum sqlite");
    let mut schema = String::new();
    if has_schema_meta {
        schema.push_str("create table schema_meta(schema_version integer not null);");
    }
    if has_migrations {
        schema.push_str(
            "create table schema_migrations(
                 migration_id text primary key,
                 applied_at text not null default current_timestamp
             );",
        );
    }
    if has_policy_snapshot {
        schema.push_str(
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
        schema.push_str(
            "create table threads(
                 thread_id text primary key,
                 model text,
                 cwd text,
                 status text not null
             );",
        );
    }
    if has_history {
        schema.push_str(&format!(
            "create table turns(
                 turn_id text primary key,
                 thread_id text not null,
                 turn_sequence integer not null check(turn_sequence > 0),
                 status text not null,
                 agent_loop_status text not null{}
             );",
            if schema_version >= 5 {
                ", foreign key(thread_id) references threads(thread_id)"
            } else {
                ""
            },
        ));
    } else {
        schema.push_str(&format!(
            "create table turns(
                 turn_id text primary key,
                 thread_id text not null,
                 status text not null,
                 agent_loop_status text not null{}
             );",
            if schema_version >= 5 {
                ", foreign key(thread_id) references threads(thread_id)"
            } else {
                ""
            },
        ));
    }
    if has_history {
        schema.push_str(&format!(
            "create table items(
                 item_id text primary key,
                 turn_id text not null,
                 item_sequence integer not null check(item_sequence > 0),
                 kind text not null,
                 payload text not null,
                 status text not null,
                 redacted integer not null check(redacted in (0, 1)){}
             );",
            if schema_version >= 5 {
                ", foreign key(turn_id) references turns(turn_id)"
            } else {
                ""
            },
        ));
    } else {
        schema.push_str(&format!(
            "create table items(
                 item_id text primary key,
                 turn_id text not null,
                 kind text not null,
                 payload text not null,
                 status text not null{}
             );",
            if schema_version >= 5 {
                ", foreign key(turn_id) references turns(turn_id)"
            } else {
                ""
            },
        ));
    }
    if has_trace_session_id {
        schema.push_str(
            "create table trace_events(
                 event_id text primary key,
                 run_id text not null,
                 session_id text not null default '',
                 payload text not null
             );",
        );
    } else {
        schema.push_str(
            "create table trace_events(
                 event_id text primary key,
                 run_id text not null,
                 payload text not null
             );",
        );
    }
    schema.push_str(
        "create table approvals(
             request_id text primary key,
             payload text not null,
             decision_outcome text,
             decision_reason text
         );",
    );
    if has_approval_decisions {
        schema.push_str(&format!(
            "create table approval_decisions(
                 decision_id text primary key,
                 request_id text not null,
                 outcome text not null,
                 reason text not null,
                 payload text not null{}
             );",
            if schema_version >= 5 {
                ", foreign key(request_id) references approvals(request_id)"
            } else {
                ""
            },
        ));
    }
    if has_artifacts {
        schema.push_str(
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
    if has_sidecar {
        schema.push_str(
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
    if has_pending_tool_calls {
        if has_pending_bindings {
            schema.push_str(&format!(
                "create table pending_tool_calls(
                     request_id text primary key,
                     thread_id text not null,
                     turn_id text not null,
                     tool_call_id text not null,
                     payload text not null{}{}{}
                 );",
                if has_execution_state {
                    if schema_version == 7 {
                        ", execution_state text not null default 'pending'
                         check(execution_state in ('pending', 'approved', 'executing', 'outcome_recorded'))"
                    } else {
                        ", execution_state text not null default 'pending'"
                    }
                } else {
                    ""
                },
                if has_execution_recovery {
                    if has_execution_state {
                        " check(execution_state in ('pending', 'executing'))"
                    } else {
                        ""
                    }
                } else {
                    ""
                },
                if schema_version >= 5 {
                    ", foreign key(request_id) references approvals(request_id),
                     foreign key(thread_id) references threads(thread_id),
                     foreign key(turn_id) references turns(turn_id)"
                } else {
                    ""
                },
            ));
        } else {
            schema.push_str(
                "create table pending_tool_calls(
                     request_id text primary key,
                     turn_id text not null,
                     payload text not null
                 );",
            );
        }
    }
    connection
        .execute_batch(&schema)
        .expect("create legacy enum schema");
    if has_history {
        connection
            .execute_batch(
                "create unique index turns_thread_sequence_unique
                     on turns(thread_id, turn_sequence);
                 create unique index items_turn_sequence_unique
                     on items(turn_id, item_sequence);
                 create index turns_history_lookup
                     on turns(thread_id, status, turn_sequence);
                 create index items_history_lookup
                     on items(turn_id, status, kind, item_sequence);",
            )
            .expect("create legacy history indexes");
    }

    if has_schema_meta {
        connection
            .execute(
                "insert into schema_meta(schema_version) values(?1)",
                rusqlite::params![schema_version],
            )
            .expect("insert legacy schema version");
    }
    if has_migrations {
        let migrations = [
            (1, "0001_initial_session_store"),
            (2, "0002_durable_ledger"),
            (3, "0003_active_sidecar_runs"),
            (4, "0004_pending_tool_calls"),
            (5, "0005_store_hardening"),
            (6, "0006_conversation_history"),
            (7, "0007_pending_execution_state"),
            (8, "0008_approval_execution_recovery"),
            (9, "0009_thread_policy_snapshot"),
        ];
        for (version, migration) in migrations {
            let is_retired_sidecar_marker = version == 3;
            if schema_version >= version && (!is_retired_sidecar_marker || has_sidecar) {
                connection
                    .execute(
                        "insert into schema_migrations(migration_id) values(?1)",
                        rusqlite::params![migration],
                    )
                    .expect("insert legacy migration marker");
            }
        }
    }
    let active = serde_json::to_string(&ThreadStatus::Active).expect("thread status");
    let completed = serde_json::to_string(&TurnStatus::Completed).expect("turn status");
    let blocked = serde_json::to_string(&TurnStatus::Blocked).expect("turn status");
    let user_kind = serde_json::to_string(&ItemKind::UserMessage).expect("item kind");
    let item_status =
        serde_json::to_string(&singularity_protocol::ItemStatus::Completed).expect("item status");
    let thread_id = "thread_legacy";
    let completed_turn_id = "turn_legacy";
    let pending_turn_id = "turn_pending";
    if has_policy_snapshot {
        connection
            .execute(
                "insert into threads(
                     thread_id, model, cwd, status, sandbox_mode, approval_policy
                 ) values(?1, null, null, ?2, ?3, ?4)",
                rusqlite::params![
                    thread_id,
                    active,
                    serde_json::to_string(&serde_json::json!("workspace-write"))
                        .expect("sandbox mode"),
                    serde_json::to_string(&serde_json::json!("on-request"))
                        .expect("approval policy"),
                ],
            )
            .expect("insert legacy thread");
    } else {
        connection
            .execute(
                "insert into threads(thread_id, model, cwd, status) values(?1, null, null, ?2)",
                rusqlite::params![thread_id, active],
            )
            .expect("insert legacy thread");
    }
    if has_history {
        for (turn_id, sequence, status) in [
            (completed_turn_id, 1_i64, completed.as_str()),
            (pending_turn_id, 2_i64, blocked.as_str()),
        ] {
            connection
                .execute(
                    "insert into turns(
                         turn_id, thread_id, turn_sequence, status, agent_loop_status
                     ) values(?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        turn_id,
                        thread_id,
                        sequence,
                        status,
                        if status == completed {
                            "completed"
                        } else {
                            "blocked"
                        },
                    ],
                )
                .expect("insert legacy turn");
        }
    } else {
        for (turn_id, status, agent_loop_status) in [
            (completed_turn_id, completed.as_str(), "completed"),
            (pending_turn_id, blocked.as_str(), "blocked"),
        ] {
            connection
                .execute(
                    "insert into turns(turn_id, thread_id, status, agent_loop_status)
                     values(?1, ?2, ?3, ?4)",
                    rusqlite::params![turn_id, thread_id, status, agent_loop_status],
                )
                .expect("insert legacy turn");
        }
    }
    let user_payload = serde_json::json!([{"type":"text","text":"legacy input"}]);
    let user_payload = serde_json::to_string(&user_payload).expect("user payload");
    let item_rows = [
        ("item_legacy_completed", completed_turn_id, 1_i64),
        ("item_legacy_pending", pending_turn_id, 1_i64),
    ];
    for (item_id, turn_id, sequence) in item_rows {
        if has_history {
            connection
                .execute(
                    "insert into items(
                         item_id, turn_id, item_sequence, kind, payload, status, redacted
                     ) values(?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    rusqlite::params![
                        item_id,
                        turn_id,
                        sequence,
                        user_kind,
                        user_payload,
                        item_status
                    ],
                )
                .expect("insert legacy item");
        } else {
            connection
                .execute(
                    "insert into items(item_id, turn_id, kind, payload, status)
                     values(?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![item_id, turn_id, user_kind, user_payload, item_status],
                )
                .expect("insert legacy item");
        }
    }
    let thread_trace = legacy_trace_payload("trace_legacy_thread", thread_id, thread_id, None);
    let turn_trace = legacy_trace_payload(
        "trace_legacy_turn_repair",
        thread_id,
        thread_id,
        Some(completed_turn_id),
    );
    if has_trace_session_id {
        connection
            .execute(
                "insert into trace_events(event_id, run_id, session_id, payload)
                 values(?1, ?2, ?3, ?4)",
                rusqlite::params!["trace_legacy_thread", thread_id, thread_id, thread_trace],
            )
            .expect("insert legacy thread trace");
        connection
            .execute(
                "insert into trace_events(event_id, run_id, session_id, payload)
                 values(?1, ?2, ?3, ?4)",
                rusqlite::params!["trace_legacy_turn_repair", thread_id, thread_id, turn_trace],
            )
            .expect("insert legacy turn trace");
    } else {
        for (event_id, payload) in [
            ("trace_legacy_thread", thread_trace),
            ("trace_legacy_turn_repair", turn_trace),
        ] {
            connection
                .execute(
                    "insert into trace_events(event_id, run_id, payload) values(?1, ?2, ?3)",
                    rusqlite::params![event_id, thread_id, payload],
                )
                .expect("insert legacy trace");
        }
    }

    if has_artifacts {
        connection
            .execute(
                "insert into artifact_refs(
                     artifact_id, run_id, item_id, kind, uri, content_digest,
                     summary, metadata, redacted
                 ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                rusqlite::params![
                    "artifact_legacy",
                    thread_id,
                    "item_legacy_completed",
                    "text",
                    "artifact://legacy",
                    "sha256:legacy",
                    "legacy artifact",
                    "{}",
                ],
            )
            .expect("insert legacy artifact");
    }
    if has_sidecar {
        connection
            .execute(
                "insert into active_sidecar_runs(
                     turn_id, thread_id, run_id, session_id, task_id, status
                 ) values(?1, ?2, ?3, ?4, ?5, 'running')",
                rusqlite::params![
                    pending_turn_id,
                    thread_id,
                    thread_id,
                    pending_turn_id,
                    pending_turn_id,
                ],
            )
            .expect("insert legacy sidecar run");
    }
}
// 旧库 trace 行只需在迁移时被读取并丢弃；payload 以可解析 JSON 构造即可。
fn legacy_trace_payload(
    event_id: &str,
    run_id: &str,
    session_id: &str,
    task_id: Option<&str>,
) -> String {
    let mut event = serde_json::json!({
        "event_id": event_id,
        "run_id": run_id,
        "session_id": session_id,
        "timestamp": "2026-01-01T00:00:00Z",
        "source": "legacy",
        "summary": "legacy trace",
        "payload": {"safe": true},
    });
    if let Some(task_id) = task_id {
        event["task_id"] = serde_json::json!(task_id);
    }
    serde_json::to_string(&event).expect("serialize legacy trace")
}

fn sqlite_snapshot(path: &std::path::Path) -> String {
    let connection = rusqlite::Connection::open(path).expect("open sqlite snapshot");
    let schema = connection
        .prepare(
            "select type, name, coalesce(sql, '')
             from sqlite_master where name not like 'sqlite_%'
             order by type, name",
        )
        .expect("prepare sqlite schema snapshot")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query sqlite schema snapshot")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect sqlite schema snapshot");
    let mut snapshot = String::new();
    for (kind, name, sql) in &schema {
        snapshot.push_str(&format!("{kind}:{name}:{sql}\n"));
    }
    for (kind, name, _) in schema {
        if kind != "table" {
            continue;
        }
        let query = format!(
            "select * from \"{}\" order by rowid",
            name.replace('"', "\"\"")
        );
        let mut statement = connection
            .prepare(&query)
            .expect("prepare sqlite row snapshot");
        let column_count = statement.column_count();
        let mut rows = statement.query([]).expect("query sqlite row snapshot");
        while let Some(row) = rows.next().expect("read sqlite row snapshot") {
            snapshot.push_str(&format!("row:{name}:"));
            for column in 0..column_count {
                snapshot.push_str(&format!(
                    "{:?};",
                    row.get_ref(column).expect("snapshot value")
                ));
            }
            snapshot.push('\n');
        }
    }
    snapshot
}

fn has_v11_temporary_tables(path: &std::path::Path) -> bool {
    let connection = rusqlite::Connection::open(path).expect("open sqlite temp-table check");
    connection
        .query_row(
            "select exists(
                 select 1 from sqlite_master
                 where type = 'table' and name like '%_v11'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query sqlite temp-table check")
}

type HistorySequences = (Vec<(String, u64)>, Vec<(String, u64, bool)>);

// 读取历史数据库中的显式 sequence，供迁移前后比较。
fn read_history_sequences(path: &std::path::Path) -> HistorySequences {
    let connection = rusqlite::Connection::open(path).expect("open sqlite");
    let turns = connection
        .prepare("select turn_id, turn_sequence from turns order by thread_id, turn_sequence")
        .expect("prepare turns")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query turns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect turns");
    let items = connection
        .prepare(
            "select item_id, item_sequence, redacted from items order by turn_id, item_sequence",
        )
        .expect("prepare items")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect items");
    (turns, items)
}
// 一次性探针（验证后删除）
#[test]
fn probe_resume_commit_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "paused",
            serde_json::json!([{"type": "text", "text": "resume me"}]),
        )
        .expect("turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Paused, "paused")
        .expect("paused turn");
    let item_id = SessionStore::allocate_assistant_item_id();
    let result = store.commit_turn_outcome_with_authority(
        &turn.turn_id,
        CommitTurnOutcomeParams {
            status: TurnStatus::Completed,
            agent_loop_status: "completed",
            assistant_item_id: Some(&item_id),
            assistant_delta: Some("resumed answer"),
        },
        TurnOutcomeAuthority::AgentLoop,
    );
    match result {
        Ok(committed) => println!("PROBE commit ok: {}", committed.turn.status.as_storage_text()),
        Err(e) => println!("PROBE commit err: {e:?}"),
    }
}

// 验证 paused/suspended 无 owner turn 在 workspace 恢复时保持可恢复（turn/resume 依赖）。
#[test]
fn workspace_recovery_preserves_paused_and_suspended_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let paused_workspace = dir.path().join("paused-workspace");
    let suspended_workspace = dir.path().join("suspended-workspace");
    std::fs::create_dir(&paused_workspace).expect("paused workspace");
    std::fs::create_dir(&suspended_workspace).expect("suspended workspace");
    let store = SessionStore::open(&db_path).expect("store");
    let paused_thread = store
        .create_thread(None, Some(&paused_workspace.to_string_lossy()))
        .expect("paused thread");
    let paused = store
        .create_turn(&paused_thread.thread_id, "running")
        .expect("paused turn");
    store
        .update_turn_state(&paused.turn_id, TurnStatus::Paused, "paused")
        .expect("pause");
    let suspended_thread = store
        .create_thread(None, Some(&suspended_workspace.to_string_lossy()))
        .expect("suspended thread");
    let suspended = store
        .create_turn(&suspended_thread.thread_id, "running")
        .expect("suspended turn");
    store
        .update_turn_state(&suspended.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend");

    store
        .recover_unowned_workspace_executions()
        .expect("recover");

    assert_eq!(
        store.get_turn(&paused.turn_id).expect("paused").status,
        TurnStatus::Paused
    );
    assert_eq!(
        store
            .get_turn(&suspended.turn_id)
            .expect("suspended")
            .status,
        TurnStatus::Suspended
    );
}
