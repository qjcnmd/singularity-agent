//! 验证 SessionStore 的 schema、绑定、恢复、历史、trace 与事务不变量。

use schemars::schema_for;
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionProfileName,
    PermissionResource, ToolId, WorkspaceRelativePath,
};
use singularity_protocol::{ItemKind, ThreadStatus, TraceBindingError, TraceEvent, TurnStatus};
use singularity_store::{
    CommitTurnOutcomeParams, ConversationRole, RegisterArtifactRefParams, SessionStore,
    SessionStoreDescriptor, StoreError,
};
use std::sync::{Arc, Barrier};

fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("valid tool id")
}

fn workspace_resource(value: &str) -> PermissionResource {
    PermissionResource::WorkspacePath(
        WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
    )
}

// 验证 thread、turn、item、trace 与 approval 的基础持久化闭环。
#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 11);
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
            "0011_typed_permission_resources".to_string()
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
    let trace = TraceEvent::new(
        "trace_1",
        "run_1",
        "session_1",
        "app_server",
        "thread started",
    );
    store.append_trace(&trace).expect("trace");
    let approval = ApprovalRequest::new(
        "approval_1",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("write_file"),
    );
    store.create_approval(&approval).expect("approval");
    let decision = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "ok");
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("decision");

    let connection =
        rusqlite::Connection::open(dir.path().join("sessions.sqlite3")).expect("open sqlite");
    let approval_binding: (String, String) = connection
        .query_row(
            "select thread_id, turn_id from approvals where request_id = 'approval_1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("approval binding projection");
    assert_eq!(
        approval_binding,
        (thread.thread_id.clone(), turn.turn_id.clone())
    );

    assert_eq!(item.kind, ItemKind::UserMessage);
    assert_eq!(store.list_trace("run_1").expect("trace list").len(), 1);
    assert_eq!(
        store.show_trace("trace_1").expect("trace show").summary,
        "thread started"
    );
    assert!(store.show_trace("missing").is_err());
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

    assert_eq!(schema_version, 11);
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
            supported: 11
        })
    ));
}

// v10 approval strings are converted only through the released tool-specific contract.
#[test]
fn v10_approval_resources_migrate_to_typed_v11_payloads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("create v11 store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_v10",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_resources([workspace_resource("README.md")]);
    store.create_approval(&request).expect("approval");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "pragma foreign_keys = off;
             create table schema_meta_v10(
                 schema_version integer not null check(schema_version = 10)
             );
             insert into schema_meta_v10(schema_version) values(10);
             drop table schema_meta;
             alter table schema_meta_v10 rename to schema_meta;
             delete from schema_migrations
              where migration_id = '0011_typed_permission_resources';",
        )
        .expect("restore v10 schema metadata");
    let legacy_payload = serde_json::json!({
        "request_id": request.request_id,
        "thread_id": request.thread_id,
        "turn_id": request.turn_id,
        "tool_call_id": null,
        "action": "edit",
        "resources": ["README.md"],
        "reason": ""
    });
    connection
        .execute(
            "update approvals set payload = ?1 where request_id = 'approval_v10'",
            [serde_json::to_string(&legacy_payload).expect("legacy approval")],
        )
        .expect("write v10 payload");
    drop(connection);

    let migrated = SessionStore::open(&db_path).expect("migrate v10 store");
    assert_eq!(migrated.descriptor().schema_version, 11);
    assert_eq!(
        migrated
            .get_pending_approval("approval_v10")
            .expect("typed approval")
            .resources,
        vec![workspace_resource("README.md")]
    );
}

// 验证 thread policy 快照在 create/get/list 和 reopen 路径保持一致。
#[test]
fn thread_policy_snapshot_persists_and_reopens() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let created = store
        .create_thread_with_policy(
            Some("gpt-test"),
            Some("C:/repo"),
            PermissionProfileName::ReadOnly,
            ApprovalPolicy::Never,
        )
        .expect("create thread with policy");
    assert_eq!(created.sandbox_mode, PermissionProfileName::ReadOnly);
    assert_eq!(created.approval_policy, ApprovalPolicy::Never);
    assert_eq!(
        store.get_thread(&created.thread_id).expect("get thread"),
        created
    );
    assert_eq!(
        store.list_threads().expect("list threads"),
        vec![created.clone()]
    );
    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    let restored = reopened
        .get_thread(&created.thread_id)
        .expect("restore thread");
    assert_eq!(restored.sandbox_mode, PermissionProfileName::ReadOnly);
    assert_eq!(restored.approval_policy, ApprovalPolicy::Never);
}

// 验证 v8 threads 在同一 v11 迁移事务中填充安全默认快照。
#[test]
fn v8_threads_migrate_to_policy_snapshot_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_legacy_enum_database(&db_path, 8);

    let migrated = SessionStore::open(&db_path).expect("migrate v8 store");
    assert_eq!(migrated.descriptor().schema_version, 11);
    let thread_id = migrated
        .list_threads()
        .expect("migrated threads")
        .into_iter()
        .next()
        .expect("migrated thread")
        .thread_id;
    let thread = migrated.get_thread(&thread_id).expect("migrated thread");
    assert_eq!(thread.sandbox_mode, PermissionProfileName::WorkspaceWrite);
    assert_eq!(thread.approval_policy, ApprovalPolicy::OnRequest);
    assert!(
        migrated
            .applied_migrations()
            .expect("migrations")
            .iter()
            .any(|migration| migration == "0009_thread_policy_snapshot")
    );
}

// 验证每个受支持的 v1-v9 历史 schema 都在同一 v11 事务中完成转换。
#[test]
fn every_supported_legacy_schema_migrates_with_trace_and_approval_data() {
    for schema_version in 1..=9 {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        create_legacy_enum_database(&db_path, schema_version);

        let store = SessionStore::open(&db_path).expect("migrate legacy schema");
        assert_eq!(store.descriptor().schema_version, 11);
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
        assert_eq!(
            store
                .list_pending_approvals()
                .expect("pending approvals")
                .iter()
                .map(|request| request.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["approval_pending"]
        );
        assert_eq!(
            store
                .get_approval_decision("approval_final_decision")
                .ok()
                .map(|decision| decision.outcome),
            (schema_version >= 2).then_some(ApprovalOutcome::Allow)
        );
        let repaired = store
            .show_trace("trace_legacy_turn_repair")
            .expect("repaired turn trace");
        assert_eq!(repaired.run_id, "thread_legacy");
        assert_eq!(repaired.session_id, "turn_legacy");
        assert_eq!(repaired.task_id.as_deref(), Some("turn_legacy"));
        assert!(!connection
            .query_row(
                "select exists(select 1 from sqlite_master where type = 'table' and name = 'active_sidecar_runs')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("sidecar absence"));
    }
}

// v4-v6 only persisted the selected tool call, not the model/history checkpoint
// required by the current AgentLoop. Migration must not invent resumable state.
#[test]
fn pre_checkpoint_pending_tool_calls_are_rejected_without_mutation() {
    for schema_version in 4..=6 {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        create_legacy_enum_database(&db_path, schema_version);
        let connection = rusqlite::Connection::open(&db_path).expect("open legacy db");
        let payload = serde_json::json!({
            "request_id": "approval_pending",
            "tool_call_id": "call_pending",
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
        });
        if schema_version == 4 {
            connection
                .execute(
                    "insert into pending_tool_calls(request_id, turn_id, payload)
                     values('approval_pending', 'turn_pending', ?1)",
                    [serde_json::to_string(&payload).expect("pending payload")],
                )
                .expect("insert v4 pending call");
        } else {
            connection
                .execute(
                    "insert into pending_tool_calls(
                         request_id, thread_id, turn_id, tool_call_id, payload
                     ) values(
                         'approval_pending', 'thread_legacy', 'turn_pending', 'call_pending', ?1
                     )",
                    [serde_json::to_string(&payload).expect("pending payload")],
                )
                .expect("insert v5-v6 pending call");
        }
        drop(connection);
        let before = sqlite_snapshot(&db_path);

        assert!(matches!(
            SessionStore::open(&db_path),
            Err(StoreError::InvalidState(message))
                if message.contains("cannot be migrated without fabricating an AgentLoop checkpoint")
        ));
        assert_eq!(sqlite_snapshot(&db_path), before);
        assert!(!has_v11_temporary_tables(&db_path));
    }
}

// Released upgrades produced a few schema shapes that differ from a fresh
// database of the same version. Each remains an explicit, bounded contract.
#[test]
fn released_legacy_schema_variants_migrate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let v2_path = dir.path().join("v2-appended-trace.sqlite3");
    create_legacy_enum_database(&v2_path, 2);
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
        11
    );

    let v5_path = dir.path().join("v5-retired-sidecar.sqlite3");
    create_legacy_enum_database(&v5_path, 5);
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
        11
    );

    let v6_path = dir.path().join("v6-initial-indexes.sqlite3");
    create_legacy_enum_database(&v6_path, 6);
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
        11
    );

    let v7_path = dir.path().join("v7-appended-state.sqlite3");
    create_legacy_enum_database(&v7_path, 7);
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
        11
    );
}

