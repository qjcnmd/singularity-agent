use schemars::schema_for;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{ItemKind, TraceEvent};
use singularity_store::{SessionStore, SessionStoreDescriptor, StoreError};

#[test]
fn sqlite_store_persists_threads_turns_items_trace_and_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 2);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string()
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
    store
        .record_approval_decision("approval_1", ApprovalOutcome::Allow, "ok")
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
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let request = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
    store.create_approval(&request).expect("approval");
    let decision = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "ok");

    let trace = store
        .record_approval_decision_with_trace(&decision, "approval", "approval decision recorded")
        .expect("decision");

    assert_eq!(trace.run_id, "approval_1");
    assert_eq!(
        store
            .get_approval_decision(&decision.decision_id)
            .expect("ledger")
            .outcome,
        ApprovalOutcome::Allow
    );
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    assert!(matches!(
        store.record_approval_decision_with_trace(
            &decision,
            "approval",
            "approval decision recorded"
        ),
        Err(StoreError::NotFound(message)) if message == "approval approval_1"
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
