//! 验证 SessionStore 的 schema、绑定、恢复、历史、trace 与事务不变量。

use schemars::schema_for;
use serde_json::Value;
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionProfileName,
    PermissionResource, ToolId, WorkspaceRelativePath,
};
use singularity_protocol::{
    ItemKind, ThreadStatus, TraceApprovalOutcome, TraceApprovalProjection, TraceBindingError,
    TraceErrorCategory, TraceErrorProjection, TraceErrorStage, TraceEvent,
    TraceFinalReviewProjection, TraceFinalReviewStatus, TraceMetricAvailability, TraceMetricSample,
    TraceMetricSampleKind, TracePolicyCause, TracePolicyDecision, TracePolicyProjection,
    TraceProviderOperationPhase, TraceProviderProtocol, TraceSandboxEnforcement,
    TraceSandboxProjection, TraceSandboxStatus, TraceSpanKind, TraceSpanPhase, TraceSpanProjection,
    TraceSpanStatus, TraceToolProjection, TraceToolStatus, TraceUsage, TraceVerificationProjection,
    TraceVerificationStatus, TraceWorkspaceMutation, TurnInputDelivery, TurnStatus,
};
use singularity_store::{
    CommitTurnOutcomeParams, ConversationRole, RegisterArtifactRefParams, SessionStore,
    SessionStoreDescriptor, StoreError, ToolExecution, ToolExecutionState, TurnOutcomeAuthority,
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

#[test]
fn typed_turn_start_is_atomic_and_duplicate_start_is_idempotent() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let started = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "hello"}]),
            "app_server",
            "turn started",
        )
        .expect("started turn");

    assert_eq!(started.2.span_kind, Some(TraceSpanKind::Turn));
    assert_eq!(started.2.span_phase, Some(TraceSpanPhase::Start));
    assert!(started.2.timestamp.is_some());
    assert_eq!(
        store
            .append_trace_idempotent(&started.2)
            .expect("duplicate start is idempotent"),
        started.2
    );
    let mut resumed_start = started.2.clone();
    resumed_start.timestamp = Some("2026-01-01T00:00:00Z".to_string());
    assert_eq!(
        store
            .append_trace_idempotent(&resumed_start)
            .expect("same occurrence start is idempotent across resume"),
        started.2
    );
    assert_eq!(
        store
            .list_trace(&thread.thread_id)
            .expect("trace list")
            .iter()
            .filter(|event| event.span_kind == Some(TraceSpanKind::Turn))
            .count(),
        1
    );
}

#[test]
fn typed_turn_end_recovers_persisted_start_after_sqlite_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, start) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "hello"}]),
            "app_server",
            "turn started",
        )
        .expect("started turn");
    drop(store);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let terminal = TraceEvent::for_turn(
        "trace_terminal_failure",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "terminal failure",
    );
    store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &terminal,
            },
        )
        .expect("terminal outcome");

    let trace = store.list_trace(&thread.thread_id).expect("trace list");
    let end = trace
        .iter()
        .find(|event| {
            event.span_kind == Some(TraceSpanKind::Turn)
                && event.span_phase == Some(TraceSpanPhase::End)
        })
        .expect("typed turn end");
    assert_eq!(end.span_id, start.span_id);
    assert_eq!(end.span_status, Some(TraceSpanStatus::Error));
    assert!(end.timestamp.is_some());
    assert!(end.duration_ms.is_some());
}

#[test]
fn approval_wait_span_defers_without_end_then_closes_once_on_allow() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_typed_wait",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_typed_wait");
    let checkpoint = serde_json::json!({"checkpoint": "opaque"});

    let started = store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("approval start");
    let turn_start = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|event| {
            event.span_kind == Some(TraceSpanKind::Turn)
                && event.span_phase == Some(TraceSpanPhase::Start)
        })
        .expect("turn start");
    assert_eq!(started.span_kind, Some(TraceSpanKind::ApprovalWait));
    assert_eq!(started.span_phase, Some(TraceSpanPhase::Start));
    assert_eq!(started.parent_span_id, turn_start.span_id);

    store
        .record_approval_decision(
            &ApprovalDecision::new(request.request_id.clone(), ApprovalOutcome::Defer, "later"),
            "approval",
            "approval deferred",
        )
        .expect("defer");
    let after_defer = store.list_trace(&thread.thread_id).expect("trace list");
    assert_eq!(
        after_defer
            .iter()
            .filter(|event| event.span_id == started.span_id)
            .count(),
        1
    );
    assert!(!after_defer.iter().any(|event| {
        event.span_id == started.span_id && event.span_phase == Some(TraceSpanPhase::End)
    }));

    let allowed = store
        .record_approval_decision(
            &ApprovalDecision::new(
                request.request_id.clone(),
                ApprovalOutcome::Allow,
                "approved",
            ),
            "approval",
            "approval allowed",
        )
        .expect("allow");
    assert_eq!(allowed.trace.span_kind, Some(TraceSpanKind::ApprovalWait));
    assert_eq!(allowed.trace.span_phase, Some(TraceSpanPhase::End));
    assert_eq!(allowed.trace.parent_span_id, turn_start.span_id);
    assert!(allowed.trace.duration_ms.is_some());
}

// 验证 thread、turn、item、trace 与 approval 的基础持久化闭环。
#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
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

// v10 approval strings are converted only through the released tool-specific contract.
#[test]
fn v10_approval_resources_migrate_to_typed_v11_payloads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    create_v10_database(&db_path);
    let request = ApprovalRequest::new("approval_v10", "thread_v10", "turn_v10", tool_id("edit"))
        .with_resources([workspace_resource("README.md")]);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
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
            "insert into approvals(
                 request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             ) values(?1, ?2, ?3, ?4, null, null)",
            rusqlite::params![
                request.request_id,
                request.thread_id,
                request.turn_id,
                serde_json::to_string(&legacy_payload).expect("legacy approval"),
            ],
        )
        .expect("insert v10 approval");
    drop(connection);

    let migrated = SessionStore::open(&db_path).expect("migrate v10 store");
    assert_eq!(migrated.descriptor().schema_version, 13);
    assert_eq!(
        migrated
            .get_pending_approval("approval_v10")
            .expect("typed approval")
            .resources,
        vec![workspace_resource("README.md")]
    );
}

// 现行 v12（typed trace、但尚无 turn resume 表）必须原子升级到 v13，且不丢失既有行。
#[test]
fn v12_to_v13_migration_adds_recovery_tables_and_preserves_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v12.sqlite3");
    let store = SessionStore::open(&db_path).expect("open v13 store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent::for_turn(
        "v12_preserved_trace",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        "test",
        "preserved",
    );
    store.append_trace(&trace).expect("trace");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open v13 sqlite");
    connection
        .execute_batch(
            r#"
pragma foreign_keys = off;
drop table turn_inputs;
drop table turn_checkpoints;
drop table tool_executions;
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
    assert!(
        migrated
            .list_trace(&thread.thread_id)
            .expect("trace list")
            .iter()
            .any(|event| event.event_id == trace.event_id)
    );
    let connection = rusqlite::Connection::open(&db_path).expect("open migrated sqlite");
    for table in ["turn_checkpoints", "tool_executions"] {
        let exists: bool = connection
            .query_row(
                "select exists(select 1 from sqlite_schema where type = 'table' and name = ?1)",
                [table],
                |row| row.get(0),
            )
            .expect("recovery table lookup");
        assert!(exists, "missing migrated table {table}");
    }
}

#[test]
fn turn_input_is_idempotent_ordered_and_consumed_with_its_checkpoint_once() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
            "app_server",
            "turn started",
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
    store
        .consume_turn_inputs_with_checkpoint(
            &turn.turn_id,
            &thread.thread_id,
            &input_ids,
            &serde_json::json!({"version": 2, "state": "after-input"}),
            2,
            false,
        )
        .expect("consume input and checkpoint");
    assert!(
        store
            .turn_boundary_state(&turn.turn_id, true)
            .expect("consumed boundary")
            .inputs
            .is_empty()
    );
    assert!(matches!(
        store.consume_turn_inputs_with_checkpoint(
            &turn.turn_id,
            &thread.thread_id,
            &input_ids,
            &serde_json::json!({"version": 2, "state": "duplicate"}),
            2,
            false,
        ),
        Err(StoreError::InvalidState(_))
    ));
}

#[test]
fn accepted_turn_input_remains_idempotent_after_turn_terminalizes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
            "app_server",
            "turn started",
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
    store
        .consume_turn_inputs_with_checkpoint(
            &turn.turn_id,
            &thread.thread_id,
            &["accepted-input".to_string()],
            &serde_json::json!({"version": 2, "state": "after-input"}),
            2,
            false,
        )
        .expect("consume accepted input");
    store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("done"),
                trace: &TraceEvent::for_turn(
                    "trace_terminal_after_accepted_input",
                    &thread.thread_id,
                    &turn.turn_id,
                    "agent_loop",
                    "terminal result",
                ),
            },
        )
        .expect("terminal turn after consuming input");

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
    assert_eq!(retried.status, TurnStatus::Completed);
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
    assert!(matches!(
        store.append_turn_input(
            &turn.turn_id,
            "new-input",
            TurnInputDelivery::Steer,
            &input,
        ),
        Err(StoreError::InvalidState(message))
            if message == "terminal turn cannot accept interactive input"
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
    assert_eq!(delivery_state, "consumed");
}

#[test]
fn pending_input_blocks_terminal_commit_and_pause_remains_resumable() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
            "app_server",
            "turn started",
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
    let terminal = TraceEvent::for_turn(
        "trace_terminal_with_pending_input",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "terminal result",
    );
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &terminal,
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
    let input_ids = boundary
        .inputs
        .iter()
        .map(|input| input.input_id.clone())
        .collect::<Vec<_>>();
    store
        .consume_turn_inputs_with_checkpoint(
            &turn.turn_id,
            &thread.thread_id,
            &input_ids,
            &serde_json::json!({"version": 2, "state": "paused"}),
            2,
            true,
        )
        .expect("consume and pause");
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("paused turn").status,
        TurnStatus::Paused
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
fn approval_allow_hands_steer_to_one_turn_checkpoint_atomically() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_input_handoff",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_input_handoff");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(serde_json::json!({"approval_checkpoint": "opaque"})),
            "approval",
            "approval requested",
        )
        .expect("blocked approval");
    store
        .append_turn_input(
            &turn.turn_id,
            "approval-steer",
            TurnInputDelivery::Steer,
            &serde_json::json!([{"type": "text", "text": "change direction"}]),
        )
        .expect("steer");
    let checkpoint = serde_json::json!({"version": 2, "state": "steered"});
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    assert!(matches!(
        store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded without boundary"
        ),
        Err(StoreError::TurnBoundaryPending { .. })
    ));
    assert!(
        store
            .has_pending_tool_call(&request.request_id)
            .expect("pending execution")
    );

    let recorded = store
        .record_approval_decision_with_turn_checkpoint(
            &decision,
            "approval",
            "approval decision recorded",
            &["approval-steer".to_string()],
            &checkpoint,
            2,
            false,
        )
        .expect("atomic approval input handoff");
    assert_eq!(recorded.turn.status, TurnStatus::Running);
    assert_eq!(
        store
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint"),
        Some(checkpoint)
    );
    assert!(
        store
            .turn_boundary_state(&turn.turn_id, true)
            .expect("boundary")
            .inputs
            .is_empty()
    );
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending execution")
    );
    assert_eq!(
        store
            .get_approval_decision(&decision.decision_id)
            .expect("decision"),
        decision
    );
}

#[test]
fn approval_deny_hands_pending_input_to_same_turn_without_terminalizing() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_deny_handoff",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_deny_handoff");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(serde_json::json!({"approval_checkpoint": "opaque"})),
            "approval",
            "approval requested",
        )
        .expect("blocked approval");
    store
        .append_turn_input(
            &turn.turn_id,
            "deny-follow-up",
            TurnInputDelivery::FollowUp,
            &serde_json::json!([{"type": "text", "text": "use a different approach"}]),
        )
        .expect("follow-up");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Deny,
        "operator denied edit",
    );
    assert!(matches!(
        store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded without boundary"
        ),
        Err(StoreError::TurnBoundaryPending { .. })
    ));

    let checkpoint = serde_json::json!({"version": 2, "state": "denied"});
    let recorded = store
        .record_approval_decision_with_turn_checkpoint(
            &decision,
            "approval",
            "approval decision recorded",
            &["deny-follow-up".to_string()],
            &checkpoint,
            2,
            false,
        )
        .expect("deny handoff");
    assert_eq!(recorded.turn.status, TurnStatus::Running);
    assert_eq!(
        store
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint"),
        Some(checkpoint)
    );
    assert!(
        store
            .turn_boundary_state(&turn.turn_id, true)
            .expect("boundary")
            .inputs
            .is_empty()
    );
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending execution")
    );
    assert_eq!(
        store
            .get_approval_decision(&decision.decision_id)
            .expect("decision"),
        decision
    );
}

// Build the released v10 schema directly so v10 migration tests do not
// relabel a current v13 database as an older version.
fn create_v10_database(path: &std::path::Path) {
    let connection = rusqlite::Connection::open(path).expect("open v10 sqlite");
    connection
        .execute_batch(
            r#"
create table schema_meta(
schema_version integer not null check(schema_version = 10)
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
        check(kind in ('userMessage', 'agentMessage', 'reasoning', 'plan', 'commandExecution', 'fileChange')),
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
    decision_outcome text check(decision_outcome in ('allow', 'deny') or decision_outcome is null),
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
insert into schema_meta(schema_version) values(10);
insert into schema_migrations(migration_id) values
    ('0001_initial_session_store'),
    ('0002_durable_ledger'),
    ('0004_pending_tool_calls'),
    ('0005_store_hardening'),
    ('0006_conversation_history'),
    ('0007_pending_execution_state'),
    ('0008_approval_execution_recovery'),
    ('0009_thread_policy_snapshot'),
    ('0010_stable_enum_text');
insert into threads(thread_id, model, cwd, status, sandbox_mode, approval_policy)
values('thread_v10', null, null, 'active', 'workspace-write', 'on-request');
insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status)
values('turn_v10', 'thread_v10', 1, 'running', 'running');
"#,
        )
        .expect("create v10 database");
}

fn create_v11_database(path: &std::path::Path) {
    create_v10_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v11 sqlite");
    connection
        .execute_batch(
            "create table schema_meta_v11(
                 schema_version integer not null check(schema_version = 11)
             );
             insert into schema_meta_v11(schema_version) values(11);
             drop table schema_meta;
             alter table schema_meta_v11 rename to schema_meta;
             insert into schema_migrations(migration_id)
             values('0011_typed_permission_resources');",
        )
        .expect("upgrade v10 schema metadata to v11");
}