// v8 historically collapsed every non-pending v7 handoff state to executing,
// which prevents an unknown external side effect from being replayed.
#[test]
fn v7_non_pending_execution_states_migrate_fail_closed_as_executing() {
    for legacy_state in ["approved", "executing", "outcome_recorded"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join(format!("v7-{legacy_state}.sqlite3"));
        create_legacy_enum_database(&db_path, 7);
        let connection = rusqlite::Connection::open(&db_path).expect("open v7 db");
        let decision = ApprovalDecision::new(
            "approval_pending",
            ApprovalOutcome::Allow,
            "legacy execution handoff",
        );
        connection
            .execute(
                "update approvals
                 set decision_outcome = ?1, decision_reason = ?2
                 where request_id = 'approval_pending'",
                rusqlite::params![
                    serde_json::to_string(&ApprovalOutcome::Allow).expect("allow"),
                    decision.reason,
                ],
            )
            .expect("finalize legacy approval");
        connection
            .execute(
                "insert into approval_decisions(
                     decision_id, request_id, outcome, reason, payload
                 ) values(?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    decision.decision_id,
                    decision.request_id,
                    serde_json::to_string(&decision.outcome).expect("allow"),
                    decision.reason,
                    serde_json::to_string(&decision).expect("decision"),
                ],
            )
            .expect("insert legacy decision");
        connection
            .execute(
                "update pending_tool_calls set execution_state = ?1
                 where request_id = 'approval_pending'",
                [legacy_state],
            )
            .expect("set legacy state");
        drop(connection);

        let store = SessionStore::open(&db_path).expect("migrate v7 state");
        drop(store);
        let connection = rusqlite::Connection::open(&db_path).expect("reopen migrated db");
        let state: String = connection
            .query_row(
                "select execution_state from pending_tool_calls
                 where request_id = 'approval_pending'",
                [],
                |row| row.get(0),
            )
            .expect("migrated execution state");
        assert_eq!(state, "executing");
    }
}

// 验证确定性损坏在任何迁移写入前拒绝，并保持 v1-v9 数据与临时对象不变。
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
            .expect("inject unknown enum");
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

// 验证现行 v11 每次 open 都拒绝 trace 列与 payload 的身份分裂。
#[test]
fn v11_trace_column_payload_mismatch_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent {
        task_id: Some(turn.turn_id.clone()),
        ..TraceEvent::for_turn(
            "trace_v11_column_mismatch",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "test",
            "trace",
        )
    };
    store.append_trace(&trace).expect("trace");
    drop(store);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "update trace_events set session_id = 'wrong_turn'
             where event_id = 'trace_v11_column_mismatch'",
            [],
        )
        .expect("tamper trace column");
    drop(connection);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("session_id column does not match payload")
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
}

// trusted reopen 只验证结构；实际 trace 行仍必须在读取边界拒绝列/payload 分裂。
#[test]
fn trusted_reopen_defers_trace_row_validation_until_read() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent {
        task_id: Some(turn.turn_id.clone()),
        ..TraceEvent::for_turn(
            "trace_trusted_reopen",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "test",
            "trace",
        )
    };
    store.append_trace(&trace).expect("trace");

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "update trace_events set session_id = 'wrong_turn'
             where event_id = 'trace_trusted_reopen'",
            [],
        )
        .expect("tamper trace column");
    drop(connection);

    let reopened = store.trusted_reopen().expect("trusted reopen");
    assert!(matches!(
        reopened.show_trace("trace_trusted_reopen"),
        Err(StoreError::InvalidState(message))
            if message.contains("columns do not match payload")
    ));
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

// trusted reopen 不扫描 approval/checkpoint 全表，但每个读取或决定事务仍 fail closed。
#[test]
fn trusted_reopen_defers_approval_and_checkpoint_validation_until_use() {
    for corruption in ["approval_payload", "decision_payload", "checkpoint"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("open store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        let request = ApprovalRequest::new(
            format!("approval_trusted_{corruption}"),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id("edit"),
        )
        .with_tool_call_id("call_1");
        let checkpoint = serde_json::json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": "call_1",
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        });
        let has_pending_checkpoint = corruption != "approval_payload";
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                has_pending_checkpoint.then_some(checkpoint),
                "approval",
                "approval requested",
            )
            .expect("approval");
        let decision_id = if corruption == "decision_payload" {
            let decision = ApprovalDecision::new(
                request.request_id.clone(),
                ApprovalOutcome::Allow,
                "allowed",
            );
            let decision_id = decision.decision_id.clone();
            store
                .record_approval_decision(&decision, "approval", "approval decision recorded")
                .expect("decision");
            Some(decision_id)
        } else {
            None
        };

        let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
        match corruption {
            "approval_payload" => {
                connection
                    .execute(
                        "update approvals set payload = '{\"request_id\":\"wrong\"}'
                         where request_id = ?1",
                        rusqlite::params![request.request_id],
                    )
                    .expect("tamper approval payload");
            }
            "decision_payload" => {
                connection
                    .execute(
                        "update approval_decisions set payload = '{\"decision_id\":\"wrong\"}'
                         where request_id = ?1",
                        rusqlite::params![request.request_id],
                    )
                    .expect("tamper decision payload");
            }
            "checkpoint" => {
                connection
                    .execute(
                        "update pending_tool_calls set payload = '{}'
                         where request_id = ?1",
                        rusqlite::params![request.request_id],
                    )
                    .expect("tamper checkpoint");
            }
            _ => unreachable!("table-driven corruption case"),
        }
        drop(connection);

        let trusted = store.trusted_reopen().expect("trusted reopen");
        match corruption {
            "approval_payload" => assert!(matches!(
                trusted.list_pending_approvals(),
                Err(StoreError::InvalidState(_))
            )),
            "decision_payload" => assert!(matches!(
                trusted.get_approval_decision(&decision_id.expect("decision id")),
                Err(StoreError::InvalidState(_))
            )),
            "checkpoint" => assert!(matches!(
                trusted.record_approval_decision(
                    &ApprovalDecision::new(
                        request.request_id.clone(),
                        ApprovalOutcome::Allow,
                        "allowed",
                    ),
                    "approval",
                    "approval decision recorded",
                ),
                Err(StoreError::InvalidState(_))
            )),
            _ => unreachable!("table-driven corruption case"),
        }
    }
}

