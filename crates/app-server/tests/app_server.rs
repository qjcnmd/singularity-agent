use singularity_app_server::AppServer;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_store::SessionStore;
use std::io::Write;
use std::process::{Command, Stdio};

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

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

    let capabilities = server
        .handle_json(r#"{"method":"server/capabilities","id":31,"params":{}}"#)
        .unwrap();
    assert!(
        capabilities[0]["result"]["transports"]
            .as_array()
            .unwrap()
            .iter()
            .any(|transport| transport["transport"] == "stdio" && transport["available"] == true)
    );
    assert!(
        capabilities[0]["result"]["transports"]
            .as_array()
            .unwrap()
            .iter()
            .any(|transport| transport["transport"] == "websocket"
                && transport["available"] == false
                && transport["authTokenRequired"] == true)
    );

    let subscription = server
        .handle_json(
            r#"{"method":"event/subscribe","id":32,"params":{"eventTypes":["thread/started","turn/started"]}}"#,
        )
        .unwrap();
    assert_eq!(
        subscription[0]["result"]["eventTypes"],
        serde_json::json!(["thread/started", "turn/started"])
    );

    let thread = server
        .handle_json(r#"{"method":"thread/start","id":4,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_result = result_message(&thread);
    let thread_id = thread_result["thread"]["thread_id"].as_str().unwrap();
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
            r#"{{"method":"turn/start","id":5,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    assert!(
        turn[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field `agentHost`")
    );

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
    let trace_tail_with_offset = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":82,"params":{{"runId":"{thread_id}","limit":1,"offset":1}}}}"#
        ))
        .unwrap();
    assert_eq!(
        trace_tail_with_offset[0]["result"]["events"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let empty_trace_page = server
        .handle_json(&format!(
            r#"{{"method":"trace/list","id":81,"params":{{"runId":"{thread_id}","limit":1,"offset":99}}}}"#
        ))
        .unwrap();
    assert!(
        empty_trace_page[0]["result"]["events"]
            .as_array()
            .unwrap()
            .is_empty()
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

#[cfg(windows)]
#[test]
fn app_server_reports_native_agent_loop_capability_as_available_after_cutover() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let capability = server
        .handle_json(r#"{"method":"agent/capability","id":2,"params":{}}"#)
        .unwrap();

    assert_eq!(
        capability[0]["result"]["nativeAgentLoop"]["available"],
        true
    );
    assert_eq!(
        capability[0]["result"]["nativeAgentLoop"]["status"],
        "completed"
    );
    assert!(
        capability[0]["result"]["nativeAgentLoop"]["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(not(windows))]
#[test]
fn app_server_reports_native_agent_loop_capability_as_unsupported_off_windows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let capability = server
        .handle_json(r#"{"method":"agent/capability","id":2,"params":{}}"#)
        .unwrap();

    assert_eq!(
        capability[0]["result"]["nativeAgentLoop"]["available"],
        false
    );
    assert_eq!(
        capability[0]["result"]["nativeAgentLoop"]["status"],
        "blocked"
    );
    assert!(
        capability[0]["result"]["nativeAgentLoop"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker == "strict_command_sandbox_unsupported_platform")
    );
}

#[test]
fn app_server_eval_run_writes_blocked_native_result_artifacts_without_python_sidecar() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    let manifest = dir.path().join("eval.json");
    let output_root = dir.path().join("eval-output");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v1",
  "tasks": [
    {
      "task_id": "fixture_eval",
      "workspace": {"type": "unsupported"},
      "user_task": "finish",
      "verification_command": "python -c pass"
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "method": "eval/run",
        "id": 2,
        "params": {
            "manifest": manifest,
            "runId": "eval_blocked",
            "outputRoot": output_root,
        }
    })
    .to_string();
    let response = server.handle_json(&request).unwrap();
    let result = result_message(&response);

    assert_eq!(result["runner"], "rust_native");
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["blocker"], "eval_workspace_failed");
    assert_eq!(result["evaluation_passed"], false);
    assert_eq!(result["tasks"][0]["agent_completed"], false);
    assert_eq!(result["tasks"][0]["tests_passed"], false);
    assert_eq!(result["tasks"][0]["smoke_command_satisfied"], true);
    assert_eq!(result["tasks"][0]["local_process_fallback_count"], 0);
    let result_path = result["result_path"].as_str().expect("result path");
    let report_path = result["report_path"].as_str().expect("report path");
    assert!(std::path::Path::new(result_path).exists());
    assert!(std::path::Path::new(report_path).exists());
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(result_path).expect("result json"))
            .expect("result payload");
    assert_eq!(payload["schema_version"], "evaluation.result/v1");
    assert_eq!(payload["runner"], "rust_native");
    assert_eq!(payload["tasks"][0]["blocker"], "eval_workspace_failed");
    assert_eq!(payload["tasks"][0]["checks"]["public"]["status"], "not_run");
    assert_eq!(
        payload["tasks"][0]["checks"]["smoke"]["status"],
        "not_required"
    );
}

#[test]
fn app_server_eval_run_reports_smoke_not_run_when_blocked_before_agent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    let manifest = dir.path().join("eval.json");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v1",
  "tasks": [
    {
      "task_id": "fixture_eval",
      "workspace": {"type": "unsupported"},
      "user_task": "finish",
      "verification_command": "python -c pass",
      "smoke_command": "python -m py_compile src/app.py"
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "method": "eval/run",
        "id": 2,
        "params": {
            "manifest": manifest,
            "runId": "eval_smoke_not_run",
        }
    })
    .to_string();
    let response = server.handle_json(&request).unwrap();
    let result = result_message(&response);

    assert_eq!(result["status"], "blocked");
    assert_eq!(result["tasks"][0]["blocker"], "eval_workspace_failed");
    assert_eq!(result["tasks"][0]["smoke_command_satisfied"], false);
    assert_eq!(result["tasks"][0]["checks"]["smoke"]["status"], "not_run");
}

#[cfg(not(windows))]
#[test]
fn app_server_eval_run_fails_closed_when_native_capability_is_unavailable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    let manifest = dir.path().join("eval.json");
    let output_root = dir.path().join("eval-output");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v1",
  "tasks": [
    {
      "task_id": "fixture_eval",
      "workspace": {"type": "fixture", "files": {"README.md": "hello"}},
      "user_task": "finish",
      "verification_command": "python -c pass"
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "method": "eval/run",
        "id": 2,
        "params": {
            "manifest": manifest,
            "runId": "eval_native_unavailable",
            "outputRoot": output_root,
        }
    })
    .to_string();
    let response = server.handle_json(&request).unwrap();
    let result = result_message(&response);

    assert_eq!(result["runner"], "rust_native");
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["blocker"], "native_agent_loop_unavailable");
    assert_eq!(result["tasks"][0]["agent_completed"], false);
    assert_eq!(
        result["tasks"][0]["blocker"],
        "native_agent_loop_unavailable"
    );
}