fn prepare_v10_pending_checkpoint(
    db_path: &std::path::Path,
    checkpoint: &mut Value,
) -> ApprovalRequest {
    create_v10_database(db_path);
    let request = ApprovalRequest::new(
        "approval_pending_v10",
        "thread_v10",
        "turn_v10",
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    checkpoint["request_id"] = serde_json::json!(&request.request_id);
    checkpoint["thread_id"] = serde_json::json!(&request.thread_id);
    checkpoint["turn_id"] = serde_json::json!(&request.turn_id);
    checkpoint["tool_call_id"] = serde_json::json!("call_1");
    let connection = rusqlite::Connection::open(db_path).expect("open sqlite");
    let legacy_approval = serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": &request.tool_call_id,
        "action": "edit",
        "resources": ["README.md"],
        "reason": ""
    });
    connection
        .execute(
            "update turns set status = 'blocked', agent_loop_status = 'blocked' where turn_id = ?1",
            [&request.turn_id],
        )
        .expect("block v10 turn");
    connection
        .execute(
            "insert into approvals(
                 request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             ) values(?1, ?2, ?3, ?4, null, null)",
            rusqlite::params![
                request.request_id,
                request.thread_id,
                request.turn_id,
                serde_json::to_string(&legacy_approval).expect("legacy approval"),
            ],
        )
        .expect("insert legacy approval");
    connection
        .execute(
            "insert into pending_tool_calls(
                 request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) values(?1, ?2, ?3, ?4, ?5, 'pending')",
            rusqlite::params![
                request.request_id,
                request.thread_id,
                request.turn_id,
                "call_1",
                serde_json::to_string(checkpoint).expect("checkpoint"),
            ],
        )
        .expect("insert legacy checkpoint");
    drop(connection);
    request
}

// 旧数据库含未完成 AgentLoop checkpoint 时，在任何 schema 写入前拒绝迁移。
#[test]
fn v10_pending_checkpoint_rejects_migration_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let mut checkpoint = serde_json::json!({
        "request_id": "approval_pending_v10",
        "thread_id": "thread_v10",
        "turn_id": "turn_v10",
        "tool_call_id": "call_1",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": ["README.md"],
        "checkpoint_version": 1,
        "messages": [{
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "tool_call_id": "call_1",
                "tool_name": "edit",
                "arguments": {},
                "raw_arguments": "{}",
                "parse_status": "valid",
                "validation_errors": []
            }]
        }],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {
            "workspace_mutated": false,
            "workspace_revision": null,
            "successful_command_count": 0,
            "required_command_counts": {},
            "terminal_command_scope_digests": [],
            "terminal_command_revisions": [],
            "unresolved_failures": []
        },
        "last_completion_error": null
    });
    let request = prepare_v10_pending_checkpoint(&db_path, &mut checkpoint);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("v10 pending AgentLoop checkpoint")
                && message.contains(&request.request_id)
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
    assert!(!has_v11_temporary_tables(&db_path));
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
    remove_legacy_pending_approval(&db_path, 8);

    let migrated = SessionStore::open(&db_path).expect("migrate v8 store");
    assert_eq!(migrated.descriptor().schema_version, 13);
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

// 验证不含未完成 checkpoint 的 v1-v9 历史 schema 在同一 v11 事务中完成转换。
#[test]
fn every_supported_legacy_schema_migrates_with_trace_and_approval_data() {
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
        assert_eq!(
            store
                .list_pending_approvals()
                .expect("pending approvals")
                .iter()
                .map(|request| request.request_id.as_str())
                .collect::<Vec<_>>(),
            Vec::<&str>::new()
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

// 任何旧 schema 的 pending checkpoint 都不属于当前 AgentLoop codec；迁移不能伪造状态。
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
                if message.contains("pending AgentLoop checkpoint")
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
fn v7_non_pending_execution_states_reject_migration_without_mutation() {
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
        let before = sqlite_snapshot(&db_path);

        assert!(matches!(
            SessionStore::open(&db_path),
            Err(StoreError::InvalidState(message))
                if message.contains("v7 pending AgentLoop checkpoint")
        ));
        assert_eq!(sqlite_snapshot(&db_path), before);
        assert!(!has_v11_temporary_tables(&db_path));
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
            "checkpoint" => assert_eq!(
                trusted
                    .get_pending_tool_call(&request.request_id)
                    .expect("opaque checkpoint payload"),
                Some(serde_json::json!({}))
            ),
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
    remove_legacy_pending_approval(&db_path, 9);
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
            if message.contains("current schema fingerprint is not canonical")
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

    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call),
            "approval",
            "approval requested",
        )
        .expect("Store persists opaque checkpoint payload");
    assert_eq!(store.list_pending_approvals().expect("pending").len(), 1);
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
            if message == "pending approval checkpoint requires an explicit tool_call_id"
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

    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_tool_call.clone()),
            "approval",
            "approval requested",
        )
        .expect("Store persists opaque checkpoint payload");
    assert_eq!(
        store
            .get_pending_tool_call(&request.request_id)
            .expect("opaque checkpoint payload"),
        Some(pending_tool_call)
    );
}

// 验证相同对象型 checkpoint 的 approval batch 重试按持久化序列化保持幂等。
#[test]
fn approval_batch_retry_accepts_same_opaque_object_checkpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_batch_idempotent",
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
        .create_approval_batch_with_pending_tool_calls_and_trace(
            &[(request.clone(), checkpoint.clone())],
            "approval",
            "approval requested",
        )
        .expect("initial approval batch");
    let retry = store
        .create_approval_batch_with_pending_tool_calls_and_trace(
            &[(request, checkpoint)],
            "approval",
            "approval requested",
        )
        .expect("idempotent approval batch retry");

    assert!(retry.is_empty());
    assert_eq!(store.list_pending_approvals().expect("pending").len(), 1);
}

// 验证批次第二条 trace 写入失败时，approval、checkpoint、turn 和首条 trace 全部回滚。
#[test]
fn approval_batch_rolls_back_when_second_write_hits_existing_trace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let first = ApprovalRequest::new(
        "approval_batch_first",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1");
    let second = ApprovalRequest::new(
        "approval_batch_second",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_2");
    let checkpoint = |request: &ApprovalRequest, tool_call_id: &str| {
        serde_json::json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
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
        .append_trace(&TraceEvent::for_turn(
            format!("trace_{}", second.request_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "test",
            "reserved event id",
        ))
        .expect("reserve second trace id");

    let result = store.create_approval_batch_with_pending_tool_calls_and_trace(
        &[
            (first.clone(), checkpoint(&first, "call_1")),
            (second.clone(), checkpoint(&second, "call_2")),
        ],
        "approval",
        "approval requested",
    );
    assert!(matches!(result, Err(StoreError::Sqlite(_))));
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Running
    );
    assert_eq!(
        store
            .get_turn(&turn.turn_id)
            .expect("turn")
            .agent_loop_status,
        "running"
    );
    assert!(
        store
            .list_pending_approvals()
            .expect("approvals")
            .is_empty()
    );
    assert!(
        !store
            .has_pending_tool_call(&first.request_id)
            .expect("first checkpoint")
    );
    assert!(
        !store
            .has_pending_tool_call(&second.request_id)
            .expect("second checkpoint")
    );
    let trace_ids = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .filter(|trace| {
            !(trace.span_kind == Some(TraceSpanKind::Turn)
                && trace.span_phase == Some(TraceSpanPhase::Start))
        })
        .map(|trace| trace.event_id)
        .collect::<Vec<_>>();
    assert_eq!(trace_ids, vec![format!("trace_{}", second.request_id)]);
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
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("too late"),
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
                assistant_item_id: None,
                assistant_delta: None,
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
        .filter(|trace| {
            !(trace.span_kind == Some(TraceSpanKind::Turn)
                && trace.span_phase == Some(TraceSpanPhase::Start))
        })
        .map(|trace| trace.event_id)
        .collect::<Vec<_>>();
    assert_eq!(trace_ids, vec!["trace_cancel_requested", "trace_cancelled"]);
}

// 验证 monitor 基础设施故障优先于 cancel_requested，并原子清理 executing checkpoint。
#[test]
fn infrastructure_failure_after_approval_claim_terminalizes_and_cleans_checkpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_infra_after_claim",
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
        .expect("pending approval");
    store
        .record_approval_decision(
            &ApprovalDecision::new(request.request_id.clone(), ApprovalOutcome::Allow, "allow"),
            "approval",
            "approval decision recorded",
        )
        .expect("claim approval");
    store
        .request_turn_cancellation(
            &turn.turn_id,
            &TraceEvent::for_turn(
                "trace_infra_after_claim_cancel",
                thread.thread_id.clone(),
                turn.turn_id.clone(),
                "app_server",
                "turn interrupt requested",
            ),
        )
        .expect("request cancellation");

    let committed = store
        .commit_turn_outcome_and_resolve_pending_execution_with_authority(
            &request.request_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &TraceEvent::for_turn(
                    "trace_infra_after_claim_failed",
                    thread.thread_id.clone(),
                    turn.turn_id.clone(),
                    "agent_loop",
                    "turn execution failed",
                ),
            },
            &[],
            TurnOutcomeAuthority::InfrastructureFailure,
        )
        .expect("infra failure must win cancellation");
    assert_eq!(committed.turn.status, TurnStatus::Failed);
    assert_eq!(committed.turn.agent_loop_status, "failed");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("executing checkpoint cleanup")
    );
}

// 验证 claim 后的终态 trace 写入失败不会留下部分状态，随后安全补偿仍可原子收口。
#[test]
fn approval_terminal_store_failure_rolls_back_then_allows_safe_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_terminal_store_failure",
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
        .expect("pending approval");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("claim approval");

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "
            create trigger fail_approval_terminal_trace
            before insert on trace_events
            when new.payload like '%forced approval terminal failure%'
            begin
                select raise(abort, 'forced approval terminal trace failure');
            end;
            ",
        )
        .expect("install trigger");
    drop(connection);

    let failed_attempt = store.commit_turn_outcome_and_resolve_pending_execution(
        &request.request_id,
        CommitTurnOutcomeParams {
            status: TurnStatus::Failed,
            agent_loop_status: "failed",
            assistant_item_id: None,
            assistant_delta: None,
            trace: &TraceEvent {
                payload: serde_json::json!({"error": "forced approval terminal failure"}),
                ..TraceEvent::for_turn(
                    "trace_approval_terminal_failure",
                    thread.thread_id.clone(),
                    turn.turn_id.clone(),
                    "agent_loop",
                    "approval terminal attempt",
                )
            },
        },
        &[],
    );
    assert!(matches!(failed_attempt, Err(StoreError::Sqlite(_))));
    let after_rollback = store.get_turn(&turn.turn_id).expect("turn after rollback");
    assert_eq!(after_rollback.status, TurnStatus::Blocked);
    assert_eq!(after_rollback.agent_loop_status, "blocked");
    assert!(
        store
            .has_pending_tool_call(&request.request_id)
            .expect("executing checkpoint remains for compensation")
    );

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch("drop trigger fail_approval_terminal_trace")
        .expect("remove trigger");
    drop(connection);

    let committed = store
        .commit_turn_outcome_and_resolve_pending_execution(
            &request.request_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &TraceEvent::for_turn(
                    "trace_approval_terminal_cleanup",
                    thread.thread_id.clone(),
                    turn.turn_id.clone(),
                    "agent_loop",
                    "approval continuation failed during approval_checkpoint",
                ),
            },
            &[],
        )
        .expect("safe cleanup");
    assert_eq!(committed.turn.status, TurnStatus::Failed);
    assert_eq!(committed.turn.agent_loop_status, "failed");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("checkpoint cleanup")
    );
}

// Infrastructure authority is a typed Failed reduction, never an alternate
// spelling of cancellation or a way to publish a successful late result.
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
            .request_turn_cancellation(
                &turn.turn_id,
                &TraceEvent::for_turn(
                    format!("trace_infrastructure_authority_{}", agent_loop_status),
                    thread.thread_id.clone(),
                    turn.turn_id.clone(),
                    "test",
                    "cancel requested",
                ),
            )
            .expect("cancel request");

        let assistant_item_id = SessionStore::allocate_assistant_item_id();
        let result = store.commit_turn_outcome_with_authority(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status,
                agent_loop_status,
                assistant_item_id: is_completed.then_some(&assistant_item_id),
                assistant_delta: is_completed.then_some("late result"),
                trace: &TraceEvent::for_turn(
                    format!(
                        "trace_infrastructure_authority_result_{}",
                        agent_loop_status
                    ),
                    thread.thread_id.clone(),
                    turn.turn_id.clone(),
                    "test",
                    "infrastructure result",
                ),
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
            store
                .list_trace(&thread.thread_id)
                .expect("trace list")
                .into_iter()
                .find(|event| event.event_id == trace.event_id)
                .map(|event| event.event_id),
            Some(trace.event_id)
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

// B1：paused/suspended turn 的 interrupt 在当前进程当场收敛（无需重启），
// checkpoint 审计证据保留，typed trace 记录 owner-loss 收敛。
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
            .save_turn_checkpoint(
                &turn.turn_id,
                &thread.thread_id,
                &serde_json::json!({"checkpoint_version": 1, "boundary": "initial"}),
                1,
            )
            .expect("checkpoint");
        store
            .update_turn_state(&turn.turn_id, status, agent_loop_status)
            .expect("ownerless state");
        let trace = TraceEvent::for_turn(
            "trace_interrupt_ownerless",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "app_server",
            "turn interrupt requested",
        );
        let interrupted = store
            .request_turn_cancellation(&turn.turn_id, &trace)
            .expect("interrupt");
        assert_eq!(interrupted.status, TurnStatus::Interrupted);
        assert_eq!(interrupted.agent_loop_status, "cancelled");
        // 未重启：当前 Store 会话即可看到终态（同一进程收敛）。
        let persisted = store.get_turn(&turn.turn_id).expect("turn");
        assert_eq!(persisted.status, TurnStatus::Interrupted);
        assert_eq!(persisted.agent_loop_status, "cancelled");
        // checkpoint 保留为审计证据。
        assert!(
            store
                .get_turn_checkpoint(&turn.turn_id)
                .expect("checkpoint lookup")
                .is_some()
        );
        let recovery_trace = store
            .list_trace(&thread.thread_id)
            .expect("trace list")
            .into_iter()
            .find(|trace| trace.event_id == "trace_interrupt_ownerless")
            .expect("interrupt trace");
        assert_eq!(recovery_trace.payload["tool_replayed"], false);
        assert_eq!(
            recovery_trace.payload["recovery_reason"],
            "execution_owner_lost"
        );
        assert_eq!(recovery_trace.payload["previous_status"], agent_loop_status);
    }
}