// 验证历史 turn-shaped trace 不能在关联不唯一时被猜测修复。
#[test]
fn ambiguous_legacy_turn_trace_is_rejected_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_legacy_enum_database(&db_path, 9);
    let connection = rusqlite::Connection::open(&db_path).expect("open legacy db");
    let payload = legacy_trace_payload(
        "trace_legacy_turn_repair",
        "thread_legacy",
        "turn_pending",
        Some("turn_legacy"),
    );
    connection
        .execute(
            "update trace_events set session_id = ?1, payload = ?2
             where event_id = 'trace_legacy_turn_repair'",
            rusqlite::params!["turn_pending", payload],
        )
        .expect("inject ambiguous trace");
    drop(connection);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message)) if message.contains("ambiguous turn binding")
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
    assert!(!has_v11_temporary_tables(&db_path));
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
fn partial_thread_policy_migration_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("create current store");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute("alter table threads drop column approval_policy", [])
        .expect("remove one policy column");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("v11 schema fingerprint is not canonical")
    ));
}

// v11 的 check/index/trigger 结构合同被削弱时，open 只读失败且不再追加任何对象或行。
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
                    .execute("drop index approvals_thread_lookup", [])
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

// 验证已经标记完成的 schema 也不会接受未知或伪造的持久化 policy 值。
#[test]
fn invalid_thread_policy_snapshot_value_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_legacy_enum_database(&db_path, 9);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            r#"update threads set sandbox_mode = '"unsupported-mode"'"#,
            [],
        )
        .expect("tamper sandbox mode");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message)) if message.contains("sandbox mode")
    ));
}

// 验证迁移后关键表重新建立完整外键约束。
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
    let pending_parents = foreign_key_parents(&connection, "pending_tool_calls");
    assert!(pending_parents.contains(&"approvals".to_string()));
    assert!(pending_parents.contains(&"threads".to_string()));
    assert!(pending_parents.contains(&"turns".to_string()));
    assert!(connection
        .execute(
            "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload) values('approval_missing', 'thread_missing', 'turn_missing', 'call_2', '{}')",
            [],
        )
        .is_err());
    assert!(
        connection
            .execute(
                "insert into pending_tool_calls(
                 request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) values(
                 'approval_pending', 'thread_legacy', 'turn_pending', 'call_invalid',
                 '{}', 'approved'
             )",
                [],
            )
            .is_err()
    );
}

// 验证新 schema 拒绝孤儿 turn 与 pending tool call。
#[test]
fn fresh_schema_rejects_orphan_turn_and_pending_tool_call_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch("pragma foreign_keys = on;")
        .expect("enable foreign keys");

    assert!(connection
        .execute(
            "insert into turns(turn_id, thread_id, status, agent_loop_status) values('turn_missing', 'thread_missing', 'running', 'running')",
            [],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload) values('approval_missing', 'thread_missing', 'turn_missing', 'call_1', '{}')",
            [],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload, execution_state) values('approval_invalid', 'thread_missing', 'turn_missing', 'call_2', '{}', 'approved')",
            [],
        )
        .is_err());
}

// 验证缺失 thread、turn、trace run 和 artifact 时统一返回 NotFound。
#[test]
fn missing_thread_turn_event_and_artifact_refs_fail_closed() {
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
    assert!(matches!(
        store.list_trace("missing_run"),
        Err(StoreError::NotFound(message)) if message == "trace run missing_run"
    ));
    assert!(matches!(
        store.get_artifact_ref("missing_artifact"),
        Err(StoreError::NotFound(message)) if message == "artifact missing_artifact"
    ));
}

// 验证 pending tool call 的 request_id 必须绑定 approval。
#[test]
fn pending_tool_call_binding_rejects_request_mismatch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("patch"),
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_other",
        "tool_call_id": "call_1",
        "tool_name": "patch",
        "raw_arguments": "{}",
        "resources": []
    });

    assert!(matches!(
        store.create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        ),
        Err(StoreError::InvalidState(message)) if message == "pending tool call request_id must match approval request"
    ));
    assert!(store.list_pending_approvals().expect("pending").is_empty());
}

// 验证 approval 必须显式绑定已有 thread 与 turn。
#[test]
fn approval_creation_requires_explicit_existing_thread_turn_binding() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let request = ApprovalRequest {
        request_id: "approval_1".to_string(),
        thread_id: String::new(),
        turn_id: String::new(),
        tool_call_id: None,
        action: tool_id("edit"),
        resources: Vec::new(),
        reason: String::new(),
    };

    assert!(matches!(
        store.create_approval(&request),
        Err(StoreError::InvalidState(message))
            if message == "approval request must include explicit thread_id and turn_id"
    ));
}

// 验证 approval 不得引用属于另一 thread 的 turn。
#[test]
fn approval_creation_rejects_turn_bound_to_another_thread() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let expected_thread = store.create_thread(None, None).expect("expected thread");
    let other_thread = store.create_thread(None, None).expect("other thread");
    let turn = store
        .create_turn(&other_thread.thread_id, "blocked")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_1",
        expected_thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    );

    assert!(matches!(
        store.create_approval(&request),
        Err(StoreError::InvalidState(message))
            if message == "approval request thread_id must match bound turn"
    ));
}

// 验证 pending tool call 必须提供与 approval 一致的 tool_call_id。
#[test]
fn pending_tool_call_binding_requires_request_tool_call_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("patch"),
    );
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_turn_call_1",
        "thread_id": &thread.thread_id,
        "turn_id": &turn.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "patch",
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });

    assert!(matches!(
        store.create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        ),
        Err(StoreError::InvalidState(message))
            if message == "pending tool call tool_call_id must match approval request"
    ));
}

// 验证缺少 checkpoint 的 pending approval 会整体回滚。
#[test]
fn pending_tool_call_requires_checkpoint_and_rolls_back_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_turn_call_1",
        "thread_id": &thread.thread_id,
        "turn_id": &turn.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": []
    });

    assert!(matches!(
        store.create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        ),
        Err(StoreError::InvalidState(message))
            if message == "pending approval must include an internal AgentLoop checkpoint"
    ));
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    assert!(matches!(
        store.list_trace(&thread.thread_id),
        Err(StoreError::NotFound(message))
            if message == format!("trace run {}", thread.thread_id)
    ));
}

// 验证创建 pending approval 不会覆盖已请求取消的 turn。
#[test]
fn pending_approval_creation_does_not_overwrite_cancel_requested_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent {
        payload: serde_json::json!({"turn_id": &turn.turn_id, "agent_loop_status": "cancel_requested"}),
        ..TraceEvent::for_turn(
            "trace_cancel_before_approval",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "app_server",
            "turn interrupt requested",
        )
    };
    store
        .request_turn_cancellation(&turn.turn_id, &trace)
        .expect("request cancellation");
    let request = ApprovalRequest::new(
        "approval_after_cancel",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let checkpoint = serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    assert!(matches!(
        store.create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        ),
        Err(StoreError::InvalidState(message))
            if message == "pending approval requires a running or blocked turn"
    ));
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    let cancelled = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(cancelled.status, TurnStatus::Running);
    assert_eq!(cancelled.agent_loop_status, "cancel_requested");
}

// 验证 pending tool call 必须绑定已存在的 turn。
#[test]
fn pending_tool_call_binding_requires_existing_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let request = ApprovalRequest::new(
        "approval_missing_turn_call_1",
        "missing_thread",
        "missing_turn",
        tool_id("patch"),
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_missing_turn_call_1",
        "tool_call_id": "call_1",
        "tool_name": "patch",
        "raw_arguments": "{}",
        "resources": []
    });

    assert!(matches!(
        store.create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        ),
        Err(StoreError::NotFound(message)) if message == "turn missing_turn"
    ));
}

