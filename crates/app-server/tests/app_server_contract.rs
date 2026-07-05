use singularity_app_server::AppServer;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_store::SessionStore;

#[test]
fn app_server_enforces_initialize_and_emits_item_events() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);

    let not_initialized = server
        .handle_json(r#"{"method":"thread/start","id":1,"params":{}}"#)
        .unwrap();
    assert_eq!(not_initialized[0]["error"]["message"], "Not initialized");

    let unknown = server
        .handle_json(r#"{"method":"thread/unknown","id":11,"params":{}}"#)
        .unwrap();
    assert_eq!(unknown[0]["error"]["code"], -32601);
    assert_eq!(
        unknown[0]["error"]["message"],
        "Method not found: thread/unknown"
    );

    let initialized = server
        .handle_json(r#"{"method":"initialize","id":2,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    assert_eq!(initialized[0]["result"]["platformFamily"], "local");

    let before_initialized = server
        .handle_json(r#"{"method":"thread/start","id":30,"params":{}}"#)
        .unwrap();
    assert_eq!(before_initialized[0]["error"]["message"], "Not initialized");

    let duplicate = server
        .handle_json(r#"{"method":"initialize","id":3,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    assert_eq!(duplicate[0]["error"]["message"], "Already initialized");

    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":4,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = thread[0]["result"]["thread"]["thread_id"].as_str().unwrap();
    assert!(
        thread
            .iter()
            .any(|message| message["method"] == "thread/started")
    );

    let list = server
        .handle_json(r#"{"method":"thread/list","id":41,"params":{}}"#)
        .unwrap();
    assert_eq!(list[0]["result"]["threads"][0]["thread_id"], thread_id);

    let read = server
        .handle_json(&format!(
            r#"{{"method":"thread/read","id":42,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(read[0]["result"]["thread"]["thread_id"], thread_id);

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":5,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = turn[0]["result"]["turn"]["turn_id"].as_str().unwrap();

    assert_eq!(
        turn[0]["result"]["turn"]["agent_loop_status"],
        "not_migrated"
    );
    assert!(
        turn.iter()
            .any(|message| message["method"] == "turn/started")
    );
    assert!(
        turn.iter()
            .any(|message| message["method"] == "item/started")
    );
    assert!(
        turn.iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    assert!(
        turn.iter()
            .any(|message| message["method"] == "item/completed")
    );

    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":51,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(status[0]["result"]["turn"]["turn_id"], turn_id);

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":52,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(interrupted[0]["result"]["status"], "interrupted");

    let missing_trace_list = server
        .handle_json(r#"{"method":"trace/list","id":6,"params":{"runId":"missing"}}"#)
        .unwrap();
    assert_eq!(
        missing_trace_list[0]["error"]["message"],
        "Trace run not found"
    );

    let missing_trace_show = server
        .handle_json(r#"{"method":"trace/show","id":7,"params":{"eventId":"missing"}}"#)
        .unwrap();
    assert_eq!(
        missing_trace_show[0]["error"]["message"],
        "Trace event not found"
    );

    let trace_tail = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":8,"params":{{"runId":"{thread_id}","limit":1}}}}"#
        ))
        .unwrap();
    assert_eq!(
        trace_tail[0]["result"]["events"].as_array().unwrap().len(),
        1
    );

    let archived = server
        .handle_json(&format!(
            r#"{{"method":"thread/archive","id":43,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(archived[0]["result"]["thread"]["status"], "archived");

    let forked = server
        .handle_json(&format!(
            r#"{{"method":"thread/fork","id":44,"params":{{"threadId":"{thread_id}","model":"gpt-fork"}}}}"#
        ))
        .unwrap();
    assert_eq!(forked[0]["result"]["sourceThreadId"], thread_id);
    assert_eq!(forked[0]["result"]["thread"]["model"], "gpt-fork");

    let deleted = server
        .handle_json(&format!(
            r#"{{"method":"thread/delete","id":45,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(deleted[0]["result"]["deleted"], true);
}

#[test]
fn approval_decisions_consume_pending_requests_once_for_all_outcomes() {
    for outcome in [
        ApprovalOutcome::Allow,
        ApprovalOutcome::Deny,
        ApprovalOutcome::Defer,
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let mut server = AppServer::new(store);
        server
            .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"method":"initialized","params":{}}"#)
            .unwrap();

        let request = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
        let request_message = serde_json::json!({
            "method": "approval/request",
            "id": 2,
            "params": request,
        });
        let request_result = server.handle_json(&request_message.to_string()).unwrap();
        assert_eq!(
            request_result[0]["result"]["approval"]["request_id"],
            "approval_1"
        );

        let approvals = server
            .handle_json(r#"{"method":"approval/list","id":22,"params":{}}"#)
            .unwrap();
        assert_eq!(
            approvals[0]["result"]["approvals"][0]["request_id"],
            "approval_1"
        );

        let decision = ApprovalDecision::new("approval_1", outcome.clone(), "operator decision");
        let decision_message = serde_json::json!({
            "method": "approval/decision",
            "id": 3,
            "params": decision,
        });
        let decision_result = server.handle_json(&decision_message.to_string()).unwrap();
        assert_eq!(
            decision_result[0]["result"]["decision"]["request_id"],
            "approval_1"
        );

        let duplicate = server.handle_json(&decision_message.to_string()).unwrap();
        assert_eq!(
            duplicate[0]["error"]["message"],
            "Pending approval not found"
        );
    }
}