#[test]
fn app_server_rejects_public_agent_host_selector() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert!(
        response[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field `agentHost`")
    );
}

#[test]
fn public_agent_host_rejection_does_not_create_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert!(
        response[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field `agentHost`")
    );
    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    let serialized = serde_json::to_string(&trace).expect("serialize trace");
    assert!(!serialized.contains("turn started"));
}

#[test]
fn turn_start_rejects_agent_host_selector_before_turn_creation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert!(
        response[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field `agentHost`")
    );
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

        let center_without_records = server
            .handle_json(r#"{"method":"approval/center","id":21,"params":{}}"#)
            .unwrap();
        assert!(
            center_without_records[0]["result"]["pendingApprovals"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            center_without_records[0]["result"]["decisions"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let request = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("reopen store");
        store
            .create_approval_with_trace(&request, "approval", "approval requested")
            .expect("approval");
        drop(store);

        let approvals = server
            .handle_json(r#"{"method":"approval/list","id":22,"params":{}}"#)
            .unwrap();
        assert_eq!(
            approvals[0]["result"]["approvals"][0]["request_id"],
            "approval_1"
        );
        let center = server
            .handle_json(r#"{"method":"approval/center","id":23,"params":{}}"#)
            .unwrap();
        assert_eq!(
            center[0]["result"]["pendingApprovals"][0]["request_id"],
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
        let center_after_decision = server
            .handle_json(r#"{"method":"approval/center","id":24,"params":{}}"#)
            .unwrap();
        assert!(
            center_after_decision[0]["result"]["pendingApprovals"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            center_after_decision[0]["result"]["decisions"][0]["request_id"],
            "approval_1"
        );

        let duplicate = server.handle_json(&decision_message.to_string()).unwrap();
        assert_eq!(
            duplicate[0]["error"]["message"],
            "Pending approval not found"
        );
    }
}

#[test]
fn approval_decision_allow_without_pending_tool_call_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("README.md"), "before").expect("readme");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let store = SessionStore::open(&db_path).expect("reopen store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "blocked",
            serde_json::json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            singularity_protocol::TurnStatus::Blocked,
            "blocked",
        )
        .expect("blocked state");
    let request = ApprovalRequest::new(
        format!("approval_{}_call_1", turn.turn_id),
        turn.turn_id.clone(),
        turn.turn_id.clone(),
        "builtin.edit",
    )
    .with_thread_turn_binding(thread.thread_id.clone(), turn.turn_id.clone())
    .with_tool_call_id("call_1")
    .with_resources(["README.md"]);
    store
        .create_approval_with_trace(&request, "approval", "approval requested")
        .expect("approval");
    drop(store);
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("read before"),
        "before"
    );

    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "operator approved",
    );
    let response = server
        .handle_json(
            &serde_json::json!({
                "method": "approval/decision",
                "id": 4,
                "params": decision,
            })
            .to_string(),
        )
        .unwrap();

    assert_eq!(
        response[0]["error"]["message"],
        "Pending approval not found"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("read readme"),
        "before"
    );
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn_after_decision = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(
        turn_after_decision.status,
        singularity_protocol::TurnStatus::Blocked
    );
    assert_eq!(turn_after_decision.agent_loop_status, "blocked");
    assert!(
        store
            .list_trace(&thread.thread_id)
            .expect("trace list")
            .into_iter()
            .all(|event| event.component != "agent_loop")
    );
    assert_eq!(
        store.list_pending_approvals().expect("pending approvals")[0].request_id,
        request.request_id
    );
}

#[test]
fn approval_decision_deny_defer_and_mismatched_resource_do_not_resume_native_turn() {
    for (outcome, request_resource) in [
        (ApprovalOutcome::Deny, "README.md"),
        (ApprovalOutcome::Defer, "README.md"),
        (ApprovalOutcome::Allow, "other.md"),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("README.md"), "before").expect("readme");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("open store");
        let mut server = AppServer::new(store);
        server
            .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"method":"initialized","params":{}}"#)
            .unwrap();
        let thread = server
            .handle_json(&format!(
                r#"{{"method":"thread/start","id":2,"params":{{"cwd":{}}}}}"#,
                serde_json::to_string(&workspace.to_string_lossy()).expect("cwd")
            ))
            .unwrap();
        let thread_id = result_message(&thread)["thread"]["thread_id"]
            .as_str()
            .unwrap();
        let store = SessionStore::open(&db_path).expect("reopen store");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                thread_id,
                "blocked",
                serde_json::json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("blocked turn");
        store
            .update_turn_state(
                &turn.turn_id,
                singularity_protocol::TurnStatus::Blocked,
                "blocked",
            )
            .expect("blocked state");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            turn.turn_id.clone(),
            turn.turn_id.clone(),
            "builtin.edit",
        )
        .with_thread_turn_binding(thread_id.to_string(), turn.turn_id.clone())
        .with_tool_call_id("call_1")
        .with_resources([request_resource]);
        store
            .create_approval_with_trace(&request, "approval", "approval requested")
            .expect("approval");
        drop(store);
        let decision =
            ApprovalDecision::new(request.request_id.clone(), outcome, "operator decision");

        let response = server
            .handle_json(
                &serde_json::json!({
                    "method": "approval/decision",
                    "id": 3,
                    "params": decision,
                })
                .to_string(),
            )
            .unwrap();

        assert_eq!(
            response[0]["error"]["message"],
            "Pending approval not found"
        );
        assert!(
            !response
                .iter()
                .any(|message| message["method"] == "item/agentMessage/delta")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("README.md")).expect("read readme"),
            "before"
        );
        let store = SessionStore::open(&db_path).expect("reopen store");
        assert_eq!(
            store.get_turn(&turn.turn_id).expect("turn").status,
            singularity_protocol::TurnStatus::Blocked
        );
        assert!(
            store
                .list_trace(thread_id)
                .expect("trace list")
                .into_iter()
                .all(|event| event.component != "agent_loop")
        );
    }
}