// 验证 approval 决定拒绝跨 turn 的 pending tool call 绑定。
#[test]
fn approval_decision_rejects_pending_tool_call_turn_mismatch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let expected_turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("expected turn");
    store
        .update_turn_state(
            &expected_turn.turn_id,
            TurnStatus::Interrupted,
            "interrupted",
        )
        .expect("finish expected turn fixture");
    let other_turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("other turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        expected_turn.turn_id.clone(),
        tool_id("patch"),
    )
    .with_tool_call_id("call_1");
    store.create_approval(&request).expect("approval");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch("pragma foreign_keys = on;")
        .expect("enable foreign keys");
    connection
        .execute(
            "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload) values(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                request.request_id.as_str(),
                thread.thread_id.as_str(),
                other_turn.turn_id.as_str(),
                "call_1",
                serde_json::json!({
                    "request_id": request.request_id.as_str(),
                    "tool_call_id": "call_1",
                    "tool_name": "patch",
                    "raw_arguments": "{}",
                    "resources": []
                })
                .to_string()
            ],
        )
        .expect("corrupt pending row");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );

    assert!(matches!(
        store.record_approval_decision(&decision, "approval", "approval decision recorded"),
        Err(StoreError::InvalidState(message)) if message == "pending tool call turn_id must match approval request"
    ));
}

// 验证终态 turn 不会被后续状态更新覆盖。
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
    let trace = TraceEvent::for_turn(
        "trace_cancel_requested",
        &thread.thread_id,
        &turn.turn_id,
        "app_server",
        "turn interrupt requested",
    );

    let cancel_requested = store
        .request_turn_cancellation(&turn.turn_id, &trace)
        .expect("request cancellation");

    assert_eq!(cancel_requested.status, TurnStatus::Running);
    assert_eq!(cancel_requested.agent_loop_status, "cancel_requested");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_delta: Some("too late"),
                plan: None,
                trace: &TraceEvent::for_turn(
                    "trace_too_late",
                    &thread.thread_id,
                    &turn.turn_id,
                    "agent_loop",
                    "late completion",
                ),
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
                assistant_delta: None,
                plan: None,
                trace: &TraceEvent::for_turn(
                    "trace_cancelled",
                    &thread.thread_id,
                    &turn.turn_id,
                    "agent_loop",
                    "turn cancelled",
                ),
            },
        )
        .expect("finalize cancellation");
    assert_eq!(interrupted.turn.status, TurnStatus::Interrupted);
    let trace_ids = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .map(|trace| trace.event_id)
        .collect::<Vec<_>>();
    assert_eq!(trace_ids, vec!["trace_cancel_requested", "trace_cancelled"]);
}

// 验证 approval 决定只写入一次且保留在 decision history。
#[test]
fn approval_decision_is_written_once_and_kept_in_decision_history() {
    for outcome in [
        ApprovalOutcome::Allow,
        ApprovalOutcome::Deny,
        ApprovalOutcome::Defer,
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "blocked")
            .expect("turn");
        let request = ApprovalRequest::new(
            "approval_1",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id("write_file"),
        );
        store.create_approval(&request).expect("approval");
        let decision = ApprovalDecision::new("approval_1", outcome, "operator decision");

        let recorded = store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("decision");
        let trace = recorded.trace;

        assert_eq!(recorded.request, request);
        assert_eq!(recorded.decision, decision);
        assert_eq!(trace.run_id, thread.thread_id);
        assert_eq!(trace.session_id, turn.turn_id);
        assert_eq!(trace.task_id.as_deref(), Some(turn.turn_id.as_str()));
        assert_eq!(trace.payload["request_id"], "approval_1");
        assert_eq!(trace.payload["decision_id"], decision.decision_id);
        assert_eq!(
            trace.payload["outcome"],
            serde_json::to_value(outcome).expect("serialize outcome")
        );
        assert_eq!(
            store.list_trace(&thread.thread_id).expect("trace list")[0].event_id,
            trace.event_id
        );
        if outcome == ApprovalOutcome::Defer {
            assert!(matches!(
                store.get_approval_decision(&decision.decision_id),
                Err(StoreError::NotFound(_))
            ));
            assert_eq!(
                store.list_pending_approvals().expect("pending"),
                vec![request]
            );
            store
                .record_approval_decision(&decision, "approval", "approval deferred")
                .expect("repeated defer");
        } else {
            assert_eq!(
                store
                    .get_approval_decision(&decision.decision_id)
                    .expect("decision history")
                    .outcome,
                outcome
            );
            assert!(store.list_pending_approvals().expect("pending").is_empty());
            assert!(matches!(
                store.record_approval_decision(
                    &decision,
                    "approval",
                    "approval decision recorded"
                ),
                Err(StoreError::NotFound(message)) if message == "approval approval_1"
            ));
        }
    }
}

// 验证进程恢复会中断 executing approval，且不会重放外部副作用。
#[test]
fn executing_approval_is_interrupted_on_process_recovery_without_replay() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Blocked, "blocked")
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        "approval_recovery",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let checkpoint = serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": &request.resources,
        "checkpoint_version": 1,
        "messages": [{"role":"assistant","content":[],"tool_calls":[{"tool_call_id":"call_1","tool_name":"edit","arguments":{},"raw_arguments":"{}","parse_status":"valid","validation_errors":[]}]}],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {},
        "last_completion_error": null
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("approval checkpoint");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("claim execution");
    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    reopened
        .recover_unowned_workspace_executions()
        .expect("recover executing approval");

    let recovered_turn = reopened.get_turn(&turn.turn_id).expect("recovered turn");
    assert_eq!(recovered_turn.status, TurnStatus::Interrupted);
    assert_eq!(recovered_turn.agent_loop_status, "interrupted");
    assert!(
        !reopened
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
    let recovery_trace = reopened
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|trace| trace.event_id == "trace_approval_recovery_recovered")
        .expect("recovery trace");
    assert_eq!(recovery_trace.payload["tool_replayed"], false);
    assert_eq!(
        recovery_trace.payload["recovery_reason"],
        "approval_execution_outcome_unknown"
    );
}

// 验证旧 handoff 半完成时恢复 pending successor 而不丢失执行上下文。
#[test]
fn process_recovery_preserves_pending_successor_after_legacy_half_handoff() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let checkpoint = |request_id: &str, tool_call_id: &str| {
        serde_json::json!({
            "request_id": request_id,
            "thread_id": &thread.thread_id,
            "turn_id": &turn.turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [{"role":"assistant","content":[],"tool_calls":[{"tool_call_id":tool_call_id,"tool_name":"edit","arguments":{},"raw_arguments":"{}","parse_status":"valid","validation_errors":[]}]}],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        })
    };
    let first = ApprovalRequest::new(
        "approval_legacy_executing",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &first,
            Some(checkpoint(&first.request_id, "call_1")),
            "approval",
            "approval requested",
        )
        .expect("first approval");
    let first_decision =
        ApprovalDecision::new(first.request_id.clone(), ApprovalOutcome::Allow, "allowed");
    store
        .record_approval_decision(&first_decision, "approval", "approval decision recorded")
        .expect("claim first execution");

    let next = ApprovalRequest::new(
        "approval_pending_successor",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_2");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &next,
            Some(checkpoint(&next.request_id, "call_2")),
            "approval",
            "approval requested",
        )
        .expect("pending successor");
    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    reopened
        .recover_unowned_workspace_executions()
        .expect("recover half handoff");
    assert!(
        !reopened
            .has_pending_tool_call(&first.request_id)
            .expect("first")
    );
    assert!(
        reopened
            .has_pending_tool_call(&next.request_id)
            .expect("next")
    );
    assert_eq!(
        reopened
            .get_pending_approval(&next.request_id)
            .expect("pending successor"),
        next
    );
    assert_eq!(
        reopened
            .get_approval_decision(&first_decision.decision_id)
            .expect("allow decision"),
        first_decision
    );
    let recovered_turn = reopened.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(recovered_turn.status, TurnStatus::Blocked);
    assert_eq!(recovered_turn.agent_loop_status, "blocked");
    let recovery_trace = reopened
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|trace| trace.event_id == "trace_approval_legacy_executing_recovered")
        .expect("half handoff recovery trace");
    assert_eq!(recovery_trace.payload["tool_replayed"], false);
    assert_eq!(
        recovery_trace.payload["recovery_reason"],
        "approval_execution_superseded_by_pending_handoff"
    );

    reopened
        .recover_unowned_workspace_executions()
        .expect("idempotent recovery");
    assert_eq!(
        reopened
            .list_trace(&thread.thread_id)
            .expect("trace list")
            .into_iter()
            .filter(|trace| trace.event_id == "trace_approval_legacy_executing_recovered")
            .count(),
        1
    );
}

