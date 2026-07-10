use schemars::schema_for;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{ItemKind, TraceEvent, TurnStatus};
use singularity_store::{SessionStore, SessionStoreDescriptor, StoreError};

#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 5);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string(),
            "0004_pending_tool_calls".to_string(),
            "0005_store_hardening".to_string()
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
            serde_json::json!({"text": "hello"}),
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

    assert_eq!(schema_version, 5);
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
            supported: 5
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
            insert into items(item_id, turn_id, kind, payload, status) values('item_1', 'turn_1', '"user_message"', '{}', '"completed"');
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
fn artifact_refs_are_durable_and_redact_secret_like_metadata() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let artifact = store
        .register_artifact_ref(
            "run_1",
            Some("item_1"),
            "file",
            "artifact://safe/result.txt",
            "sha256:abc",
            "contains token output",
            serde_json::json!({
                "path": "safe/result.txt",
                "api_key": "abc123",
                "nested": {"authorization": "Bearer abc123"}
            }),
        )
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