// B2：启动恢复将可归属不一致的 ownerless turn 终态化（Interrupted/interrupted），
// 保留审计证据、清理未解决 pending approval，且不阻断健康 sibling。
#[test]
fn recovery_terminalizes_inconsistent_ownerless_turns_without_blocking_siblings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("store");

    // 场景 1：suspended 缺 checkpoint（附 unknown execution 验证保留）。
    let thread_1 = store.create_thread(None, None).expect("thread 1");
    let turn_1 = store
        .create_turn(&thread_1.thread_id, "running")
        .expect("turn 1");
    store
        .update_turn_state(&turn_1.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend 1");
    let execution_1 = format!("exec:unknown:{}", turn_1.turn_id);
    let connection = rusqlite::Connection::open(&db_path).expect("sqlite");
    connection
        .execute(
            "insert into tool_executions(
                execution_id, thread_id, turn_id, tool_call_id, execution_state, payload
             ) values(?1, ?2, ?3, ?4, 'unknown', ?5)",
            rusqlite::params![
                execution_1,
                thread_1.thread_id,
                turn_1.turn_id,
                "call_1",
                serde_json::to_string(
                    &serde_json::json!({"kind": "tool_call", "tool_name": "read"})
                )
                .expect("execution payload")
            ],
        )
        .expect("insert unknown execution");
    drop(connection);

    // 场景 2：paused 缺 checkpoint。
    let thread_2 = store.create_thread(None, None).expect("thread 2");
    let turn_2 = store
        .create_turn(&thread_2.thread_id, "running")
        .expect("turn 2");
    store
        .update_turn_state(&turn_2.turn_id, TurnStatus::Paused, "paused")
        .expect("pause 2");

    // 场景 3：paused 带未解决 pending approval（approvals + pending_tool_calls）。
    let thread_3 = store.create_thread(None, None).expect("thread 3");
    let turn_3 = store
        .create_turn(&thread_3.thread_id, "running")
        .expect("turn 3");
    let request_3 = ApprovalRequest::new(
        "approval_recovery_3",
        thread_3.thread_id.clone(),
        turn_3.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_3")
    .with_resources([workspace_resource("README.md")]);
    let checkpoint_3 = serde_json::json!({
        "request_id": &request_3.request_id,
        "thread_id": &request_3.thread_id,
        "turn_id": &request_3.turn_id,
        "tool_call_id": "call_3",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": &request_3.resources,
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
            &request_3,
            Some(checkpoint_3),
            "approval",
            "approval requested",
        )
        .expect("pending 3");
    store
        .update_turn_state(&turn_3.turn_id, TurnStatus::Paused, "paused")
        .expect("pause 3");

    // 场景 4：suspended 带未解决 pending approval（ownerless 状态残留的
    // 不一致 pending；running+pending 会被 store 打开 preflight 拒绝，属于
    // 不可归属损坏，保持 fail-closed，不在本场景内）。
    let thread_4 = store.create_thread(None, None).expect("thread 4");
    let turn_4 = store
        .create_turn(&thread_4.thread_id, "running")
        .expect("turn 4");
    let request_4 = ApprovalRequest::new(
        "approval_recovery_4",
        thread_4.thread_id.clone(),
        turn_4.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_4")
    .with_resources([workspace_resource("README.md")]);
    let checkpoint_4 = serde_json::json!({
        "request_id": &request_4.request_id,
        "thread_id": &request_4.thread_id,
        "turn_id": &request_4.turn_id,
        "tool_call_id": "call_4",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": &request_4.resources,
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
            &request_4,
            Some(checkpoint_4),
            "approval",
            "approval requested",
        )
        .expect("pending 4");
    store
        .update_turn_state(&turn_4.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend 4");

    // 健康 sibling 1：suspended 有 checkpoint（正常可恢复）。
    let thread_5 = store.create_thread(None, None).expect("thread 5");
    let turn_5 = store
        .create_turn(&thread_5.thread_id, "running")
        .expect("turn 5");
    store
        .save_turn_checkpoint(
            &turn_5.turn_id,
            &thread_5.thread_id,
            &serde_json::json!({"checkpoint_version": 1, "boundary": "initial"}),
            1,
        )
        .expect("checkpoint 5");
    store
        .update_turn_state(&turn_5.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend 5");

    // 健康 sibling 2：blocked + 1 pending approval（正常待审批）。
    let thread_6 = store.create_thread(None, None).expect("thread 6");
    let turn_6 = store
        .create_turn(&thread_6.thread_id, "running")
        .expect("turn 6");
    let request_6 = ApprovalRequest::new(
        "approval_recovery_6",
        thread_6.thread_id.clone(),
        turn_6.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_6")
    .with_resources([workspace_resource("README.md")]);
    let checkpoint_6 = serde_json::json!({
        "request_id": &request_6.request_id,
        "thread_id": &request_6.thread_id,
        "turn_id": &request_6.turn_id,
        "tool_call_id": "call_6",
        "tool_name": "edit",
        "raw_arguments": "{}",
        "resources": &request_6.resources,
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
            &request_6,
            Some(checkpoint_6),
            "approval",
            "approval requested",
        )
        .expect("pending 6");
    store
        .update_turn_state(&turn_6.turn_id, TurnStatus::Blocked, "blocked")
        .expect("block 6");

    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    reopened
        .recover_unowned_workspace_executions()
        .expect("recover inconsistent ownerless turns");

    // 四个坏 turn 全部收敛为 Interrupted/interrupted。
    for turn in [&turn_1, &turn_2, &turn_3, &turn_4] {
        let recovered = reopened.get_turn(&turn.turn_id).expect("recovered turn");
        assert_eq!(recovered.status, TurnStatus::Interrupted);
        assert_eq!(recovered.agent_loop_status, "interrupted");
    }
    // 场景 3 的未解决 approval/pending 行已删除，不留下孤儿。
    assert!(
        !reopened
            .has_pending_tool_call(&request_3.request_id)
            .expect("pending 3 lookup")
    );
    // 场景 4 的 pending approval 行已删除。
    assert!(
        !reopened
            .has_pending_tool_call(&request_4.request_id)
            .expect("pending 4 lookup")
    );
    // 坏 turn 的未解决 approval 已删除，健康 blocked sibling 的 pending 保留。
    let pending_requests = reopened
        .list_pending_approvals()
        .expect("pending approvals");
    assert!(
        !pending_requests
            .iter()
            .any(|request| request.request_id == request_3.request_id)
    );
    assert!(
        !pending_requests
            .iter()
            .any(|request| request.request_id == request_4.request_id)
    );
    assert!(
        pending_requests
            .iter()
            .any(|request| request.request_id == request_6.request_id)
    );
    // unknown execution 保留为审计证据（不删除、不重放）。
    assert_eq!(
        reopened
            .get_tool_execution(&execution_1)
            .expect("execution lookup")
            .expect("unknown execution")
            .state,
        ToolExecutionState::Unknown
    );
    // typed trace 记录收敛原因与 previous 状态。
    let recovery_trace = reopened
        .list_trace(&thread_1.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|trace| {
            trace
                .payload
                .get("recovery_reason")
                .and_then(|value| value.as_str())
                == Some("inconsistent_turn_state")
        })
        .expect("recovery trace");
    assert_eq!(recovery_trace.payload["tool_replayed"], false);
    assert_eq!(recovery_trace.payload["previous_status"], "suspended");
    assert_eq!(
        recovery_trace.payload["previous_agent_loop_status"],
        "suspended"
    );
    // 健康 sibling 不受影响。
    let healthy_5 = reopened.get_turn(&turn_5.turn_id).expect("turn 5");
    assert_eq!(healthy_5.status, TurnStatus::Suspended);
    assert_eq!(healthy_5.agent_loop_status, "suspended");
    let healthy_6 = reopened.get_turn(&turn_6.turn_id).expect("turn 6");
    assert_eq!(healthy_6.status, TurnStatus::Blocked);
    assert_eq!(healthy_6.agent_loop_status, "blocked");
    assert!(
        reopened
            .has_pending_tool_call(&request_6.request_id)
            .expect("pending 6 lookup")
    );
}

#[test]
fn parallel_tool_result_checkpoint_clears_the_complete_batch_before_owner_recovery() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let pending_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "tool_calls_ready"
    });
    let executions = ["call_first", "call_second"].map(|tool_call_id| ToolExecution {
        execution_id: format!("turn:{}:tool:{tool_call_id}", turn.turn_id),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
        tool_call_id: tool_call_id.to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "tool_call", "tool_name": "read"}),
    });
    assert!(
        store
            .begin_tool_executions_at_checkpoint(&executions, &pending_checkpoint, 1)
            .expect("begin parallel read executions")
    );

    let committed_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "parallel_tool_results_committed",
        "tool_call_ids": ["call_first", "call_second"]
    });
    store
        .commit_tool_results_checkpoint(
            &executions
                .iter()
                .map(|execution| execution.execution_id.clone())
                .collect::<Vec<_>>(),
            &turn.turn_id,
            &thread.thread_id,
            &committed_checkpoint,
            1,
        )
        .expect("commit complete parallel result checkpoint");
    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    reopened
        .recover_unowned_workspace_executions()
        .expect("recover owner loss after complete batch");
    for execution in &executions {
        assert!(
            reopened
                .get_tool_execution(&execution.execution_id)
                .expect("execution lookup")
                .is_none(),
            "a checkpoint containing the complete batch must clear every execution owner"
        );
    }
    let (claimed, checkpoint) = reopened
        .claim_suspended_turn(&turn.turn_id)
        .expect("complete batch checkpoint remains resumable");
    assert_eq!(claimed.status, TurnStatus::Running);
    assert_eq!(checkpoint, committed_checkpoint);
}

#[test]
fn tool_result_checkpoint_rejects_an_incomplete_or_invalid_batch_without_writes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let pending_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "tool_calls_ready"
    });
    let executions = ["call_first", "call_second"].map(|tool_call_id| ToolExecution {
        execution_id: format!("turn:{}:tool:{tool_call_id}", turn.turn_id),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
        tool_call_id: tool_call_id.to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "tool_call", "tool_name": "read"}),
    });
    assert!(
        store
            .begin_tool_executions_at_checkpoint(&executions, &pending_checkpoint, 1)
            .expect("begin parallel read executions")
    );
    let committed_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "parallel_tool_results_committed"
    });
    let invalid_batches = [
        vec![executions[0].execution_id.clone()],
        vec![
            executions[0].execution_id.clone(),
            executions[0].execution_id.clone(),
        ],
        vec![
            executions[0].execution_id.clone(),
            "turn:unknown:tool:call_unknown".to_string(),
        ],
    ];

    for invalid_batch in invalid_batches {
        assert!(matches!(
            store.commit_tool_results_checkpoint(
                &invalid_batch,
                &turn.turn_id,
                &thread.thread_id,
                &committed_checkpoint,
                1,
            ),
            Err(StoreError::InvalidState(_))
        ));
        assert_eq!(
            store
                .get_turn_checkpoint(&turn.turn_id)
                .expect("checkpoint lookup")
                .expect("pending checkpoint"),
            pending_checkpoint
        );
        for execution in &executions {
            assert_eq!(
                store
                    .get_tool_execution(&execution.execution_id)
                    .expect("execution lookup")
                    .expect("running execution")
                    .state,
                ToolExecutionState::Running
            );
        }
    }
}

#[test]
fn terminal_turn_rejects_late_tool_execution_begin_without_writes() {
    let store = SessionStore::open(":memory:").expect("open store");
    let terminal_statuses = [
        (TurnStatus::Completed, "completed"),
        (TurnStatus::Failed, "failed"),
        (TurnStatus::Interrupted, "interrupted"),
        (TurnStatus::Paused, "paused"),
        (TurnStatus::Suspended, "suspended"),
    ];

    for (status, agent_loop_status) in terminal_statuses {
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        store
            .update_turn_state(&turn.turn_id, status.clone(), agent_loop_status)
            .expect("terminalize turn");
        let checkpoint = serde_json::json!({
            "checkpoint_version": 1,
            "boundary": "tool_calls_ready"
        });
        let execution = ToolExecution {
            execution_id: format!("turn:{}:tool:late", turn.turn_id),
            thread_id: thread.thread_id.clone(),
            turn_id: turn.turn_id.clone(),
            tool_call_id: "call_late".to_string(),
            state: ToolExecutionState::Running,
            payload: serde_json::json!({"kind": "tool_call", "tool_name": "read"}),
        };

        assert!(matches!(
            store.begin_tool_executions_at_checkpoint(
                std::slice::from_ref(&execution),
                &checkpoint,
                1,
            ),
            Err(StoreError::InvalidState(_))
        ));
        assert!(
            store
                .get_turn_checkpoint(&turn.turn_id)
                .expect("checkpoint lookup")
                .is_none()
        );
        assert!(
            store
                .get_tool_execution(&execution.execution_id)
                .expect("execution lookup")
                .is_none()
        );
        assert_eq!(
            store.get_turn(&turn.turn_id).expect("turn lookup").status,
            status
        );
    }
}

#[test]
fn blocked_begin_rejects_cross_thread_executing_pending_tool_call_without_writes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let other_thread = store.create_thread(None, None).expect("other thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_cross_thread_pending",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_cross_thread");
    store.create_approval(&request).expect("approval");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Blocked, "blocked")
        .expect("blocked turn");

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into pending_tool_calls(
                 request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) values(?1, ?2, ?3, ?4, ?5, 'executing')",
            rusqlite::params![
                request.request_id,
                other_thread.thread_id,
                turn.turn_id,
                "call_cross_thread",
                "{}"
            ],
        )
        .expect("cross-thread pending execution");
    drop(connection);

    let checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "tool_calls_ready"
    });
    let execution = ToolExecution {
        execution_id: format!("turn:{}:tool:cross_thread", turn.turn_id),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
        tool_call_id: "call_cross_thread".to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "tool_call", "tool_name": "edit"}),
    };

    assert!(matches!(
        store
            .begin_tool_executions_at_checkpoint(std::slice::from_ref(&execution), &checkpoint, 1,),
        Err(StoreError::InvalidState(_))
    ));
    assert!(
        store
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint lookup")
            .is_none()
    );
    assert!(
        store
            .get_tool_execution(&execution.execution_id)
            .expect("execution lookup")
            .is_none()
    );
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("turn lookup").status,
        TurnStatus::Blocked
    );
}