// 验证 v11 open 会在恢复前拒绝不一致的 approval/checkpoint 绑定且不改库。
#[test]
fn v11_open_rejects_inconsistent_approval_checkpoint_before_recovery() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let other_thread = store.create_thread(None, None).expect("other thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let checkpoint = |request_id: &str, tool_call_id: &str| {
        serde_json::json!({
            "request_id": request_id,
            "thread_id": &thread.thread_id,
            "turn_id": &turn.turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        })
    };
    let first = ApprovalRequest::new(
        "approval_corrupt_first",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &first,
            Some(checkpoint(&first.request_id, "call_1")),
            "approval",
            "approval requested",
        )
        .expect("first approval");
    store
        .record_approval_decision(
            &ApprovalDecision::new(first.request_id.clone(), ApprovalOutcome::Allow, "allowed"),
            "approval",
            "approval decision recorded",
        )
        .expect("claim first execution");
    let next = ApprovalRequest::new(
        "approval_corrupt_next",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_2");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &next,
            Some(checkpoint(&next.request_id, "call_2")),
            "approval",
            "approval requested",
        )
        .expect("pending successor");
    drop(store);

    let orphan_request_id = "approval_orphan_pending";
    let orphan_request = ApprovalRequest::new(
        orphan_request_id,
        other_thread.thread_id,
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_orphan");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into approvals(request_id, thread_id, turn_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                orphan_request_id,
                thread.thread_id,
                turn.turn_id,
                serde_json::to_string(&orphan_request).expect("orphan approval payload")
            ],
        )
        .expect("insert mismatched approval fixture");
    connection
        .execute(
            "insert into pending_tool_calls(
                request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) values(?1, ?2, ?3, ?4, '{}', 'pending')",
            rusqlite::params![
                orphan_request_id,
                thread.thread_id,
                turn.turn_id,
                "call_orphan"
            ],
        )
        .expect("insert orphan pending execution");
    drop(connection);

    let before = sqlite_snapshot(&db_path);
    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(_))
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
    assert!(!has_v11_temporary_tables(&db_path));
}

// 验证 v11 open 会在恢复前拒绝缺失或游离 decision history，并保持数据库不变。
#[test]
fn v11_open_rejects_missing_or_stray_decision_history_without_mutation() {
    for corruption in ["missing", "stray"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("open store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        let request = ApprovalRequest::new(
            format!("approval_history_{corruption}"),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id("edit"),
        )
        .with_tool_call_id("call_1");
        let checkpoint = serde_json::json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": "call_1",
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        });
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(checkpoint),
                "approval",
                "approval requested",
            )
            .expect("approval");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        if corruption == "missing" {
            store
                .record_approval_decision(&decision, "approval", "approval decision recorded")
                .expect("claim execution");
        }
        drop(store);

        let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
        if corruption == "missing" {
            connection
                .execute(
                    "delete from approval_decisions where request_id = ?1",
                    rusqlite::params![request.request_id],
                )
                .expect("remove decision history");
        } else {
            connection
                .execute(
                    "insert into approval_decisions(decision_id, request_id, outcome, reason, payload)
                     values(?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        decision.decision_id,
                        request.request_id,
                        ApprovalOutcome::Allow.as_storage_text(),
                        decision.reason,
                        serde_json::to_string(&decision).expect("decision"),
                    ],
                )
                .expect("insert stray decision history");
        }
        drop(connection);

        let before = sqlite_snapshot(&db_path);
        assert!(matches!(
            SessionStore::open(&db_path),
            Err(StoreError::InvalidState(_))
        ));
        assert_eq!(sqlite_snapshot(&db_path), before);
        assert!(!has_v11_temporary_tables(&db_path));
    }
}

// 验证未带 checkpoint 的 unresolved tool approval 无法恢复。
#[test]
fn process_recovery_rejects_unresolved_tool_approval_without_checkpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Blocked, "blocked")
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        "approval_missing_checkpoint",
        thread.thread_id,
        turn.turn_id,
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    store.create_approval(&request).expect("approval history");

    assert!(matches!(
        store.recover_unowned_workspace_executions(),
        Err(StoreError::InvalidState(message))
            if message == "approval approval_missing_checkpoint has inconsistent checkpoint state"
    ));
}

// 验证 approval execution handoff 原子替换旧 checkpoint 与新 approval。
#[test]
fn approval_execution_handoff_atomically_replaces_old_checkpoint_with_next_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let first = ApprovalRequest::new(
        "approval_first",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let checkpoint = |request_id: &str, tool_call_id: &str| {
        serde_json::json!({
            "request_id": request_id,
            "thread_id": &thread.thread_id,
            "turn_id": &turn.turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        })
    };
    store
        .create_approval_with_pending_tool_call_and_trace(
            &first,
            Some(checkpoint("approval_first", "call_1")),
            "approval",
            "approval requested",
        )
        .expect("first checkpoint");
    store
        .record_approval_decision(
            &ApprovalDecision::new(first.request_id.clone(), ApprovalOutcome::Allow, "allow"),
            "approval",
            "approval decision recorded",
        )
        .expect("claim first execution");
    let next = ApprovalRequest::new(
        "approval_next",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_2");
    let trace = TraceEvent::for_turn(
        "trace_handoff",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        "agent_loop",
        "agent loop blocked",
    );

    assert!(matches!(
        store.commit_turn_outcome_and_resolve_pending_execution(
            &first.request_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "interrupted",
                assistant_delta: None,
                plan: None,
                trace: &trace,
            },
            &[(next.clone(), checkpoint("approval_next", "call_2"))],
        ),
        Err(StoreError::InvalidState(message))
            if message == "next approval handoff requires a blocked turn outcome"
    ));
    assert!(
        store
            .has_pending_tool_call(&first.request_id)
            .expect("first remains executing")
    );
    assert!(matches!(
        store.get_pending_approval(&next.request_id),
        Err(StoreError::NotFound(_))
    ));

    store
        .commit_turn_outcome_and_resolve_pending_execution(
            &first.request_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Blocked,
                agent_loop_status: "blocked",
                assistant_delta: None,
                plan: None,
                trace: &trace,
            },
            &[(next.clone(), checkpoint("approval_next", "call_2"))],
        )
        .expect("atomic approval handoff");

    assert!(
        !store
            .has_pending_tool_call(&first.request_id)
            .expect("first")
    );
    assert!(store.has_pending_tool_call(&next.request_id).expect("next"));
    assert_eq!(
        store
            .get_pending_approval(&next.request_id)
            .expect("next approval"),
        next
    );
    let blocked = store.get_turn(&turn.turn_id).expect("blocked turn");
    assert_eq!(blocked.status, TurnStatus::Blocked);
    assert_eq!(blocked.agent_loop_status, "blocked");
}