#[test]
fn app_server_maps_store_boundary_failures_to_json_rpc_errors() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let missing_turn_thread = server
        .handle_json(
            r#"{"method":"turn/start","id":2,"params":{"threadId":"missing","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();
    assert_eq!(
        missing_turn_thread[0]["error"]["message"],
        "Thread not found"
    );

    let request = ApprovalRequest::new("approval_public", "session_1", "task_1", "write_file");
    let request_message = serde_json::json!({
        "method": "approval/request",
        "id": 3,
        "params": request,
    });
    let public_request = server.handle_json(&request_message.to_string()).unwrap();

    assert_eq!(public_request[0]["error"]["code"], -32600);
    assert_eq!(
        public_request[0]["error"]["message"],
        "approval/request is internal to the Rust AgentLoop approval ledger"
    );

    let missing_artifact = server
        .handle_json(r#"{"method":"artifact/fetch","id":4,"params":{"artifactId":"missing"}}"#)
        .unwrap();
    assert_eq!(
        missing_artifact[0]["error"]["message"],
        "Artifact not found"
    );
}

#[test]
fn turn_start_missing_thread_fails_before_turn_creation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let missing_thread = server
        .handle_json(
            r#"{"method":"turn/start","id":2,"params":{"threadId":"missing","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();

    assert_eq!(missing_thread[0]["error"]["message"], "Thread not found");
}

#[test]
fn turn_lifecycle_interrupt_on_terminal_turn_is_idempotent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "completed")
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            singularity_protocol::TurnStatus::Completed,
            "completed",
        )
        .expect("completed turn");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":2,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":3,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();

    assert_eq!(interrupted[0]["result"]["status"], "completed");
    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "completed");
    assert_eq!(status_result["turn"]["agent_loop_status"], "completed");
}

#[test]
fn app_server_binary_errors_are_valid_json_rpc_lines() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let mut child = Command::new(app_server_bin())
        .env("SINGULARITY_APP_SERVER_DB", &db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"{\"method\":\"initialize\",\"id\":\"quoted-id\",\"params\":\"bad\"}\n")
        .expect("write invalid params");
    drop(stdin);
    let output = child.wait_with_output().expect("app-server output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let first_line = stdout.lines().next().expect("error line");
    let value: serde_json::Value = serde_json::from_str(first_line).expect("valid json error");

    assert_eq!(value["id"], "quoted-id");
    assert_eq!(value["error"]["code"], -32603);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid type")
    );
}

fn app_server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_singularity_app_server").unwrap_or_else(|_| {
        let mut path = workspace_root();
        path.push("target");
        path.push("debug");
        path.push(format!(
            "singularity_app_server{}",
            std::env::consts::EXE_SUFFIX
        ));
        path.to_string_lossy().to_string()
    })
}

fn result_message(messages: &[serde_json::Value]) -> &serde_json::Value {
    messages
        .iter()
        .find_map(|message| message.get("result"))
        .expect("json-rpc result")
}