#[test]
fn terminal_outcomes_reconcile_running_tool_executions() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let prepare = |tool_call_id: &str| {
        let (turn, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                "running",
                serde_json::json!([{"type": "text", "text": "run"}]),
                "app_server",
                "turn started",
            )
            .expect("started turn");
        let checkpoint = serde_json::json!({
            "checkpoint_version": 1,
            "boundary": "tool_calls_ready",
            "pending_tool_calls": [{"tool_call_id": tool_call_id}]
        });
        let execution = ToolExecution {
            execution_id: format!("turn:{}:tool:{tool_call_id}", turn.turn_id),
            thread_id: thread.thread_id.clone(),
            turn_id: turn.turn_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            state: ToolExecutionState::Running,
            payload: serde_json::json!({"kind": "tool_call", "tool_name": "command"}),
        };
        assert!(
            store
                .begin_tool_executions_at_checkpoint(
                    std::slice::from_ref(&execution),
                    &checkpoint,
                    1,
                )
                .expect("running execution")
        );
        (turn, execution, checkpoint)
    };

    let (failed_turn, failed_execution, failed_checkpoint) = prepare("call_failed");
    let failed = store
        .commit_turn_outcome(
            &failed_turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &TraceEvent::for_turn(
                    "trace_failed_with_execution",
                    &thread.thread_id,
                    &failed_turn.turn_id,
                    "agent_loop",
                    "tool execution failed",
                ),
            },
        )
        .expect("failed outcome");
    assert_eq!(failed.turn.status, TurnStatus::Failed);
    assert_eq!(
        store
            .get_tool_execution(&failed_execution.execution_id)
            .expect("failed execution lookup")
            .expect("failed execution")
            .state,
        ToolExecutionState::Unknown
    );
    assert_eq!(
        store
            .get_turn_checkpoint(&failed_turn.turn_id)
            .expect("failed checkpoint lookup")
            .expect("failed checkpoint"),
        failed_checkpoint
    );

    let (interrupted_turn, interrupted_execution, interrupted_checkpoint) =
        prepare("call_cancelled");
    let interrupted = store
        .commit_turn_outcome(
            &interrupted_turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "cancelled",
                assistant_item_id: None,
                assistant_delta: None,
                trace: &TraceEvent::for_turn(
                    "trace_interrupted_with_execution",
                    &thread.thread_id,
                    &interrupted_turn.turn_id,
                    "agent_loop",
                    "tool execution cancelled",
                ),
            },
        )
        .expect("interrupted outcome");
    assert_eq!(interrupted.turn.status, TurnStatus::Interrupted);
    assert_eq!(
        store
            .get_tool_execution(&interrupted_execution.execution_id)
            .expect("interrupted execution lookup")
            .expect("interrupted execution")
            .state,
        ToolExecutionState::Unknown
    );
    assert_eq!(
        store
            .get_turn_checkpoint(&interrupted_turn.turn_id)
            .expect("interrupted checkpoint lookup")
            .expect("interrupted checkpoint"),
        interrupted_checkpoint
    );

    let (completed_turn, completed_execution, completed_checkpoint) = prepare("call_completed");
    let assistant_item_id = SessionStore::allocate_assistant_item_id();
    assert!(matches!(
        store.commit_turn_outcome(
            &completed_turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("done"),
                trace: &TraceEvent::for_turn(
                    "trace_completed_with_execution",
                    &thread.thread_id,
                    &completed_turn.turn_id,
                    "agent_loop",
                    "completion attempted",
                ),
            },
        ),
        Err(StoreError::InvalidState(message))
            if message == "completed turn outcome cannot commit with running tool execution"
    ));
    assert_eq!(
        store
            .get_turn(&completed_turn.turn_id)
            .expect("completed turn lookup")
            .status,
        TurnStatus::Running
    );
    assert_eq!(
        store
            .get_tool_execution(&completed_execution.execution_id)
            .expect("completed execution lookup")
            .expect("completed execution")
            .state,
        ToolExecutionState::Running
    );
    assert_eq!(
        store
            .get_turn_checkpoint(&completed_turn.turn_id)
            .expect("completed checkpoint lookup")
            .expect("completed checkpoint"),
        completed_checkpoint
    );
}

#[test]
fn turn_checkpoint_commit_is_atomic_and_unknown_execution_blocks_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let initial_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "initial"
    });
    store
        .save_turn_checkpoint(&turn.turn_id, &thread.thread_id, &initial_checkpoint, 1)
        .expect("initial checkpoint");
    let execution_id = format!("turn:{}:tool:call_completed", turn.turn_id);
    let execution = ToolExecution {
        execution_id: execution_id.clone(),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
        tool_call_id: "call_completed".to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "tool_call", "tool_name": "update_plan"}),
    };
    assert!(
        store
            .begin_tool_executions_at_checkpoint(
                std::slice::from_ref(&execution),
                &initial_checkpoint,
                1,
            )
            .expect("running execution")
    );
    let committed_checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "tool_result_committed"
    });
    store
        .commit_tool_results_checkpoint(
            std::slice::from_ref(&execution_id),
            &turn.turn_id,
            &thread.thread_id,
            &committed_checkpoint,
            1,
        )
        .expect("atomic tool result checkpoint");
    assert_eq!(
        store
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint")
            .expect("checkpoint row"),
        committed_checkpoint
    );
    assert!(
        store
            .get_tool_execution(&execution_id)
            .expect("execution lookup")
            .is_none()
    );

    // A safe checkpoint with no active or unknown external execution is resumable and recovery is
    // idempotent; it must not require a synthetic owner sentinel.
    store
        .recover_unowned_workspace_executions()
        .expect("safe checkpoint recovery");
    let suspended = store.get_turn(&turn.turn_id).expect("suspended turn");
    assert_eq!(suspended.status, TurnStatus::Suspended);
    assert_eq!(suspended.agent_loop_status, "suspended");
    let (claimed, claimed_checkpoint) = store
        .claim_suspended_turn(&turn.turn_id)
        .expect("safe resume claim");
    assert_eq!(claimed.status, TurnStatus::Running);
    assert_eq!(claimed_checkpoint, committed_checkpoint);
    let suspended_after_failure = store
        .suspend_claimed_turn_after_failure(&turn.turn_id)
        .expect("release failed resume claim");
    assert_eq!(suspended_after_failure.status, TurnStatus::Suspended);
    let (reclaimed, reclaimed_checkpoint) = store
        .claim_suspended_turn(&turn.turn_id)
        .expect("retry released resume claim");
    assert_eq!(reclaimed.status, TurnStatus::Running);
    assert_eq!(reclaimed_checkpoint, committed_checkpoint);

    // An owner lost while a real tool is in flight remains terminally Unknown and cannot be
    // retried by an explicit resume.
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Failed, "failed")
        .expect("finish first turn");
    let unknown_turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("unknown turn");
    store
        .save_turn_checkpoint(
            &unknown_turn.turn_id,
            &thread.thread_id,
            &initial_checkpoint,
            1,
        )
        .expect("unknown checkpoint");
    let unknown_execution_id = format!("turn:{}:tool:call_unknown", unknown_turn.turn_id);
    let unknown_execution = ToolExecution {
        execution_id: unknown_execution_id.clone(),
        thread_id: thread.thread_id.clone(),
        turn_id: unknown_turn.turn_id.clone(),
        tool_call_id: "call_unknown".to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "tool_call", "tool_name": "edit"}),
    };
    assert!(
        store
            .begin_tool_executions_at_checkpoint(
                std::slice::from_ref(&unknown_execution),
                &initial_checkpoint,
                1,
            )
            .expect("unknown running execution")
    );
    drop(store);

    let reopened = SessionStore::open(&db_path).expect("reopen store");
    reopened
        .recover_unowned_workspace_executions()
        .expect("unknown execution recovery");
    assert_eq!(
        reopened
            .get_tool_execution(&unknown_execution_id)
            .expect("unknown lookup")
            .expect("unknown execution")
            .state,
        ToolExecutionState::Unknown
    );
    assert!(matches!(
        reopened.claim_suspended_turn(&unknown_turn.turn_id),
        Err(StoreError::InvalidState(message))
            if message == "turn has unknown tool execution and cannot be resumed"
    ));
}

#[test]
fn turn_checkpoint_provider_reasoning_payload_survives_store_reopen_and_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let checkpoint = serde_json::json!({
        "checkpoint_version": 5,
        "thread_id": thread.thread_id,
        "turn_id": turn.turn_id,
        "provider_reasoning_history": [{
            "kind": "responses",
            "provider_name": "deepseek",
            "model_name": "deepseek-reasoner",
            "reasoning_effort": "high",
            "tool_call_ids": ["call_1"],
            "item": {
                "type": "reasoning",
                "id": "rs_opaque",
                "encrypted_content": "opaque-provider-state"
            }
        }]
    });
    store
        .save_turn_checkpoint(&turn.turn_id, &thread.thread_id, &checkpoint, 5)
        .expect("save checkpoint");
    drop(store);

    // Reopening the SQLite store models a process restart. The payload remains opaque to Store,
    // but its exact bytes must be available to the typed Agent checkpoint decoder on resume.
    let reopened = SessionStore::open(&db_path).expect("reopen store");
    assert_eq!(
        reopened
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint lookup")
            .expect("checkpoint row"),
        checkpoint
    );
    reopened
        .recover_unowned_workspace_executions()
        .expect("suspend running turn after owner loss");
    let (_claimed, resumed_checkpoint) = reopened
        .claim_suspended_turn(&turn.turn_id)
        .expect("claim resumable turn");
    assert_eq!(resumed_checkpoint, checkpoint);
}

// 两个独立连接并发恢复同一安全 checkpoint 时，Store CAS 只能交给一个 owner。
#[test]
fn suspended_turn_claim_allows_only_one_owner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let checkpoint = serde_json::json!({
        "checkpoint_version": 1,
        "boundary": "safe"
    });
    store
        .save_turn_checkpoint(&turn.turn_id, &thread.thread_id, &checkpoint, 1)
        .expect("checkpoint");
    store
        .recover_unowned_workspace_executions()
        .expect("suspend turn");
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("suspended").status,
        TurnStatus::Suspended
    );
    drop(store);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = db_path.clone();
        let turn_id = turn.turn_id.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let store = SessionStore::open(path).expect("open concurrent store");
            barrier.wait();
            store.claim_suspended_turn(&turn_id)
        }));
    }

    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("claim thread"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(StoreError::InvalidState(_))))
            .count(),
        1
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
                assistant_item_id: None,
                assistant_delta: None,
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
                assistant_item_id: None,
                assistant_delta: None,
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
    store
        .record_approval_decision(
            &ApprovalDecision::new(next.request_id.clone(), ApprovalOutcome::Allow, "allow"),
            "approval",
            "approval decision recorded",
        )
        .expect("successor approval has a typed wait span");
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
        .record_approval_decision_with_first_attempt_failure(
            &decision,
            "approval",
            "approval decision recorded",
            true,
        )
        .expect("deny approval");

    let denied_turn = store.get_turn(&turn.turn_id).expect("denied turn");
    assert_eq!(denied_turn.status, TurnStatus::Failed);
    assert_eq!(denied_turn.agent_loop_status, "failed");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
    let denial_samples = store
        .list_trace(&thread.thread_id)
        .expect("trace")
        .iter()
        .flat_map(|event| event.metric_samples.iter())
        .filter(|sample| sample.kind == TraceMetricSampleKind::ToolFirstAttemptFailure)
        .count();
    assert_eq!(denial_samples, 1);
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
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, item, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "blocked",
            serde_json::json!([{"type": "text", "text": "delete me"}]),
            "test",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Blocked, "blocked")
        .expect("blocked turn");
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

    // Populate every recovery table with a real bound row before deletion.
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into turn_inputs(input_id, turn_id, item_id, delivery, delivery_state, consumed_at)
             values(?1, ?2, ?3, 'steer', 'consumed', current_timestamp)",
            rusqlite::params!["input_delete", turn.turn_id, item.item_id],
        )
        .expect("turn input");
    connection
        .execute(
            "insert into tool_executions(execution_id, thread_id, turn_id, tool_call_id, execution_state, payload)
             values(?1, ?2, ?3, 'call_delete', 'unknown', '{}')",
            rusqlite::params![
                "execution_delete",
                thread.thread_id,
                turn.turn_id
            ],
        )
        .expect("tool execution");
    connection
        .execute(
            "insert into turn_checkpoints(turn_id, thread_id, payload, checkpoint_version)
             values(?1, ?2, '{\"checkpoint_version\":1}', 1)",
            rusqlite::params![turn.turn_id, thread.thread_id],
        )
        .expect("turn checkpoint");
    for (table, count) in [
        ("turn_inputs", 1_i64),
        ("tool_executions", 1_i64),
        ("turn_checkpoints", 1_i64),
    ] {
        let actual: i64 = connection
            .query_row(
                &format!("select count(*) from {table} where turn_id = ?1"),
                [&turn.turn_id],
                |row| row.get(0),
            )
            .expect("recovery row count");
        assert_eq!(actual, count, "{table} fixture");
    }
    drop(connection);

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

    let connection = rusqlite::Connection::open(&db_path).expect("reopen sqlite");
    for table in [
        "approval_decisions",
        "pending_tool_calls",
        "turn_inputs",
        "tool_executions",
        "turn_checkpoints",
        "approvals",
        "items",
        "trace_events",
        "artifact_refs",
        "turns",
        "threads",
    ] {
        let count: i64 = connection
            .query_row(&format!("select count(*) from {table}"), [], |row| {
                row.get(0)
            })
            .expect("deleted table count");
        assert_eq!(count, 0, "{table} rows remain after thread deletion");
    }
}