// 验证 deny with checkpoint 会终止 turn 并移除 checkpoint。
#[test]
fn deny_with_checkpoint_atomically_terminalizes_turn_and_removes_checkpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_deny",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let checkpoint = serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("approval checkpoint");
    let decision =
        ApprovalDecision::new(request.request_id.clone(), ApprovalOutcome::Deny, "denied");

    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("deny approval");

    let denied_turn = store.get_turn(&turn.turn_id).expect("denied turn");
    assert_eq!(denied_turn.status, TurnStatus::Failed);
    assert_eq!(denied_turn.agent_loop_status, "failed");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
}

// 验证 allow claim 在 store 事务内重新检查 active thread。
#[test]
fn allow_claim_rechecks_active_thread_inside_the_store_transaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_archive_race",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let checkpoint = serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("approval");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "update threads set status = ?1 where thread_id = ?2",
            rusqlite::params![ThreadStatus::Archived.as_storage_text(), thread.thread_id],
        )
        .expect("simulate external archive race");

    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    assert!(matches!(
        store.record_approval_decision(&decision, "approval", "approval decision recorded"),
        Err(StoreError::InvalidState(message))
            if message == "pending approval allow requires an active thread"
    ));
    assert!(
        store
            .has_pending_tool_call(&request.request_id)
            .expect("pending")
    );
    assert!(
        store
            .list_approval_decisions()
            .expect("decisions")
            .is_empty()
    );
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Blocked
    );
}

// 验证删除 thread 会级联清理 approval、decision、trace 与相关数据。
#[test]
fn thread_delete_removes_bound_approvals_decisions_and_traces() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("patch"),
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_turn_call_1",
        "thread_id": &thread.thread_id,
        "turn_id": &turn.turn_id,
        "tool_call_id": "call_1",
        "tool_name": "patch",
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    let request_trace = store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        )
        .expect("approval");
    assert_eq!(request_trace.run_id, thread.thread_id);
    assert_eq!(request_trace.session_id, turn.turn_id);
    assert_eq!(
        request_trace.task_id.as_deref(),
        Some(turn.turn_id.as_str())
    );
    assert!(
        store
            .list_trace(&thread.thread_id)
            .expect("thread trace")
            .iter()
            .any(|event| event.event_id == request_trace.event_id)
    );

    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Deny,
        "operator denied",
    );
    let decision_trace = store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("decision")
        .trace;
    assert_eq!(decision_trace.run_id, thread.thread_id);
    assert_eq!(
        decision_trace.task_id.as_deref(),
        Some(turn.turn_id.as_str())
    );

    store
        .delete_thread(&thread.thread_id)
        .expect("delete thread");

    assert!(store.list_pending_approvals().expect("pending").is_empty());
    assert!(matches!(
        store.get_approval_decision(&decision.decision_id),
        Err(StoreError::NotFound(message)) if message == format!("approval decision {}", decision.decision_id)
    ));
    assert!(matches!(
        store.list_trace(&thread.thread_id),
        Err(StoreError::NotFound(message)) if message == format!("trace run {}", thread.thread_id)
    ));
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

// 验证 turn user input 可供 approval resume 读取。
#[test]
fn turn_user_input_can_be_read_for_approval_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let payload = serde_json::json!([{"type": "text", "text": "resume this turn"}]);
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "blocked",
            payload.clone(),
            "app_server",
            "turn started",
        )
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

// 验证 turn start 在 trace insert 失败时回滚全部副作用。
#[test]
fn transactional_turn_start_rolls_back_when_trace_insert_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "
            create trigger fail_turn_trace
            before insert on trace_events
            when new.payload like '%rollback trace%'
            begin
                select raise(abort, 'forced trace failure');
            end;
            ",
        )
        .expect("install trigger");

    let failed = store.create_turn_with_input_and_trace(
        &thread.thread_id,
        "running",
        serde_json::json!([{"type": "text", "text": "rollback"}]),
        "test",
        "rollback trace",
    );

    assert!(failed.is_err());
    assert!(store.list_trace("missing_after_rollback").is_err());
    let successful = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "ok"}]),
            "test",
            "turn trace",
        )
        .expect("successful turn");
    assert!(store.get_turn(&successful.0.turn_id).is_ok());
}

// 验证终态 turn、assistant item 与 trace 在同一事务提交。
#[test]
fn terminal_turn_state_assistant_item_and_trace_commit_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "user"}]),
            "test",
            "turn started",
        )
        .expect("turn");
    let trace = TraceEvent::for_turn(
        "trace_terminal_success",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "terminal result",
    );
    let plan = serde_json::json!({
        "steps": [{"step": "verify", "status": "completed"}]
    });

    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_delta: Some("assistant"),
                plan: Some(&plan),
                trace: &trace,
            },
        )
        .expect("commit terminal outcome");

    assert_eq!(committed.turn.status, TurnStatus::Completed);
    assert_eq!(
        committed.plan_item.as_ref().map(|item| &item.kind),
        Some(&singularity_protocol::ItemKind::Plan)
    );
    assert_eq!(
        committed
            .plan_item
            .as_ref()
            .map(|item| item.payload.clone()),
        Some(plan)
    );
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .and_then(|item| item.payload["delta"].as_str()),
        Some("assistant")
    );
    assert_eq!(committed.trace.event_id, "trace_terminal_success");
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

// 验证终态提交的 trace 失败会回滚状态与 item。
#[test]
fn terminal_turn_commit_rolls_back_state_and_item_when_trace_insert_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "user"}]),
            "test",
            "turn started",
        )
        .expect("turn");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "
            create trigger fail_terminal_trace
            before insert on trace_events
            when new.payload like '%forced terminal failure%'
            begin
                select raise(abort, 'forced terminal trace failure');
            end;
            ",
        )
        .expect("install trigger");
    drop(connection);
    let trace = TraceEvent::for_turn(
        "trace_terminal_failure",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "forced terminal failure",
    );

    let result = store.commit_turn_outcome(
        &turn.turn_id,
        CommitTurnOutcomeParams {
            status: TurnStatus::Completed,
            agent_loop_status: "completed",
            assistant_delta: Some("assistant"),
            plan: None,
            trace: &trace,
        },
    );

    assert!(result.is_err());
    assert_eq!(
        store
            .get_turn(&turn.turn_id)
            .expect("turn after rollback")
            .status,
        TurnStatus::Running
    );
    assert!(
        store
            .read_thread_history(&thread.thread_id, None, 10)
            .expect("history")
            .messages
            .is_empty()
    );
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let assistant_count: u64 = connection
        .query_row(
            "select count(*) from items where turn_id = ?1 and kind = ?2",
            rusqlite::params![turn.turn_id, ItemKind::AgentMessage.as_storage_text()],
            |row| row.get(0),
        )
        .expect("assistant count");
    assert_eq!(assistant_count, 0);
}

// 验证 runtime turn trace 绑定错误以 typed StoreError 传播，而不是退化为字符串状态。
#[test]
fn turn_trace_binding_error_remains_typed_at_store_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent::for_turn(
        "trace_wrong_turn_binding",
        &thread.thread_id,
        "wrong_turn",
        "agent_loop",
        "invalid turn binding",
    );

    let error = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "interrupted",
                assistant_delta: None,
                plan: None,
                trace: &trace,
            },
        )
        .expect_err("mismatched turn trace must be rejected");

    assert!(matches!(
        error,
        StoreError::TraceBinding(TraceBindingError::SessionIdMismatch { expected, actual })
            if expected == turn.turn_id && actual == "wrong_turn"
    ));
    assert_eq!(
        store
            .get_turn(&turn.turn_id)
            .expect("turn after rejection")
            .status,
        TurnStatus::Running
    );
}

