use schemars::schema_for;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{ItemKind, TraceEvent};
use singularity_store::{ActiveSidecarRun, SessionStore, SessionStoreDescriptor, StoreError};

#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 4);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string(),
            "0003_active_sidecar_runs".to_string(),
            "0004_pending_tool_calls".to_string()
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
    let approval = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
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
fn approval_decision_is_written_once_and_kept_in_decision_ledger() {
    for outcome in [
        ApprovalOutcome::Allow,
        ApprovalOutcome::Deny,
        ApprovalOutcome::Defer,
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let request = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
        store.create_approval(&request).expect("approval");
        let decision = ApprovalDecision::new("approval_1", outcome, "operator decision");

        let recorded = store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("decision");
        let trace = recorded.trace;

        assert_eq!(recorded.request, request);
        assert_eq!(recorded.decision, decision);
        assert_eq!(trace.run_id, "session_1");
        assert_eq!(trace.session_id, "session_1");
        assert_eq!(trace.task_id.as_deref(), Some("task_1"));
        assert_eq!(trace.payload["request_id"], "approval_1");
        assert_eq!(trace.payload["decision_id"], decision.decision_id);
        assert_eq!(
            trace.payload["outcome"],
            serde_json::to_value(outcome).expect("serialize outcome")
        );
        assert_eq!(
            store.list_trace("session_1").expect("trace list")[0].event_id,
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

#[test]
fn active_sidecar_run_register_read_clear_and_missing_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    let active = store
        .register_active_sidecar_run(&turn.turn_id, "run_1", "session_1", "task_1", "running")
        .expect("register active run");

    assert_eq!(
        active,
        ActiveSidecarRun {
            turn_id: turn.turn_id.clone(),
            thread_id: thread.thread_id,
            run_id: "run_1".to_string(),
            session_id: "session_1".to_string(),
            task_id: "task_1".to_string(),
            status: "running".to_string(),
            created_at: active.created_at.clone(),
            updated_at: active.updated_at.clone(),
        }
    );
    assert_eq!(
        store
            .get_active_sidecar_run(&turn.turn_id)
            .expect("read active run"),
        active
    );

    store
        .clear_active_sidecar_run(&turn.turn_id, "completed")
        .expect("clear active run");
    assert!(matches!(
        store.get_active_sidecar_run(&turn.turn_id),
        Err(StoreError::NotFound(message)) if message == format!("active sidecar run {}", turn.turn_id)
    ));
}

#[test]
fn active_sidecar_run_duplicate_replaces_single_active_record() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    store
        .register_active_sidecar_run(&turn.turn_id, "run_1", "session_1", "task_1", "running")
        .expect("first active run");
    let replaced = store
        .register_active_sidecar_run(&turn.turn_id, "run_2", "session_2", "task_2", "running")
        .expect("replace active run");

    assert_eq!(replaced.run_id, "run_2");
    assert_eq!(replaced.session_id, "session_2");
    assert_eq!(
        store
            .list_active_sidecar_runs()
            .expect("list active runs")
            .len(),
        1
    );
}

#[test]
fn delete_thread_clears_active_sidecar_runs_for_thread() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .register_active_sidecar_run(&turn.turn_id, "run_1", "session_1", "task_1", "running")
        .expect("active run");

    store
        .delete_thread(&thread.thread_id)
        .expect("delete thread");

    assert!(store.get_active_sidecar_run(&turn.turn_id).is_err());
}