#[test]
fn malformed_checkpoint_terminalization_marks_unknown_and_never_replays() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "recover safely"}]),
            "test",
            "turn started",
        )
        .expect("turn");
    let checkpoint = serde_json::json!({"checkpoint_version": 1, "pending": "opaque"});
    store
        .save_turn_checkpoint(&turn.turn_id, &thread.thread_id, &checkpoint, 1)
        .expect("checkpoint");
    let execution = ToolExecution {
        execution_id: format!("turn:{}:tool:call_recovery", turn.turn_id),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
        tool_call_id: "call_recovery".to_string(),
        state: ToolExecutionState::Running,
        payload: serde_json::json!({"kind": "command"}),
    };
    assert!(
        store
            .begin_tool_executions_at_checkpoint(std::slice::from_ref(&execution), &checkpoint, 1)
            .expect("claim execution")
    );

    let failed = store
        .terminalize_checkpoint_failure(
            &thread.thread_id,
            &turn.turn_id,
            TurnStatus::Running,
            "running",
            None,
        )
        .expect("terminalize malformed checkpoint");
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(failed.agent_loop_status, "failed");
    assert_eq!(
        store
            .get_tool_execution(&execution.execution_id)
            .expect("execution lookup")
            .expect("execution retained")
            .state,
        ToolExecutionState::Unknown
    );
    assert_eq!(
        store
            .get_turn_checkpoint(&turn.turn_id)
            .expect("checkpoint lookup"),
        Some(checkpoint)
    );
    let trace = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|event| event.payload["failure_kind"] == "checkpoint_decode_failed")
        .expect("typed checkpoint failure trace");
    assert_eq!(trace.payload["tool_replayed"], false);

    assert!(matches!(
        store.terminalize_checkpoint_failure(
            &thread.thread_id,
            &turn.turn_id,
            TurnStatus::Running,
            "running",
            None,
        ),
        Err(StoreError::InvalidState(message))
            if message.contains("owner/status changed")
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
    let assistant_item_id = SessionStore::allocate_assistant_item_id();

    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("assistant"),
                trace: &trace,
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
                    trace: &TraceEvent::for_turn(
                        format!("trace_pairing_{}", turn.turn_id),
                        &thread.thread_id,
                        &turn.turn_id,
                        "test",
                        "terminal pairing",
                    ),
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
    let commit = |turn: &singularity_protocol::Turn, thread_id: &str, trace_id: &str| {
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&item_id),
                assistant_delta: Some("assistant"),
                trace: &TraceEvent::for_turn(
                    trace_id,
                    thread_id,
                    &turn.turn_id,
                    "test",
                    "terminal outcome",
                ),
            },
        )
    };

    commit(&first_turn, &first_thread.thread_id, "trace_first").expect("first commit");
    let error = commit(&second_turn, &second_thread.thread_id, "trace_second")
        .expect_err("duplicate item ID must fail closed");

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
            assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
            assistant_delta: Some("assistant"),
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
                assistant_item_id: None,
                assistant_delta: None,
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
                assistant_item_id: None,
                assistant_delta: None,
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
    assert_ne!(first.payload_hash, second.payload_hash);
    let serialized = serde_json::to_string(&first).expect("serialize trace");
    assert!(!serialized.contains("sentinel-secret-value"));
}

#[test]
fn trace_storage_preserves_stable_provider_codes_and_redacts_invalid_codes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let identity = || TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiChatCompletions),
        operation_phase: Some(TraceProviderOperationPhase::Completion),
        attempt_index: Some(1),
        retry_count: Some(0),
        ..TraceSpanProjection::default()
    };
    let append_pair = |prefix: &str, code: &str| {
        let mut start = provider_span(&format!("{prefix}_start"), TraceSpanPhase::Start);
        start.span_id = Some(prefix.to_string());
        start.span_projection = Some(identity());

        let mut end = provider_span(&format!("{prefix}_end"), TraceSpanPhase::End);
        end.span_id = Some(prefix.to_string());
        end.span_status = Some(TraceSpanStatus::Error);
        let mut projection = identity();
        projection.error = Some(TraceErrorProjection {
            category: TraceErrorCategory::JsonSchema,
            stage: Some(TraceErrorStage::ResponseValidation),
            code: Some(code.to_string()),
        });
        end.span_projection = Some(projection);

        store.append_trace(&start).expect("provider span start");
        store.append_trace(&end).expect("provider span end");
    };

    append_pair("stable_code", "provider_response_invalid");
    append_pair("invalid_code", "provider response invalid");
    append_pair("secret_code", "sk-abcdefgh");
    append_pair(
        "jwt_code",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature",
    );

    let stable = store.show_trace("stable_code_end").expect("stable trace");
    assert_eq!(
        stable
            .span_projection
            .and_then(|projection| projection.error)
            .and_then(|error| error.code)
            .as_deref(),
        Some("provider_response_invalid")
    );

    let invalid = store.show_trace("invalid_code_end").expect("invalid trace");
    assert_eq!(
        invalid
            .span_projection
            .and_then(|projection| projection.error)
            .and_then(|error| error.code)
            .as_deref(),
        Some("[redacted]")
    );

    for event_id in ["secret_code_end", "jwt_code_end"] {
        let redacted = store.show_trace(event_id).expect("secret trace");
        assert_eq!(
            redacted
                .span_projection
                .and_then(|projection| projection.error)
                .and_then(|error| error.code)
                .as_deref(),
            Some("[redacted]"),
            "{event_id} must not persist a credential-shaped code"
        );
    }
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

fn provider_span(event_id: &str, phase: TraceSpanPhase) -> TraceEvent {
    let mut event = TraceEvent::new(event_id, "run_span", "session_span", "provider", "span");
    event.span_id = Some("provider_span".to_string());
    event.span_kind = Some(TraceSpanKind::ProviderAttempt);
    event.span_phase = Some(phase);
    if phase == TraceSpanPhase::End {
        event.span_status = Some(TraceSpanStatus::Ok);
        event.duration_ms = Some(20);
        event.time_to_first_token_ms = Some(8);
    }
    event
}

#[test]
fn typed_trace_span_lifecycle_rejects_duplicates_and_cross_run_parents() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    store
        .append_trace(&provider_span("span_start", TraceSpanPhase::Start))
        .expect("span start");
    assert!(
        store
            .append_trace(&provider_span(
                "span_start_duplicate",
                TraceSpanPhase::Start
            ))
            .is_err()
    );
    store
        .append_trace(&provider_span("span_end", TraceSpanPhase::End))
        .expect("span end");
    assert!(
        store
            .append_trace(&provider_span("span_end_duplicate", TraceSpanPhase::End))
            .is_err()
    );

    let mut cross_run = TraceEvent::new(
        "cross_run_parent",
        "another_run",
        "another_session",
        "provider",
        "cross run",
    );
    cross_run.span_id = Some("child".to_string());
    cross_run.parent_span_id = Some("provider_span".to_string());
    cross_run.span_kind = Some(TraceSpanKind::ProviderAttempt);
    cross_run.span_phase = Some(TraceSpanPhase::Start);
    assert!(store.append_trace(&cross_run).is_err());
}

#[test]
fn prompt_assembly_start_end_contract_is_closed_and_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let make_pair = |span_id: &str,
                     start_projection: Option<TraceSpanProjection>,
                     end_projection: TraceSpanProjection| {
        let mut start = TraceEvent::new(
            format!("{span_id}_start"),
            "prompt_run",
            "prompt_session",
            "agent",
            "prompt",
        );
        start.span_id = Some(span_id.to_string());
        start.span_kind = Some(TraceSpanKind::PromptAssembly);
        start.span_phase = Some(TraceSpanPhase::Start);
        start.span_projection = start_projection;

        let mut end = TraceEvent::new(
            format!("{span_id}_end"),
            "prompt_run",
            "prompt_session",
            "agent",
            "prompt",
        );
        end.span_id = Some(span_id.to_string());
        end.span_kind = Some(TraceSpanKind::PromptAssembly);
        end.span_phase = Some(TraceSpanPhase::End);
        end.span_status = Some(TraceSpanStatus::Ok);
        end.duration_ms = Some(4);
        end.span_projection = Some(end_projection);
        (start, end)
    };

    let terminal_projection = || TraceSpanProjection {
        message_count: Some(3),
        tool_count: Some(1),
        request_token_count: Some(8),
        request_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        compacted: Some(true),
        finalization_only: Some(false),
        model_turn_ordinal: Some(2),
        ..TraceSpanProjection::default()
    };

    let (unknown_start, real_end) = make_pair("unknown", None, terminal_projection());
    store
        .append_trace_batch(&[unknown_start, real_end])
        .expect("unknown start and real end");

    let (terminal_start, terminal_end) = make_pair(
        "terminal_start",
        Some(TraceSpanProjection {
            message_count: Some(3),
            ..TraceSpanProjection::default()
        }),
        terminal_projection(),
    );
    assert!(
        store
            .append_trace_batch(&[terminal_start, terminal_end])
            .is_err()
    );

    let (empty_digest_start, mut empty_digest_end) =
        make_pair("empty_digest", None, terminal_projection());
    empty_digest_end
        .span_projection
        .as_mut()
        .expect("end projection")
        .request_digest = Some(String::new());
    assert!(
        store
            .append_trace_batch(&[empty_digest_start, empty_digest_end])
            .is_err()
    );

    let (stable_start, stable_end) = make_pair(
        "stable_mismatch",
        Some(TraceSpanProjection {
            finalization_only: Some(false),
            model_turn_ordinal: Some(1),
            ..TraceSpanProjection::default()
        }),
        TraceSpanProjection {
            finalization_only: Some(false),
            model_turn_ordinal: Some(2),
            ..TraceSpanProjection::default()
        },
    );
    assert!(
        store
            .append_trace_batch(&[stable_start, stable_end])
            .is_err()
    );
}

