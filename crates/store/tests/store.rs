use schemars::schema_for;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{ItemKind, ThreadStatus, TraceEvent, TurnStatus};
use singularity_store::{
    ConversationRole, RegisterArtifactRefParams, SessionStore, SessionStoreDescriptor, StoreError,
};
use std::sync::{Arc, Barrier};

#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 6);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string(),
            "0004_pending_tool_calls".to_string(),
            "0005_store_hardening".to_string(),
            "0006_conversation_history".to_string()
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
        .create_turn(&thread.thread_id, "not_migrated")
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
        "write_file",
    );
    store.create_approval(&approval).expect("approval");
    let decision = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "ok");
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("decision");

    assert_eq!(item.kind, ItemKind::UserMessage);
    assert_eq!(store.list_trace("run_1").expect("trace list").len(), 1);
    assert_eq!(
        store.show_trace("trace_1").expect("trace show").summary,
        "thread started"
    );
    assert!(store.show_trace("missing").is_err());
}

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

    assert_eq!(schema_version, 6);
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
}

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
            supported: 6
        })
    ));
}

#[test]
fn migrated_schema_rebuilds_foreign_key_tables() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            r#"
            create table schema_migrations(
                migration_id text primary key,
                applied_at text not null default current_timestamp
            );
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
                agent_loop_status text not null
            );
            create table items(
                item_id text primary key,
                turn_id text not null,
                kind text not null,
                payload text not null,
                status text not null
            );
            create table trace_events(
                event_id text primary key,
                run_id text not null,
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
                payload text not null
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
                turn_id text not null,
                tool_call_id text not null,
                payload text not null
            );
            insert into threads(thread_id, model, cwd, status) values('thread_1', null, null, '"active"');
            insert into turns(turn_id, thread_id, status, agent_loop_status) values('turn_1', 'thread_1', '"blocked"', 'blocked');
            insert into items(item_id, turn_id, kind, payload, status) values('item_1', 'turn_1', '"userMessage"', '[{"type":"text","text":"legacy input"}]', '"completed"');
            insert into approvals(request_id, payload, decision_outcome, decision_reason)
            values('approval_1', '{"request_id":"approval_1","session_id":"thread_1","task_id":"turn_1","thread_id":"thread_1","turn_id":"turn_1","tool_call_id":"call_1","action":"builtin.edit","resources":[],"reason":""}', null, null);
            insert into pending_tool_calls(request_id, turn_id, tool_call_id, payload)
            values('approval_1', 'turn_1', 'call_1', '{"request_id":"approval_1","tool_call_id":"call_1"}');
            "#,
        )
        .expect("legacy schema");
    drop(connection);

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
}

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
            "insert into turns(turn_id, thread_id, status, agent_loop_status) values('turn_missing', 'thread_missing', '\"running\"', 'running')",
            [],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload) values('approval_missing', 'thread_missing', 'turn_missing', 'call_1', '{}')",
            [],
        )
        .is_err());
}

#[test]
fn missing_thread_turn_event_and_artifact_refs_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    assert!(matches!(
        store.create_turn("missing_thread", "not_migrated"),
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
        "builtin.patch",
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_other",
        "tool_call_id": "call_1",
        "tool_name": "builtin.patch",
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

#[test]
fn approval_creation_requires_explicit_existing_thread_turn_binding() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let request = ApprovalRequest {
        request_id: "approval_1".to_string(),
        session_id: "session_legacy".to_string(),
        task_id: "task_legacy".to_string(),
        thread_id: String::new(),
        turn_id: String::new(),
        tool_call_id: None,
        action: "builtin.edit".to_string(),
        resources: Vec::new(),
        reason: String::new(),
    };

    assert!(matches!(
        store.create_approval(&request),
        Err(StoreError::InvalidState(message))
            if message == "approval request must include explicit thread_id and turn_id"
    ));
}

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
        "builtin.edit",
    );

    assert!(matches!(
        store.create_approval(&request),
        Err(StoreError::InvalidState(message))
            if message == "approval request thread_id must match bound turn"
    ));
}

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
        "builtin.patch",
    );
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_turn_call_1",
        "tool_call_id": "call_1",
        "tool_name": "builtin.patch",
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
            if message == "pending tool call tool_call_id must match approval request"
    ));
}