#[test]
fn missing_turn_trace_task_id_remains_typed_at_store_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let mut trace = TraceEvent::for_turn(
        "trace_missing_task",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "missing task",
    );
    trace.task_id = None;

    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "interrupted",
                assistant_delta: None,
                plan: None,
                trace: &trace,
            },
        ),
        Err(StoreError::TraceBinding(TraceBindingError::TaskIdMismatch { expected, actual: None }))
            if expected == turn.turn_id
    ));
}

// append_trace 的绑定检查与插入必须和 delete_thread 共享同一个写事务，不能留下孤立 turn trace。
#[test]
fn append_and_delete_race_cannot_leave_an_orphan_turn_trace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let setup = SessionStore::open(&db_path).expect("open store");
    let thread = setup.create_thread(None, None).expect("thread");
    let turn = setup
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    setup
        .update_turn_state(&turn.turn_id, TurnStatus::Completed, "completed")
        .expect("terminal turn");
    drop(setup);

    let append_store = SessionStore::open(&db_path).expect("append store");
    let delete_store = SessionStore::open(&db_path).expect("delete store");
    let trace = TraceEvent::for_turn(
        "trace_append_delete_race",
        &thread.thread_id,
        &turn.turn_id,
        "test",
        "race",
    );
    let barrier = Arc::new(Barrier::new(3));
    let append_barrier = Arc::clone(&barrier);
    let append_handle = std::thread::spawn(move || {
        append_barrier.wait();
        append_store.append_trace(&trace)
    });
    let delete_barrier = Arc::clone(&barrier);
    let thread_id = thread.thread_id.clone();
    let delete_handle = std::thread::spawn(move || {
        delete_barrier.wait();
        delete_store.delete_thread(&thread_id)
    });
    barrier.wait();

    let append_result = append_handle.join().expect("append worker");
    let delete_result = delete_handle.join().expect("delete worker");
    assert!(delete_result.is_ok(), "delete result: {delete_result:?}");
    assert!(
        append_result.is_ok()
            || matches!(append_result, Err(StoreError::InvalidState(ref message)) if message.contains("existing turn")),
        "append result: {append_result:?}"
    );

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    assert!(matches!(
        reopened.get_thread(&thread.thread_id),
        Err(StoreError::NotFound(_))
    ));
    let connection = rusqlite::Connection::open(&db_path).expect("inspect store");
    let trace_count: i64 = connection
        .query_row(
            "select count(*) from trace_events where event_id = 'trace_append_delete_race'",
            [],
            |row| row.get(0),
        )
        .expect("trace count");
    assert_eq!(trace_count, 0);
}

// 验证 trace 列表支持分页与尾部窗口读取。
#[test]
fn trace_list_supports_pagination_and_tail() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    for index in 0..3 {
        store
            .append_trace(&TraceEvent::new(
                format!("trace_{index}"),
                "run_1",
                "session_1",
                "test",
                format!("event {index}"),
            ))
            .expect("trace");
    }

    let page = store
        .list_trace_page("run_1", Some(1), Some(1))
        .expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].event_id, "trace_1");
    assert!(
        store
            .list_trace_page("run_1", Some(1), Some(99))
            .expect("empty page")
            .is_empty()
    );
    assert_eq!(
        store.tail_trace("run_1", 2, None).expect("tail")[0].event_id,
        "trace_1"
    );
    assert_eq!(
        store.tail_trace("run_1", 2, Some(1)).expect("offset tail")[0].event_id,
        "trace_0"
    );
}

// 验证 trace payload 递归脱敏并按 canonical JSON 计算 hash。
#[test]
fn trace_storage_redacts_recursively_and_hashes_canonical_payload() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut first = TraceEvent::new(
        "trace_redacted_1",
        "run_redacted",
        "session_redacted",
        "test",
        "Authorization: Bearer sentinel-secret-value",
    );
    first.payload = serde_json::json!({
        "z": 1,
        "nested": {
            "authorization": "Bearer sentinel-secret-value",
            "safe": "ok"
        }
    });
    let mut second = TraceEvent::new(
        "trace_redacted_2",
        "run_redacted",
        "session_redacted",
        "test",
        "safe summary",
    );
    second.payload = serde_json::json!({
        "nested": {
            "safe": "ok",
            "authorization": "different-secret-value"
        },
        "z": 1
    });

    store.append_trace(&first).expect("first trace");
    store.append_trace(&second).expect("second trace");
    let first = store.show_trace("trace_redacted_1").expect("stored first");
    let second = store.show_trace("trace_redacted_2").expect("stored second");

    assert_eq!(first.summary, "[redacted]");
    assert_eq!(first.payload["nested"]["authorization"], "[redacted]");
    assert_eq!(first.payload["nested"]["safe"], "ok");
    assert!(first.redaction_applied);
    assert!(first.payload_hash.starts_with("sha256:"));
    assert_eq!(first.payload_hash.len(), "sha256:".len() + 64);
    assert_eq!(first.payload_hash, second.payload_hash);
    let serialized = serde_json::to_string(&first).expect("serialize trace");
    assert!(!serialized.contains("sentinel-secret-value"));
}

// 验证被篡改的 trace payload hash 会 fail closed。
#[test]
fn tampered_trace_payload_hash_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut trace = TraceEvent::new(
        "trace_tampered",
        "run_tampered",
        "session_tampered",
        "test",
        "safe",
    );
    trace.payload = serde_json::json!({"safe": "before"});
    store.append_trace(&trace).expect("append trace");

    let connection = rusqlite::Connection::open(&db_path).expect("open tamper connection");
    let payload: String = connection
        .query_row(
            "select payload from trace_events where event_id = 'trace_tampered'",
            [],
            |row| row.get(0),
        )
        .expect("read payload");
    let mut payload: serde_json::Value = serde_json::from_str(&payload).expect("parse payload");
    payload["payload"]["safe"] = serde_json::json!("after");
    connection
        .execute(
            "update trace_events set payload = ?1 where event_id = 'trace_tampered'",
            [serde_json::to_string(&payload).expect("serialize tampered payload")],
        )
        .expect("tamper payload");

    let error = store
        .show_trace("trace_tampered")
        .expect_err("tampered trace must fail closed");
    assert!(error.to_string().contains("trace integrity"));
}

// 验证 trace tail 返回有界且按时间正序排列的最新窗口。
#[test]
fn trace_tail_returns_the_bounded_latest_window_in_chronological_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    for index in 0..100 {
        store
            .append_trace(&TraceEvent::new(
                format!("trace_window_{index}"),
                "run_window",
                "session_window",
                "test",
                format!("event {index}"),
            ))
            .expect("append trace");
    }

    let tail = store.tail_trace("run_window", 3, Some(2)).expect("tail");
    assert_eq!(
        tail.into_iter()
            .map(|event| event.event_id)
            .collect::<Vec<_>>(),
        vec!["trace_window_95", "trace_window_96", "trace_window_97"]
    );
}