#[test]
fn typed_trace_span_identity_rejects_every_stable_mismatch_before_insert() {
    let assert_rejected = |label: &str,
                           kind: TraceSpanKind,
                           start_projection: TraceSpanProjection,
                           end_projection: TraceSpanProjection| {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let run_id = format!("identity_{label}");
        let mut start = TraceEvent::new(
            format!("{label}_start"),
            run_id.clone(),
            format!("{label}_session"),
            "identity",
            "start",
        );
        start.span_id = Some(format!("{label}_span"));
        start.span_kind = Some(kind);
        start.span_phase = Some(TraceSpanPhase::Start);
        start.span_projection = Some(start_projection);

        let mut end = TraceEvent::new(
            format!("{label}_end"),
            run_id.clone(),
            format!("{label}_session"),
            "identity",
            "end",
        );
        end.span_id = Some(format!("{label}_span"));
        end.span_kind = Some(kind);
        end.span_phase = Some(TraceSpanPhase::End);
        end.span_status = Some(TraceSpanStatus::Ok);
        end.duration_ms = Some(1);
        end.span_projection = Some(end_projection);

        assert!(
            store.append_trace_batch(&[start, end]).is_err(),
            "accepted stable identity mismatch for {label}"
        );
        assert!(matches!(
            store.list_trace(&run_id),
            Err(StoreError::NotFound(message)) if message == format!("trace run {run_id}")
        ));
    };

    let prompt = |model_turn_ordinal, finalization_only| TraceSpanProjection {
        model_turn_ordinal: Some(model_turn_ordinal),
        finalization_only: Some(finalization_only),
        ..TraceSpanProjection::default()
    };
    assert_rejected(
        "prompt_model_turn",
        TraceSpanKind::PromptAssembly,
        prompt(1, false),
        prompt(2, false),
    );
    assert_rejected(
        "prompt_finalization",
        TraceSpanKind::PromptAssembly,
        prompt(1, false),
        prompt(1, true),
    );

    let provider = || TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        operation_phase: Some(TraceProviderOperationPhase::Completion),
        attempt_index: Some(1),
        retry_count: Some(0),
        ..TraceSpanProjection::default()
    };
    let mut end = provider();
    end.provider_name = Some("other_provider".to_string());
    assert_rejected(
        "provider_name",
        TraceSpanKind::ProviderAttempt,
        provider(),
        end,
    );
    let mut end = provider();
    end.model_name = Some("other_model".to_string());
    assert_rejected(
        "model_name",
        TraceSpanKind::ProviderAttempt,
        provider(),
        end,
    );
    let mut end = provider();
    end.protocol = Some(TraceProviderProtocol::OpenAiChatCompletions);
    assert_rejected("protocol", TraceSpanKind::ProviderAttempt, provider(), end);
    let mut end = provider();
    end.operation_phase = Some(TraceProviderOperationPhase::CapabilityProbe);
    assert_rejected(
        "operation_phase",
        TraceSpanKind::ProviderAttempt,
        provider(),
        end,
    );
    let mut end = provider();
    end.attempt_index = Some(2);
    assert_rejected(
        "attempt_index",
        TraceSpanKind::ProviderAttempt,
        provider(),
        end,
    );
    let mut end = provider();
    end.retry_count = Some(1);
    assert_rejected(
        "retry_count",
        TraceSpanKind::ProviderAttempt,
        provider(),
        end,
    );

    let tool = || TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection {
            tool_name: Some("command".to_string()),
            tool_call_id_digest: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            tool_call_ordinal: Some(1),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = tool();
    end.model_turn_ordinal = Some(3);
    assert_rejected("tool_model_turn", TraceSpanKind::ToolCall, tool(), end);
    let mut end = tool();
    end.tool.as_mut().expect("tool identity").tool_name = Some("edit".to_string());
    assert_rejected("tool_name", TraceSpanKind::ToolCall, tool(), end);
    let mut end = tool();
    end.tool
        .as_mut()
        .expect("tool identity")
        .tool_call_id_digest =
        Some("sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string());
    assert_rejected("tool_call_id_digest", TraceSpanKind::ToolCall, tool(), end);
    let mut end = tool();
    end.tool.as_mut().expect("tool identity").tool_call_ordinal = Some(2);
    assert_rejected("tool_call_ordinal", TraceSpanKind::ToolCall, tool(), end);

    let policy = || TraceSpanProjection {
        policy: Some(TracePolicyProjection {
            operation_count: Some(1),
            resource_count: Some(2),
            ..TracePolicyProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = policy();
    end.policy
        .as_mut()
        .expect("policy identity")
        .operation_count = Some(2);
    assert_rejected(
        "policy_operation_count",
        TraceSpanKind::PolicyDecision,
        policy(),
        end,
    );
    let mut end = policy();
    end.policy.as_mut().expect("policy identity").resource_count = Some(3);
    assert_rejected(
        "policy_resource_count",
        TraceSpanKind::PolicyDecision,
        policy(),
        end,
    );

    let approval = || TraceSpanProjection {
        approval: Some(TraceApprovalProjection {
            request_count: Some(1),
            ..TraceApprovalProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = approval();
    end.approval
        .as_mut()
        .expect("approval identity")
        .request_count = Some(2);
    assert_rejected(
        "approval_request_count",
        TraceSpanKind::ApprovalWait,
        approval(),
        end,
    );

    let sandbox = || TraceSpanProjection {
        sandbox: Some(TraceSandboxProjection {
            command_id_digest: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            command_id_binding_valid: Some(true),
            ..TraceSandboxProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = sandbox();
    end.sandbox
        .as_mut()
        .expect("sandbox identity")
        .command_id_digest =
        Some("sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string());
    assert_rejected(
        "sandbox_command_id_digest",
        TraceSpanKind::SandboxExecution,
        sandbox(),
        end,
    );
    let mut end = sandbox();
    end.sandbox
        .as_mut()
        .expect("sandbox identity")
        .command_id_binding_valid = Some(false);
    assert_rejected(
        "sandbox_command_binding",
        TraceSpanKind::SandboxExecution,
        sandbox(),
        end,
    );

    let verification = || TraceSpanProjection {
        verification: Some(TraceVerificationProjection {
            required_command_count: Some(1),
            occurrence_count: Some(1),
            ..TraceVerificationProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = verification();
    end.verification
        .as_mut()
        .expect("verification identity")
        .required_command_count = Some(2);
    assert_rejected(
        "verification_required_count",
        TraceSpanKind::Verification,
        verification(),
        end,
    );
    let mut end = verification();
    end.verification
        .as_mut()
        .expect("verification identity")
        .occurrence_count = Some(2);
    assert_rejected(
        "verification_occurrence_count",
        TraceSpanKind::Verification,
        verification(),
        end,
    );

    let final_review = || TraceSpanProjection {
        final_review: Some(TraceFinalReviewProjection {
            model_turn_ordinal: Some(2),
            ..TraceFinalReviewProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    let mut end = final_review();
    end.final_review
        .as_mut()
        .expect("review identity")
        .model_turn_ordinal = Some(3);
    assert_rejected(
        "final_review_model_turn",
        TraceSpanKind::FinalReview,
        final_review(),
        end,
    );
}

#[test]
fn prompt_identity_rejects_known_start_with_missing_end_before_insert() {
    let assert_rejected = |label: &str, start_projection: TraceSpanProjection| {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let run_id = format!("prompt_identity_{label}");
        let mut start = TraceEvent::new(
            format!("{label}_start"),
            run_id.clone(),
            format!("{label}_session"),
            "identity",
            "start",
        );
        start.span_id = Some(format!("{label}_span"));
        start.span_kind = Some(TraceSpanKind::PromptAssembly);
        start.span_phase = Some(TraceSpanPhase::Start);
        start.span_projection = Some(start_projection);

        let mut end = TraceEvent::new(
            format!("{label}_end"),
            run_id.clone(),
            format!("{label}_session"),
            "identity",
            "end",
        );
        end.span_id = Some(format!("{label}_span"));
        end.span_kind = Some(TraceSpanKind::PromptAssembly);
        end.span_phase = Some(TraceSpanPhase::End);
        end.span_status = Some(TraceSpanStatus::Ok);
        end.duration_ms = Some(1);
        end.span_projection = Some(TraceSpanProjection::default());

        assert!(store.append_trace_batch(&[start, end]).is_err());
        assert!(matches!(
            store.list_trace(&run_id),
            Err(StoreError::NotFound(message)) if message == format!("trace run {run_id}")
        ));
    };

    assert_rejected(
        "model_turn_ordinal",
        TraceSpanProjection {
            model_turn_ordinal: Some(2),
            ..TraceSpanProjection::default()
        },
    );
    assert_rejected(
        "finalization_only",
        TraceSpanProjection {
            finalization_only: Some(false),
            ..TraceSpanProjection::default()
        },
    );
}

#[test]
fn typed_trace_span_identity_ignores_terminal_results_and_allows_prompt_unknown_start() {
    let assert_accepted = |label: &str,
                           kind: TraceSpanKind,
                           start_projection: Option<TraceSpanProjection>,
                           end_projection: TraceSpanProjection| {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let run_id = format!("terminal_identity_{label}");
        let mut start = TraceEvent::new(
            format!("{label}_start"),
            run_id.clone(),
            format!("{label}_session"),
            "identity",
            "start",
        );
        start.span_id = Some(format!("{label}_span"));
        start.span_kind = Some(kind);
        start.span_phase = Some(TraceSpanPhase::Start);
        start.span_projection = start_projection;

        let mut end = TraceEvent::new(
            format!("{label}_end"),
            run_id,
            format!("{label}_session"),
            "identity",
            "end",
        );
        end.span_id = Some(format!("{label}_span"));
        end.span_kind = Some(kind);
        end.span_phase = Some(TraceSpanPhase::End);
        end.span_status = Some(TraceSpanStatus::Ok);
        end.duration_ms = Some(1);
        end.span_projection = Some(end_projection);
        store
            .append_trace_batch(&[start, end])
            .unwrap_or_else(|error| panic!("terminal fields rejected for {label}: {error}"));
    };

    assert_accepted(
        "prompt",
        TraceSpanKind::PromptAssembly,
        None,
        TraceSpanProjection {
            operation_count: Some(1),
            message_count: Some(2),
            tool_count: Some(1),
            request_token_count: Some(3),
            request_digest: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            compacted: Some(true),
            finalization_only: Some(false),
            model_turn_ordinal: Some(2),
            ..TraceSpanProjection::default()
        },
    );

    let provider = || TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        operation_phase: Some(TraceProviderOperationPhase::Completion),
        attempt_index: Some(1),
        retry_count: Some(0),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "provider",
        TraceSpanKind::ProviderAttempt,
        Some(provider()),
        TraceSpanProjection {
            queue_duration_ms: Some(2),
            request_send_to_headers_ms: Some(3),
            retry_backoff_ms: Some(4),
            usage: Some(TraceUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_input_tokens: 1,
                reasoning_tokens: 0,
            }),
            error: Some(TraceErrorProjection {
                category: TraceErrorCategory::Network,
                stage: Some(TraceErrorStage::ResponseStatus),
                code: Some("provider_unavailable".to_string()),
            }),
            ..provider()
        },
    );

    let tool = || TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection {
            tool_name: Some("command".to_string()),
            tool_call_id_digest: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            tool_call_ordinal: Some(1),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "tool",
        TraceSpanKind::ToolCall,
        Some(tool()),
        TraceSpanProjection {
            tool: Some(TraceToolProjection {
                status: Some(TraceToolStatus::Failed),
                ..tool().tool.expect("tool identity")
            }),
            ..tool()
        },
    );

    let policy = || TraceSpanProjection {
        policy: Some(TracePolicyProjection {
            operation_count: Some(1),
            resource_count: Some(2),
            ..TracePolicyProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "policy",
        TraceSpanKind::PolicyDecision,
        Some(policy()),
        TraceSpanProjection {
            policy: Some(TracePolicyProjection {
                decision: Some(TracePolicyDecision::Deny),
                cause: Some(TracePolicyCause::Explicit),
                ..policy().policy.expect("policy identity")
            }),
            ..policy()
        },
    );

    let approval = || TraceSpanProjection {
        approval: Some(TraceApprovalProjection {
            request_count: Some(1),
            ..TraceApprovalProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "approval",
        TraceSpanKind::ApprovalWait,
        Some(approval()),
        TraceSpanProjection {
            approval: Some(TraceApprovalProjection {
                outcome: Some(TraceApprovalOutcome::Deny),
                ..approval().approval.expect("approval identity")
            }),
            ..approval()
        },
    );

    let sandbox = || TraceSpanProjection {
        sandbox: Some(TraceSandboxProjection {
            command_id_digest: Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            ),
            command_id_binding_valid: Some(true),
            ..TraceSandboxProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "sandbox",
        TraceSpanKind::SandboxExecution,
        Some(sandbox()),
        TraceSpanProjection {
            sandbox: Some(TraceSandboxProjection {
                status: Some(TraceSandboxStatus::Error),
                workspace_mutation: Some(TraceWorkspaceMutation::Changed),
                enforcement: Some(TraceSandboxEnforcement::Strict),
                ..sandbox().sandbox.expect("sandbox identity")
            }),
            ..sandbox()
        },
    );

    let verification = || TraceSpanProjection {
        verification: Some(TraceVerificationProjection {
            required_command_count: Some(1),
            satisfied_command_count: Some(0),
            occurrence_count: Some(1),
            ..TraceVerificationProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "verification",
        TraceSpanKind::Verification,
        Some(verification()),
        TraceSpanProjection {
            verification: Some(TraceVerificationProjection {
                satisfied_command_count: Some(1),
                status: Some(TraceVerificationStatus::CommandPassed),
                command_duration_ms: Some(5),
                ..verification().verification.expect("verification identity")
            }),
            ..verification()
        },
    );

    let final_review = || TraceSpanProjection {
        final_review: Some(TraceFinalReviewProjection {
            model_turn_ordinal: Some(2),
            ..TraceFinalReviewProjection::default()
        }),
        ..TraceSpanProjection::default()
    };
    assert_accepted(
        "final_review",
        TraceSpanKind::FinalReview,
        Some(final_review()),
        TraceSpanProjection {
            final_review: Some(TraceFinalReviewProjection {
                status: Some(TraceFinalReviewStatus::Failed),
                ..final_review().final_review.expect("review identity")
            }),
            ..final_review()
        },
    );
}

#[test]
fn append_trace_batch_validates_every_event_before_any_insert() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let valid_start = provider_span("batch_start", TraceSpanPhase::Start);
    let valid_end = provider_span("batch_end", TraceSpanPhase::End);
    let mut invalid = provider_span("batch_invalid", TraceSpanPhase::Start);
    invalid.span_status = Some(TraceSpanStatus::Error);

    assert!(
        store
            .append_trace_batch(&[valid_start, valid_end, invalid])
            .is_err()
    );
    assert!(matches!(
        store.list_trace("run_span"),
        Err(StoreError::NotFound(message)) if message == "trace run run_span"
    ));
}

#[test]
fn append_trace_batch_rejects_overflow_before_persisting_any_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let start = provider_span("overflow_start", TraceSpanPhase::Start);
    let mut end = provider_span("overflow_end", TraceSpanPhase::End);
    end.duration_ms = Some(u64::MAX);
    assert!(store.append_trace_batch(&[start, end]).is_err());
    assert!(matches!(
        store.list_trace("run_span"),
        Err(StoreError::NotFound(message)) if message == "trace run run_span"
    ));

    let mut prompt = metric_span(
        "overflow_prompt",
        "overflow_prompt_span",
        TraceSpanKind::PromptAssembly,
        TraceSpanPhase::Start,
        0,
    );
    prompt.span_projection = Some(TraceSpanProjection {
        model_turn_ordinal: Some(u64::MAX),
        ..TraceSpanProjection::default()
    });
    assert!(store.append_trace(&prompt).is_err());
    assert!(matches!(
        store.list_trace("metrics_run"),
        Err(StoreError::NotFound(message)) if message == "trace run metrics_run"
    ));
}

#[test]
fn trace_metrics_report_incomplete_start_end_without_faking_duration_zero() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    store
        .append_trace(&metric_span(
            "incomplete_turn_start",
            "incomplete_turn",
            TraceSpanKind::Turn,
            TraceSpanPhase::Start,
            0,
        ))
        .expect("start-only span");
    let metrics = store.trace_metrics("metrics_run").expect("metrics");
    let metric = metrics.metric("turn_duration_ms").expect("turn metric");
    assert!(matches!(
        metric.availability,
        TraceMetricAvailability::Unavailable {
            reason: singularity_protocol::TraceMetricUnavailableReason::IncompleteStartEnd
        }
    ));
    assert!(metric.distribution.is_none());
}

fn metric_span(
    event_id: &str,
    span_id: &str,
    kind: TraceSpanKind,
    phase: TraceSpanPhase,
    duration_ms: u64,
) -> TraceEvent {
    let mut event = TraceEvent::new(
        event_id,
        "metrics_run",
        "metrics_session",
        "metrics",
        "span",
    );
    event.span_id = Some(span_id.to_string());
    event.span_kind = Some(kind);
    event.span_phase = Some(phase);
    if phase == TraceSpanPhase::End {
        event.span_status = Some(TraceSpanStatus::Ok);
        event.duration_ms = Some(duration_ms);
    }
    event
}

#[test]
fn trace_metrics_are_derived_from_typed_trace_events_with_deterministic_percentiles() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let mut provider_start = metric_span(
        "provider_start_1",
        "provider_1",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::Start,
        0,
    );
    provider_start.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(1),
        ..TraceSpanProjection::default()
    });
    let mut provider_end = metric_span(
        "provider_end_1",
        "provider_1",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::End,
        10,
    );
    provider_end.time_to_first_token_ms = Some(4);
    provider_end.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(1),
        retry_count: Some(0),
        request_send_to_headers_ms: Some(3),
        queue_duration_ms: Some(1),
        usage: Some(TraceUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        ..TraceSpanProjection::default()
    });

    let mut provider_start_2 = metric_span(
        "provider_start_2",
        "provider_2",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::Start,
        0,
    );
    provider_start_2.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(2),
        ..TraceSpanProjection::default()
    });
    let mut provider_end_2 = metric_span(
        "provider_end_2",
        "provider_2",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::End,
        20,
    );
    provider_end_2.time_to_first_token_ms = Some(8);
    provider_end_2.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(2),
        retry_count: Some(1),
        retry_backoff_ms: Some(6),
        ..TraceSpanProjection::default()
    });

    let mut tool_start = metric_span(
        "tool_start",
        "tool_1",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::Start,
        0,
    );
    tool_start.span_projection = Some(TraceSpanProjection {
        tool: Some(Default::default()),
        ..TraceSpanProjection::default()
    });
    let mut tool_end = metric_span(
        "tool_end",
        "tool_1",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::End,
        7,
    );
    tool_end.span_projection = Some(TraceSpanProjection {
        tool: Some(TraceToolProjection {
            status: Some(TraceToolStatus::Succeeded),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    });

    let mut samples = TraceEvent::new(
        "metric_samples",
        "metrics_run",
        "metrics_session",
        "metrics",
        "samples",
    );
    samples.metric_samples = vec![
        TraceMetricSample {
            kind: TraceMetricSampleKind::CompletionRejection,
            count: 2,
        },
        TraceMetricSample {
            kind: TraceMetricSampleKind::CompletionRepair,
            count: 1,
        },
        TraceMetricSample {
            kind: TraceMetricSampleKind::EventGap,
            count: 3,
        },
    ];

    store
        .append_trace_batch(&[
            provider_start,
            provider_end,
            provider_start_2,
            provider_end_2,
            tool_start,
            tool_end,
            samples,
        ])
        .expect("append metric events");

    let metrics = store.trace_metrics("metrics_run").expect("trace metrics");
    let provider_duration = metrics
        .metric("provider_attempt_duration_ms")
        .expect("provider duration metric");
    let distribution = provider_duration
        .distribution
        .as_ref()
        .expect("distribution");
    assert_eq!(distribution.count, 2);
    assert_eq!(distribution.sum, 30);
    assert_eq!(distribution.p50, Some(10));
    assert_eq!(distribution.p95, Some(20));
    assert!(matches!(
        metrics
            .metric("provider_time_to_first_token_ms")
            .expect("ttft")
            .availability,
        TraceMetricAvailability::Available
    ));
    assert_eq!(
        metrics
            .metric("completion_rejection_count")
            .expect("rejection")
            .distribution
            .as_ref()
            .expect("rejection distribution")
            .count,
        1
    );
    assert_eq!(
        metrics
            .metric("completion_rejection_count")
            .expect("rejection")
            .distribution
            .as_ref()
            .expect("rejection distribution")
            .sum,
        2
    );
}

#[test]
fn trace_metrics_accept_untyped_transport_samples_and_new_observation_projections() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let prompt_start = metric_span(
        "prompt_start",
        "prompt",
        TraceSpanKind::PromptAssembly,
        TraceSpanPhase::Start,
        0,
    );
    let mut prompt_end = metric_span(
        "prompt_end",
        "prompt",
        TraceSpanKind::PromptAssembly,
        TraceSpanPhase::End,
        4,
    );
    prompt_end.span_projection = Some(TraceSpanProjection {
        message_count: Some(3),
        tool_count: Some(1),
        request_token_count: Some(8),
        request_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        compacted: Some(true),
        finalization_only: Some(false),
        model_turn_ordinal: Some(2),
        ..TraceSpanProjection::default()
    });

    let mut provider_start = metric_span(
        "provider_start_cached",
        "provider_cached",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::Start,
        0,
    );
    provider_start.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(1),
        ..TraceSpanProjection::default()
    });
    let mut provider_end = metric_span(
        "provider_end_cached",
        "provider_cached",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::End,
        12,
    );
    provider_end.span_projection = Some(TraceSpanProjection {
        provider_name: Some("provider".to_string()),
        model_name: Some("model".to_string()),
        protocol: Some(TraceProviderProtocol::OpenAiResponses),
        attempt_index: Some(1),
        usage: Some(TraceUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_input_tokens: 7,
            reasoning_tokens: 0,
        }),
        ..TraceSpanProjection::default()
    });

    let mut tool_start = metric_span(
        "tool_start_success",
        "tool_success",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::Start,
        0,
    );
    tool_start.span_projection = Some(TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection::default()),
        ..TraceSpanProjection::default()
    });
    let mut tool_end = metric_span(
        "tool_end_success",
        "tool_success",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::End,
        5,
    );
    tool_end.span_projection = Some(TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection {
            status: Some(TraceToolStatus::Succeeded),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    });

    let mut tool_failed_start = metric_span(
        "tool_start_failed",
        "tool_failed",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::Start,
        0,
    );
    tool_failed_start.span_projection = Some(TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection::default()),
        ..TraceSpanProjection::default()
    });
    let mut tool_failed_end = metric_span(
        "tool_end_failed",
        "tool_failed",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::End,
        6,
    );
    tool_failed_end.span_status = Some(TraceSpanStatus::Error);
    tool_failed_end.span_projection = Some(TraceSpanProjection {
        model_turn_ordinal: Some(2),
        tool: Some(TraceToolProjection {
            status: Some(TraceToolStatus::Failed),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    });

    let mut sandbox_start = metric_span(
        "sandbox_start",
        "sandbox",
        TraceSpanKind::SandboxExecution,
        TraceSpanPhase::Start,
        0,
    );
    sandbox_start.span_projection = Some(TraceSpanProjection {
        sandbox: Some(TraceSandboxProjection::default()),
        ..TraceSpanProjection::default()
    });
    let mut sandbox_end = metric_span(
        "sandbox_end",
        "sandbox",
        TraceSpanKind::SandboxExecution,
        TraceSpanPhase::End,
        9,
    );
    sandbox_end.span_projection = Some(TraceSpanProjection {
        sandbox: Some(TraceSandboxProjection {
            workspace_mutation: Some(TraceWorkspaceMutation::Changed),
            enforcement: Some(TraceSandboxEnforcement::Strict),
            ..TraceSandboxProjection::default()
        }),
        ..TraceSpanProjection::default()
    });

    let mut transport_sample = TraceEvent::new(
        "transport_samples",
        "metrics_run",
        "metrics_session",
        "transport",
        "closed samples",
    );
    transport_sample.metric_samples = vec![
        TraceMetricSample {
            kind: TraceMetricSampleKind::ProviderCapabilityCacheHit,
            count: 2,
        },
        TraceMetricSample {
            kind: TraceMetricSampleKind::ProviderCapabilityCacheMiss,
            count: 1,
        },
        TraceMetricSample {
            kind: TraceMetricSampleKind::EventGap,
            count: 3,
        },
    ];

    store
        .append_trace_batch(&[
            prompt_start,
            prompt_end,
            provider_start,
            provider_end,
            tool_start,
            tool_end,
            tool_failed_start,
            tool_failed_end,
            sandbox_start,
            sandbox_end,
            transport_sample,
        ])
        .expect("append observation projections");

    let stored = store.list_trace("metrics_run").expect("list trace");
    let stored_prompt = stored
        .iter()
        .find(|event| event.event_id == "prompt_end")
        .expect("prompt end");
    assert_eq!(
        stored_prompt
            .span_projection
            .as_ref()
            .and_then(|projection| projection.request_digest.as_deref()),
        Some("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    );
    let stored_sandbox = stored
        .iter()
        .find(|event| event.event_id == "sandbox_end")
        .expect("sandbox end");
    assert_eq!(
        stored_sandbox
            .span_projection
            .as_ref()
            .and_then(|projection| projection.sandbox.as_ref())
            .and_then(|sandbox| sandbox.workspace_mutation),
        Some(TraceWorkspaceMutation::Changed)
    );

    let metrics = store.trace_metrics("metrics_run").expect("trace metrics");
    assert_eq!(
        metrics
            .metric("provider_cached_input_tokens")
            .expect("cached input metric")
            .distribution
            .as_ref()
            .expect("cached input distribution")
            .min,
        Some(7)
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_count")
            .expect("cache hit count")
            .distribution
            .as_ref()
            .expect("cache hit distribution")
            .min,
        Some(2)
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_count")
            .expect("cache hit count")
            .distribution
            .as_ref()
            .expect("cache hit distribution")
            .sum,
        2
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_rate_bps")
            .expect("cache hit rate")
            .distribution
            .as_ref()
            .expect("cache hit rate distribution")
            .min,
        Some(6666)
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_rate_bps")
            .expect("cache hit rate")
            .distribution
            .as_ref()
            .expect("cache hit rate distribution")
            .sum,
        6666
    );
    assert_eq!(
        metrics
            .metric("tool_success_rate_bps")
            .expect("tool success rate")
            .distribution
            .as_ref()
            .expect("tool success rate distribution")
            .min,
        Some(5000)
    );
    assert_eq!(
        metrics
            .metric("tool_success_rate_bps")
            .expect("tool success rate")
            .distribution
            .as_ref()
            .expect("tool success rate distribution")
            .sum,
        5000
    );
    assert_eq!(
        metrics
            .metric("event_gap_count")
            .expect("event gap")
            .distribution
            .as_ref()
            .expect("event gap distribution")
            .min,
        Some(3)
    );
    assert_eq!(
        metrics
            .metric("event_gap_count")
            .expect("event gap")
            .distribution
            .as_ref()
            .expect("event gap distribution")
            .sum,
        3
    );
}

#[test]
fn trace_metrics_expose_known_zero_counts_and_single_sided_cache_capabilities() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let provider_start = metric_span(
        "provider_success_start",
        "provider_success",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::Start,
        0,
    );
    let provider_end = metric_span(
        "provider_success_end",
        "provider_success",
        TraceSpanKind::ProviderAttempt,
        TraceSpanPhase::End,
        1,
    );
    let mut tool_start = metric_span(
        "tool_failure_start",
        "tool_failure",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::Start,
        0,
    );
    tool_start.span_projection = Some(TraceSpanProjection {
        tool: Some(TraceToolProjection::default()),
        ..TraceSpanProjection::default()
    });
    let mut tool_end = metric_span(
        "tool_failure_end",
        "tool_failure",
        TraceSpanKind::ToolCall,
        TraceSpanPhase::End,
        1,
    );
    tool_end.span_status = Some(TraceSpanStatus::Error);
    tool_end.span_projection = Some(TraceSpanProjection {
        tool: Some(TraceToolProjection {
            status: Some(TraceToolStatus::Failed),
            ..TraceToolProjection::default()
        }),
        ..TraceSpanProjection::default()
    });
    let mut hit_samples = TraceEvent::new(
        "cache_hit_samples",
        "metrics_run",
        "metrics_session",
        "transport",
        "cache hits",
    );
    hit_samples.metric_samples = vec![
        TraceMetricSample {
            kind: TraceMetricSampleKind::ProviderCapabilityCacheHit,
            count: 2,
        },
        TraceMetricSample {
            kind: TraceMetricSampleKind::ProviderCapabilityCacheHit,
            count: 3,
        },
    ];
    store
        .append_trace_batch(&[
            provider_start,
            provider_end,
            tool_start,
            tool_end,
            hit_samples,
        ])
        .expect("append known zero metrics");

    let metrics = store.trace_metrics("metrics_run").expect("trace metrics");
    for name in ["provider_error_count", "tool_success_count"] {
        let metric = metrics.metric(name).expect("count metric");
        assert!(matches!(
            metric.availability,
            TraceMetricAvailability::Available
        ));
        assert_eq!(
            metric
                .distribution
                .as_ref()
                .expect("count distribution")
                .sum,
            0
        );
    }
    assert_eq!(
        metrics
            .metric("tool_success_rate_bps")
            .expect("tool success rate")
            .distribution
            .as_ref()
            .expect("tool success rate distribution")
            .sum,
        0
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_count")
            .expect("cache hit count")
            .distribution
            .as_ref()
            .expect("cache hit distribution")
            .sum,
        5
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_miss_count")
            .expect("cache miss count")
            .distribution
            .as_ref()
            .expect("cache miss distribution")
            .sum,
        0
    );
    assert_eq!(
        metrics
            .metric("provider_capability_cache_hit_rate_bps")
            .expect("cache hit rate")
            .distribution
            .as_ref()
            .expect("cache hit rate distribution")
            .sum,
        10_000
    );

    let miss_dir = tempfile::tempdir().expect("miss temp dir");
    let miss_store =
        SessionStore::open(miss_dir.path().join("sessions.sqlite3")).expect("open miss store");
    let mut miss_samples = TraceEvent::new(
        "cache_miss_samples",
        "metrics_run",
        "metrics_session",
        "transport",
        "cache misses",
    );
    miss_samples.metric_samples = vec![TraceMetricSample {
        kind: TraceMetricSampleKind::ProviderCapabilityCacheMiss,
        count: 4,
    }];
    miss_store
        .append_trace(&miss_samples)
        .expect("append cache miss");
    let miss_metrics = miss_store
        .trace_metrics("metrics_run")
        .expect("miss metrics");
    assert_eq!(
        miss_metrics
            .metric("provider_capability_cache_hit_count")
            .expect("cache hit count")
            .distribution
            .as_ref()
            .expect("cache hit distribution")
            .sum,
        0
    );
    assert_eq!(
        miss_metrics
            .metric("provider_capability_cache_miss_count")
            .expect("cache miss count")
            .distribution
            .as_ref()
            .expect("cache miss distribution")
            .sum,
        4
    );
    assert_eq!(
        miss_metrics
            .metric("provider_capability_cache_hit_rate_bps")
            .expect("cache hit rate")
            .distribution
            .as_ref()
            .expect("cache hit rate distribution")
            .sum,
        0
    );
}

#[test]
fn trace_metrics_fail_closed_for_missing_or_conflicting_tool_terminal_status() {
    for (span_status, tool_status) in [
        (TraceSpanStatus::Ok, None),
        (TraceSpanStatus::Ok, Some(TraceToolStatus::Failed)),
        (TraceSpanStatus::Cancelled, Some(TraceToolStatus::Succeeded)),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let mut start = metric_span(
            "tool_terminal_start",
            "tool_terminal",
            TraceSpanKind::ToolCall,
            TraceSpanPhase::Start,
            0,
        );
        start.span_projection = Some(TraceSpanProjection {
            tool: Some(TraceToolProjection::default()),
            ..TraceSpanProjection::default()
        });
        let mut end = metric_span(
            "tool_terminal_end",
            "tool_terminal",
            TraceSpanKind::ToolCall,
            TraceSpanPhase::End,
            1,
        );
        end.span_status = Some(span_status);
        end.span_projection = Some(TraceSpanProjection {
            tool: Some(TraceToolProjection {
                status: tool_status,
                ..TraceToolProjection::default()
            }),
            ..TraceSpanProjection::default()
        });
        store
            .append_trace_batch(&[start, end])
            .expect("append tool trace");
        assert!(store.trace_metrics("metrics_run").is_err());
    }
}

#[test]
fn trace_metric_sample_sum_overflow_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut event = TraceEvent::new(
        "overflow_samples",
        "metrics_run",
        "metrics_session",
        "transport",
        "overflow samples",
    );
    event.metric_samples = vec![
        TraceMetricSample {
            kind: TraceMetricSampleKind::EventGap,
            count: i64::MAX as u64,
        };
        3
    ];
    store.append_trace(&event).expect("append overflow samples");
    assert!(matches!(
        store.trace_metrics("metrics_run"),
        Err(StoreError::InvalidState(message)) if message.contains("trace metric sum overflow")
    ));
}

#[test]
fn trace_metrics_expose_unavailable_reasons_instead_of_zero_placeholders() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    store
        .append_trace(&TraceEvent::new(
            "legacy_metric_event",
            "legacy_metric_run",
            "legacy_metric_session",
            "legacy",
            "legacy",
        ))
        .expect("legacy event");
    let metrics = store
        .trace_metrics("legacy_metric_run")
        .expect("legacy metrics");
    assert!(matches!(
        metrics
            .metric("provider_time_to_first_token_ms")
            .expect("ttft")
            .availability,
        TraceMetricAvailability::Unavailable { .. }
    ));
    assert!(
        metrics
            .metric("provider_time_to_first_token_ms")
            .expect("ttft")
            .distribution
            .is_none()
    );
    assert!(matches!(
        metrics
            .metric("provider_capability_cache_hit_rate_bps")
            .expect("cache hit rate")
            .availability,
        TraceMetricAvailability::Unavailable {
            reason: singularity_protocol::TraceMetricUnavailableReason::LegacyOnly
        }
    ));
    assert!(matches!(
        metrics
            .metric("tool_success_rate_bps")
            .expect("tool success rate")
            .availability,
        TraceMetricAvailability::Unavailable {
            reason: singularity_protocol::TraceMetricUnavailableReason::LegacyOnly
        }
    ));
}

#[test]
fn trace_envelope_and_projection_tampering_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut event = TraceEvent::new(
        "trace_envelope_tampered",
        "run_envelope",
        "session_envelope",
        "test",
        "safe summary",
    );
    event.timestamp = Some("2026-07-21T00:00:00Z".to_string());
    store.append_trace(&event).expect("append envelope trace");
    let connection = rusqlite::Connection::open(&db_path).expect("open tamper connection");
    let payload: String = connection
        .query_row(
            "select payload from trace_events where event_id = 'trace_envelope_tampered'",
            [],
            |row| row.get(0),
        )
        .expect("read envelope payload");
    let mut payload: Value = serde_json::from_str(&payload).expect("parse envelope payload");
    payload["timestamp"] = serde_json::json!("2026-07-21T00:00:01Z");
    connection
        .execute(
            "update trace_events set payload = ?1 where event_id = 'trace_envelope_tampered'",
            [serde_json::to_string(&payload).expect("serialize envelope payload")],
        )
        .expect("tamper envelope");
    assert!(matches!(
        store.show_trace("trace_envelope_tampered"),
        Err(StoreError::TraceIntegrity(_))
    ));
    drop(connection);

    let typed = provider_span("projection_tampered", TraceSpanPhase::Start);
    store.append_trace(&typed).expect("append typed trace");
    let connection = rusqlite::Connection::open(&db_path).expect("open projection connection");
    connection
        .execute(
            "update trace_events set span_kind = 'turn' where event_id = 'projection_tampered'",
            [],
        )
        .expect("tamper projection");
    assert!(matches!(
        store.show_trace("projection_tampered"),
        Err(StoreError::InvalidState(message)) if message.contains("columns do not match")
    ));
}

#[test]
fn verification_projection_corruption_and_unknown_repair_reason_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");

    for (event_id, projection) in [
        (
            "projection_malformed",
            r#"{"verification":{"bogus":true}}"#.to_string(),
        ),
        (
            "projection_unknown_repair_reason",
            r#"{"verification":{"repair_reason":"unknown"}}"#.to_string(),
        ),
    ] {
        let db_path = dir.path().join(format!("{event_id}.sqlite3"));
        let store = SessionStore::open(&db_path).expect("open store");
        let mut event = TraceEvent::new(
            event_id,
            "run_projection_corruption",
            "session_projection_corruption",
            "test",
            "projection",
        );
        event.span_id = Some(format!("{event_id}_span"));
        event.span_kind = Some(TraceSpanKind::Verification);
        event.span_phase = Some(TraceSpanPhase::Start);
        event.span_projection = Some(TraceSpanProjection {
            verification: Some(TraceVerificationProjection::default()),
            ..TraceSpanProjection::default()
        });
        store.append_trace(&event).expect("append projection");

        let connection = rusqlite::Connection::open(&db_path).expect("open tamper connection");
        connection
            .execute(
                "update trace_events set span_projection = ?1 where event_id = ?2",
                rusqlite::params![projection, event_id],
            )
            .expect("tamper projection");
        drop(connection);

        assert!(matches!(
            store.show_trace(event_id),
            Err(StoreError::InvalidState(message))
                if message.contains("trace span projection is invalid")
        ));
    }
}

#[test]
fn v11_to_v13_migration_rehashes_legacy_trace_without_fabricating_spans() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v11.sqlite3");
    create_v11_database(&db_path);
    let mut legacy = TraceEvent::new(
        "v11_legacy_trace",
        "thread_v10",
        "thread_v10",
        "legacy",
        "legacy event",
    );
    legacy.payload = serde_json::json!({"safe": true});
    let connection = rusqlite::Connection::open(&db_path).expect("open v11 connection");
    connection
        .execute(
            "insert into trace_events(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                legacy.event_id,
                legacy.run_id,
                legacy.session_id,
                serde_json::to_string(&legacy).expect("legacy trace")
            ],
        )
        .expect("insert v11 trace");
    drop(connection);

    let migrated = SessionStore::open(&db_path).expect("migrate v11 store");
    let stored = migrated
        .show_trace("v11_legacy_trace")
        .expect("read migrated trace");
    assert_eq!(migrated.descriptor().schema_version, 13);
    assert_eq!(stored.span_id, None);
    assert_eq!(stored.parent_span_id, None);
    assert_eq!(stored.span_kind, None);
    assert_eq!(stored.span_phase, None);
    assert_eq!(stored.span_status, None);
    assert_eq!(stored.duration_ms, None);
    assert_eq!(stored.time_to_first_token_ms, None);
    let connection = rusqlite::Connection::open(&db_path).expect("inspect migrated trace");
    let projection: (Option<String>, Option<String>, Option<String>, Option<i64>) = connection
        .query_row(
            "select span_id, span_kind, span_phase, duration_ms
             from trace_events where event_id = 'v11_legacy_trace'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read migrated projection");
    assert_eq!(projection, (None, None, None, None));
}

#[test]
fn v9_empty_trace_payload_hash_migrates_as_pre_hash_legacy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v9-empty-trace-hash.sqlite3");
    create_legacy_enum_database(&db_path, 9);
    remove_legacy_pending_approval(&db_path, 9);

    let mut legacy = TraceEvent::new(
        "v9_empty_trace_hash",
        "thread_legacy",
        "thread_legacy",
        "legacy",
        "Authorization: Bearer v9-secret",
    );
    legacy.redaction_applied = true;
    legacy.payload = serde_json::json!({"authorization": "Bearer v9-secret"});
    legacy.payload_hash.clear();
    let connection = rusqlite::Connection::open(&db_path).expect("open v9 connection");
    connection
        .execute(
            "insert into trace_events(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                legacy.event_id,
                legacy.run_id,
                legacy.session_id,
                serde_json::to_string(&legacy).expect("legacy trace")
            ],
        )
        .expect("insert v9 trace");
    drop(connection);

    let migrated = SessionStore::open(&db_path).expect("migrate v9 store");
    let stored = migrated
        .show_trace("v9_empty_trace_hash")
        .expect("read migrated trace");
    assert_eq!(migrated.descriptor().schema_version, 13);
    assert_eq!(stored.summary, "[redacted]");
    assert_eq!(stored.payload["authorization"], "[redacted]");
    assert!(stored.payload_hash.starts_with("sha256:"));
}

#[test]
fn v9_nonempty_trace_payload_hash_mismatch_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v9-bad-trace-hash.sqlite3");
    create_legacy_enum_database(&db_path, 9);
    remove_legacy_pending_approval(&db_path, 9);

    let mut legacy = TraceEvent::new(
        "v9_bad_trace_hash",
        "thread_legacy",
        "thread_legacy",
        "legacy",
        "legacy event",
    );
    legacy.redaction_applied = true;
    legacy.payload = serde_json::json!({"safe": true});
    legacy.payload_hash = format!("sha256:{}", "0".repeat(64));
    let connection = rusqlite::Connection::open(&db_path).expect("open v9 connection");
    connection
        .execute(
            "insert into trace_events(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                legacy.event_id,
                legacy.run_id,
                legacy.session_id,
                serde_json::to_string(&legacy).expect("legacy trace")
            ],
        )
        .expect("insert v9 trace");
    drop(connection);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::TraceIntegrity(message))
            if message.contains("trace envelope hash mismatch")
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
}