#[test]
fn pending_tool_call_binding_requires_existing_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let request = ApprovalRequest::new(
        "approval_missing_turn_call_1",
        "missing_thread",
        "missing_turn",
        "builtin.patch",
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_missing_turn_call_1",
        "tool_call_id": "call_1",
        "tool_name": "builtin.patch",
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

#[test]
fn approval_decision_rejects_pending_tool_call_turn_mismatch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let expected_turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("expected turn");
    let other_turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("other turn");
    let request = ApprovalRequest::new(
        "approval_turn_call_1",
        thread.thread_id.clone(),
        expected_turn.turn_id.clone(),
        "builtin.patch",
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
                    "tool_name": "builtin.patch",
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

#[test]
fn cancellation_request_is_atomic_and_rejects_late_completion() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let trace = TraceEvent::new(
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
            TurnStatus::Completed,
            "completed",
            Some("too late"),
            &TraceEvent::new(
                "trace_too_late",
                &thread.thread_id,
                &turn.turn_id,
                "agent_loop",
                "late completion",
            ),
        ),
        Err(StoreError::InvalidState(message))
            if message == "cancel-requested turn can only finalize as interrupted"
    ));
    let interrupted = store
        .commit_turn_outcome(
            &turn.turn_id,
            TurnStatus::Interrupted,
            "cancelled",
            None,
            &TraceEvent::new(
                "trace_cancelled",
                &thread.thread_id,
                &turn.turn_id,
                "agent_loop",
                "turn cancelled",
            ),
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

#[test]
fn approval_decision_is_written_once_and_kept_in_decision_ledger() {
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
            "write_file",
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
        assert_eq!(trace.session_id, thread.thread_id);
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
        assert_eq!(
            store
                .get_approval_decision(&decision.decision_id)
                .expect("ledger")
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
        "builtin.patch",
    )
    .with_tool_call_id("call_1");
    let pending_tool_call = serde_json::json!({
        "request_id": "approval_turn_call_1",
        "tool_call_id": "call_1",
        "tool_name": "builtin.patch",
        "raw_arguments": "{}",
        "resources": []
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
    assert_eq!(request_trace.session_id, thread.thread_id);
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

fn foreign_key_parents(connection: &rusqlite::Connection, table: &str) -> Vec<String> {
    let query = format!("pragma foreign_key_list({table})");
    let mut statement = connection.prepare(&query).expect("foreign key list");
    statement
        .query_map([], |row| row.get::<_, String>(2))
        .expect("query foreign keys")
        .map(|row| row.expect("foreign key row"))
        .collect()
}

#[test]
fn turn_user_input_can_be_read_for_native_resume() {
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
        "not_migrated",
        serde_json::json!([{"type": "text", "text": "rollback"}]),
        "test",
        "rollback trace",
    );

    assert!(failed.is_err());
    assert!(store.list_trace("missing_after_rollback").is_err());
    let successful = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "not_migrated",
            serde_json::json!([{"type": "text", "text": "ok"}]),
            "test",
            "turn trace",
        )
        .expect("successful turn");
    assert!(store.get_turn(&successful.0.turn_id).is_ok());
}

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
    let trace = TraceEvent::new(
        "trace_terminal_success",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "terminal result",
    );

    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            TurnStatus::Completed,
            "completed",
            Some("assistant"),
            &trace,
        )
        .expect("commit terminal outcome");

    assert_eq!(committed.turn.status, TurnStatus::Completed);
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
    let trace = TraceEvent::new(
        "trace_terminal_failure",
        &thread.thread_id,
        &turn.turn_id,
        "agent_loop",
        "forced terminal failure",
    );

    let result = store.commit_turn_outcome(
        &turn.turn_id,
        TurnStatus::Completed,
        "completed",
        Some("assistant"),
        &trace,
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
            rusqlite::params![
                turn.turn_id,
                serde_json::to_string(&ItemKind::AgentMessage).expect("agent kind")
            ],
            |row| row.get(0),
        )
        .expect("assistant count");
    assert_eq!(assistant_count, 0);
}

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

#[test]
fn complete_history_schema_without_migration_marker_is_resanitized() {
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
    connection
        .execute(
            "update items set payload = ?1, redacted = 0 where item_id = ?2",
            rusqlite::params![
                r#"[{"type":"text","text":"SINGULARITY_API_KEY=unmarked-secret"}]"#,
                item.item_id
            ],
        )
        .expect("inject legacy secret");
    drop(connection);

    let store = SessionStore::open(&db_path).expect("reopen and sanitize");
    drop(store);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let (payload, redacted): (String, bool) = connection
        .query_row(
            "select payload, redacted from items where item_id = ?1",
            [&item.item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read sanitized item");
    assert!(!payload.contains("unmarked-secret"));
    assert!(payload.contains("[redacted sensitive user input]"));
    assert!(redacted);
}

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
            .update_turn_status(&started.turn.turn_id, status)
            .expect("terminal status");
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
        "builtin.read",
    );
    store.create_approval(&approval).expect("approval");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('started_agent_item', ?1, 5, ?2, '{\"delta\":\"not completed\"}', ?3, 0)",
            rusqlite::params![
                completed,
                serde_json::to_string(&ItemKind::AgentMessage).expect("agent kind"),
                serde_json::to_string(&singularity_protocol::ItemStatus::Started)
                    .expect("started status"),
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
            rusqlite::params![
                turn_id,
                serde_json::to_string(&ItemKind::AgentMessage).expect("kind")
            ],
        )
        .expect("tamper payload");

    assert!(matches!(
        store.read_thread_history(&thread.thread_id, None, 10),
        Err(StoreError::InvalidState(_))
    ));
}

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
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values('duplicate_turn', ?1, 1, '\"running\"', 'running')",
            [&thread.thread_id],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('duplicate_item', ?1, 1, '\"reasoning\"', '{}', '\"completed\"', 0)",
            [&turn.turn_id],
        )
        .is_err());
}

#[test]
fn concurrent_connections_allocate_unique_turn_and_item_sequences() {
    const WORKERS: usize = 12;

    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let shared_turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("shared turn");
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

    for handle in handles {
        let (turn, item) = handle.join().expect("worker joins");
        turn.expect("concurrent turn allocation");
        item.expect("concurrent item allocation");
    }

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let turn_sequences = connection
        .prepare("select turn_sequence from turns where thread_id = ?1 order by turn_sequence")
        .expect("prepare turns")
        .query_map([&thread.thread_id], |row| row.get::<_, u64>(0))
        .expect("query turns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect turn sequences");
    assert_eq!(
        turn_sequences,
        (1..=u64::try_from(WORKERS + 1).expect("worker count")).collect::<Vec<_>>()
    );

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

type HistorySequences = (Vec<(String, u64)>, Vec<(String, u64, bool)>);

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