// 验证 artifact ref 持久化并脱敏 secret-like metadata。
#[test]
fn artifact_refs_are_durable_and_redact_secret_like_metadata() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let artifact = store
        .register_artifact_ref(RegisterArtifactRefParams {
            run_id: "run_1",
            item_id: Some("item_1"),
            kind: "file",
            uri: "artifact://safe/result.txt",
            content_digest: "sha256:abc",
            summary: "contains token output",
            metadata: serde_json::json!({
                "path": "safe/result.txt",
                "api_key": "abc123",
                "nested": {"authorization": "Bearer abc123"}
            }),
        })
        .expect("artifact");

    assert!(artifact.redacted);
    assert_eq!(artifact.summary, "[redacted]");
    assert_eq!(artifact.metadata["api_key"], "[redacted]");
    assert_eq!(artifact.metadata["nested"]["authorization"], "[redacted]");
    let fetched = store
        .get_artifact_ref(&artifact.artifact_id)
        .expect("fetched");
    assert_eq!(fetched, artifact);
    assert_eq!(
        store.list_artifact_refs("run_1").expect("list")[0].artifact_id,
        artifact.artifact_id
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
    let (_, item, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "safe"}]),
            "test",
            "turn started",
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
            .create_turn_with_input_trace_and_history(
                &thread.thread_id,
                "running",
                serde_json::json!([{"type": "text", "text": "must not replay"}]),
                "test",
                "turn started",
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
        .create_turn_with_input_trace_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "incomplete completed turn"}]),
            "test",
            "turn started",
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
    store
        .append_trace(&TraceEvent::new(
            "history_trace",
            &thread.thread_id,
            &thread.thread_id,
            "test",
            "trace metadata",
        ))
        .expect("trace");
    let approval = ApprovalRequest::new(
        "history_approval",
        thread.thread_id.clone(),
        completed.clone(),
        tool_id("read"),
    );
    store.create_approval(&approval).expect("approval");
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
    store
        .register_artifact_ref(RegisterArtifactRefParams {
            run_id: &thread.thread_id,
            item_id: None,
            kind: "file",
            uri: "artifact://history/result.txt",
            content_digest: "sha256:history",
            summary: "artifact metadata",
            metadata: serde_json::json!({"safe": true}),
        })
        .expect("artifact");

    let history = store
        .read_thread_history(&thread.thread_id, None, 20)
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].content, "safe user");
    assert_eq!(history.messages[1].content, "safe assistant");
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
        .create_turn_with_input_trace_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": raw_user_secret}]),
            "test",
            "turn started",
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
        store.create_turn_with_input_trace_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "cannot start"}]),
            "test",
            "turn started",
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
        .create_turn_with_input_trace_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            "test",
            "turn started",
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
    assert_eq!(started.trace.run_id, thread.thread_id);
    assert_eq!(started.trace.session_id, started.turn.turn_id);
}

// 追加一条 completed conversation，供 history 测试构造数据。
fn append_completed_conversation(
    store: &SessionStore,
    thread_id: &str,
    user: &str,
    assistant: &str,
) -> String {
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": user}]),
            "test",
            "turn started",
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
                    serde_json::to_string(&PermissionProfileName::WorkspaceWrite)
                        .expect("sandbox mode"),
                    serde_json::to_string(&ApprovalPolicy::OnRequest).expect("approval policy"),
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

    let pending_request = if schema_version >= 7 {
        ApprovalRequest::new(
            "approval_pending",
            thread_id,
            pending_turn_id,
            tool_id("edit"),
        )
        .with_tool_call_id("call_pending")
    } else {
        ApprovalRequest::new(
            "approval_pending",
            thread_id,
            pending_turn_id,
            tool_id("edit"),
        )
    };
    let pending_payload = legacy_approval_payload(schema_version, &pending_request);
    connection
        .execute(
            "insert into approvals(request_id, payload, decision_outcome, decision_reason)
             values(?1, ?2, null, null)",
            rusqlite::params![pending_request.request_id, pending_payload],
        )
        .expect("insert pending legacy approval");

    if has_approval_decisions {
        let final_request = ApprovalRequest::new(
            "approval_final",
            thread_id,
            completed_turn_id,
            tool_id("read"),
        );
        let final_decision = ApprovalDecision::new(
            final_request.request_id.clone(),
            ApprovalOutcome::Allow,
            "legacy allow",
        );
        let allow = serde_json::to_string(&ApprovalOutcome::Allow).expect("approval outcome");
        connection
            .execute(
                "insert into approvals(
                     request_id, payload, decision_outcome, decision_reason
                 ) values(?1, ?2, ?3, ?4)",
                rusqlite::params![
                    final_request.request_id,
                    legacy_approval_payload(schema_version, &final_request),
                    allow,
                    final_decision.reason,
                ],
            )
            .expect("insert final legacy approval");
        connection
            .execute(
                "insert into approval_decisions(
                     decision_id, request_id, outcome, reason, payload
                 ) values(?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    final_decision.decision_id,
                    final_decision.request_id,
                    allow,
                    final_decision.reason,
                    serde_json::to_string(&final_decision).expect("final decision"),
                ],
            )
            .expect("insert final legacy decision");
    }

    if schema_version >= 7 {
        let checkpoint = serde_json::json!({
            "request_id": pending_request.request_id,
            "thread_id": thread_id,
            "turn_id": pending_turn_id,
            "tool_call_id": "call_pending",
            "tool_name": "edit",
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        });
        let checkpoint = serde_json::to_string(&checkpoint).expect("checkpoint");
        if has_pending_bindings {
            if has_execution_state {
                connection
                    .execute(
                        "insert into pending_tool_calls(
                             request_id, thread_id, turn_id, tool_call_id, payload, execution_state
                         ) values(?1, ?2, ?3, ?4, ?5, 'pending')",
                        rusqlite::params![
                            pending_request.request_id,
                            thread_id,
                            pending_turn_id,
                            "call_pending",
                            checkpoint,
                        ],
                    )
                    .expect("insert pending checkpoint");
            } else {
                connection
                    .execute(
                        "insert into pending_tool_calls(
                             request_id, thread_id, turn_id, tool_call_id, payload
                         ) values(?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![
                            pending_request.request_id,
                            thread_id,
                            pending_turn_id,
                            "call_pending",
                            checkpoint,
                        ],
                    )
                    .expect("insert pending checkpoint");
            }
        } else {
            connection
                .execute(
                    "insert into pending_tool_calls(request_id, turn_id, payload)
                     values(?1, ?2, ?3)",
                    rusqlite::params![pending_request.request_id, pending_turn_id, checkpoint],
                )
                .expect("insert pending checkpoint");
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

fn legacy_approval_payload(schema_version: u32, request: &ApprovalRequest) -> String {
    let value = match schema_version {
        1..=3 => serde_json::json!({
            "request_id": request.request_id,
            "session_id": request.thread_id,
            "task_id": request.turn_id,
            "action": request.action,
            "reason": request.reason,
        }),
        4 => serde_json::json!({
            "request_id": request.request_id,
            "session_id": request.thread_id,
            "task_id": request.turn_id,
            "action": request.action,
            "resources": request.resources,
            "reason": request.reason,
        }),
        5 | 6 => serde_json::json!({
            "request_id": request.request_id,
            "session_id": request.thread_id,
            "task_id": request.turn_id,
            "thread_id": request.thread_id,
            "turn_id": request.turn_id,
            "tool_call_id": request.tool_call_id,
            "action": request.action,
            "resources": request.resources,
            "reason": request.reason,
        }),
        7..=9 => serde_json::json!({
            "request_id": request.request_id,
            "thread_id": request.thread_id,
            "turn_id": request.turn_id,
            "tool_call_id": request.tool_call_id,
            "action": request.action,
            "resources": request.resources,
            "reason": request.reason,
        }),
        _ => panic!("unsupported legacy schema version {schema_version}"),
    };
    serde_json::to_string(&value).expect("serialize legacy approval")
}

fn legacy_trace_payload(
    event_id: &str,
    run_id: &str,
    session_id: &str,
    task_id: Option<&str>,
) -> String {
    let mut event = TraceEvent::new(event_id, run_id, session_id, "legacy", "legacy trace");
    event.task_id = task_id.map(str::to_string);
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