#[test]
fn v10_empty_trace_payload_hash_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("v10-empty-trace-hash.sqlite3");
    create_v10_database(&db_path);

    let mut legacy = TraceEvent::new(
        "v10_empty_trace_hash",
        "thread_v10",
        "thread_v10",
        "legacy",
        "legacy event",
    );
    legacy.redaction_applied = true;
    legacy.payload = serde_json::json!({"safe": true});
    legacy.payload_hash.clear();
    let connection = rusqlite::Connection::open(&db_path).expect("open v10 connection");
    connection
        .execute(
            "insert into trace_events(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                legacy.event_id,
                legacy.run_id,
                legacy.session_id,
                serde_json::to_string(&legacy).expect("legacy trace")
            ],
        )
        .expect("insert v10 trace");
    drop(connection);
    let before = sqlite_snapshot(&db_path);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::TraceIntegrity(message))
            if message.contains("trace envelope hash mismatch")
    ));
    assert_eq!(sqlite_snapshot(&db_path), before);
}

#[test]
fn invalid_v11_span_data_rolls_back_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("invalid-v11.sqlite3");
    create_v11_database(&db_path);
    let mut invalid = provider_span("invalid_v11_span", TraceSpanPhase::Start);
    invalid.span_status = Some(TraceSpanStatus::Ok);
    let connection = rusqlite::Connection::open(&db_path).expect("open v11 connection");
    connection
        .execute(
            "insert into trace_events(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
            rusqlite::params![
                invalid.event_id,
                invalid.run_id,
                invalid.session_id,
                serde_json::to_string(&invalid).expect("invalid trace")
            ],
        )
        .expect("insert invalid v11 trace");
    drop(connection);
    let before = sqlite_snapshot(&db_path);
    assert!(SessionStore::open(&db_path).is_err());
    assert_eq!(sqlite_snapshot(&db_path), before);
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
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let item = store
        .append_item(
            &turn.turn_id,
            ItemKind::FileChange,
            serde_json::json!({"changed_files": ["safe/result.txt"]}),
        )
        .expect("item");
    let content_digest = format!("sha256:{}", "a".repeat(64));

    let artifact = store
        .register_artifact_ref(RegisterArtifactRefParams {
            run_id: &thread.thread_id,
            item_id: Some(&item.item_id),
            kind: "file",
            uri: "artifact://safe/result.txt",
            content_digest: &content_digest,
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
        store.list_artifact_refs(&thread.thread_id).expect("list")[0].artifact_id,
        artifact.artifact_id
    );

    let connection =
        rusqlite::Connection::open(dir.path().join("sessions.sqlite3")).expect("reopen sqlite");
    connection
        .execute(
            "update artifact_refs set metadata = ?1 where artifact_id = ?2",
            rusqlite::params![
                serde_json::json!({"note": "token=raw"}).to_string(),
                artifact.artifact_id
            ],
        )
        .expect("tamper metadata");
    let tampered = store.get_artifact_ref(&artifact.artifact_id);
    assert!(matches!(
        tampered,
        Err(StoreError::InvalidState(message)) if message.contains("unredacted sensitive")
    ));
}

// 验证 artifact registration 在同一事务内拒绝不存在、错绑和重复引用，并随 thread 删除。
#[test]
fn artifact_registration_enforces_thread_turn_item_binding_and_deletion() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let item = store
        .append_item(
            &turn.turn_id,
            ItemKind::FileChange,
            serde_json::json!({"changed_files": ["safe/result.txt"]}),
        )
        .expect("item");
    let other_thread = store.create_thread(None, None).expect("other thread");
    let other_turn = store
        .create_turn(&other_thread.thread_id, "running")
        .expect("other turn");
    let other_item = store
        .append_item(
            &other_turn.turn_id,
            ItemKind::FileChange,
            serde_json::json!({"changed_files": ["other.txt"]}),
        )
        .expect("other item");
    let content_digest = format!("sha256:{}", "b".repeat(64));
    fn registration<'a>(
        run_id: &'a str,
        item_id: Option<&'a str>,
        content_digest: &'a str,
    ) -> RegisterArtifactRefParams<'a> {
        RegisterArtifactRefParams {
            run_id,
            item_id,
            kind: "file",
            uri: "artifact://safe/result.txt",
            content_digest,
            summary: "safe result",
            metadata: serde_json::json!({"path": "safe/result.txt"}),
        }
    }

    assert!(matches!(
        store.register_artifact_ref(registration(
            "missing_thread",
            None,
            &content_digest
        )),
        Err(StoreError::NotFound(message)) if message == "artifact run missing_thread"
    ));
    assert!(matches!(
        store.register_artifact_ref(registration(
            &turn.turn_id,
            Some(&item.item_id),
            &content_digest
        )),
        Err(StoreError::InvalidState(message)) if message.contains("run_id must identify a thread")
    ));
    assert!(matches!(
        store.register_artifact_ref(registration(
            &thread.thread_id,
            Some("missing_item"),
            &content_digest
        )),
        Err(StoreError::NotFound(message)) if message == "artifact item missing_item"
    ));
    assert!(matches!(
        store.register_artifact_ref(registration(
            &thread.thread_id,
            Some(&other_item.item_id),
            &content_digest
        )),
        Err(StoreError::InvalidState(message)) if message.contains("item does not belong to run")
    ));

    let artifact = store
        .register_artifact_ref(registration(
            &thread.thread_id,
            Some(&item.item_id),
            &content_digest,
        ))
        .expect("valid artifact");
    assert!(matches!(
        store.register_artifact_ref(registration(
            &thread.thread_id,
            Some(&item.item_id),
            &content_digest,
        )),
        Err(StoreError::AlreadyExists(message)) if message.contains("artifact")
    ));

    store
        .update_turn_status(&turn.turn_id, TurnStatus::Completed)
        .expect("complete turn");
    store
        .delete_thread(&thread.thread_id)
        .expect("delete thread");
    assert!(matches!(
        store.get_artifact_ref(&artifact.artifact_id),
        Err(StoreError::NotFound(message)) if message == format!("artifact {}", artifact.artifact_id)
    ));
}

// 验证 artifact 的公共字段合同在任何写入前拒绝歧义或不可验证输入。
#[test]
fn artifact_registration_rejects_invalid_public_contract_fields() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let item = store
        .append_item(
            &turn.turn_id,
            ItemKind::FileChange,
            serde_json::json!({"changed_files": ["safe/result.txt"]}),
        )
        .expect("item");
    let content_digest = format!("sha256:{}", "c".repeat(64));
    let valid = || RegisterArtifactRefParams {
        run_id: &thread.thread_id,
        item_id: Some(&item.item_id),
        kind: "file",
        uri: "artifact://safe/result.txt",
        content_digest: &content_digest,
        summary: "safe result",
        metadata: serde_json::json!({"path": "safe/result.txt"}),
    };

    let mut invalid_kind = valid();
    invalid_kind.kind = "";
    assert!(matches!(
        store.register_artifact_ref(invalid_kind),
        Err(StoreError::InvalidState(message)) if message.contains("artifact kind")
    ));

    let mut invalid_uri = valid();
    invalid_uri.uri = "file://outside/result.txt";
    assert!(matches!(
        store.register_artifact_ref(invalid_uri),
        Err(StoreError::InvalidState(message)) if message.contains("artifact uri")
    ));

    let mut invalid_digest = valid();
    invalid_digest.content_digest = "sha256:short";
    assert!(matches!(
        store.register_artifact_ref(invalid_digest),
        Err(StoreError::InvalidState(message)) if message.contains("artifact content digest")
    ));

    let mut invalid_metadata = valid();
    invalid_metadata.metadata = serde_json::json!({
        "artifact_ref": "artifact://unregistered",
    });
    assert!(matches!(
        store.register_artifact_ref(invalid_metadata),
        Err(StoreError::InvalidState(message)) if message.contains("artifact metadata")
    ));
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
            content_digest:
                "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
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

// Issue #24 批次 A（失败证据）：带追加输入（steer/follow-up）的 completed turn
// 应完整进入下一轮历史。当前实现只收恰好 [User, Assistant] 两条消息的 turn，
// 该 turn 会被整体排除，下一轮历史缺失追加输入前后的消息。
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
        .create_turn_with_input_trace_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            "test",
            "turn started",
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
        7..=10 => serde_json::json!({
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
