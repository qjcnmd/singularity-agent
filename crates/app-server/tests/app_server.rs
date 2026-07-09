use singularity_agent::PythonSidecarConfig;
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

fn test_python_bin() -> String {
    let root = workspace_root();
    let candidates = [
        root.join(".venv").join("Scripts").join("python.exe"),
        root.join(".venv").join("bin").join("python"),
    ];
    candidates
        .into_iter()
        .find(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python".to_string())
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
    let turn_result = result_message(&turn);
    let turn_id = turn_result["turn"]["turn_id"].as_str().unwrap();

    assert_eq!(turn_result["turn"]["agent_loop_status"], "not_migrated");
    assert!(
        turn.iter()
            .any(|message| message["method"] == "turn/started")
    );
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/started")
    );
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/completed")
    );

    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":51,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(result_message(&status)["turn"]["turn_id"], turn_id);

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
        1
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
fn app_server_keeps_python_oracle_explicit_after_native_cutover() {
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert_eq!(
        result_message(&turn)["turn"]["agent_loop_status"],
        "not_migrated"
    );
    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    let serialized = serde_json::to_string(&trace).expect("serialize trace");
    assert!(!serialized.contains("agent_loop"));
}

#[test]
fn python_oracle_without_sidecar_config_stays_not_migrated() {
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert_eq!(
        result_message(&turn)["turn"]["agent_loop_status"],
        "not_migrated"
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
fn approval_decision_allow_without_pending_tool_call_does_not_resume_native_turn() {
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

    assert!(
        !response
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    assert_eq!(
        result_message(&response)["decision"]["request_id"],
        request.request_id
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).expect("read readme"),
        "before"
    );
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn_after_decision = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(
        turn_after_decision.status,
        singularity_protocol::TurnStatus::Failed
    );
    let native_trace = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|event| event.component == "agent_loop")
        .expect("native trace");
    assert_eq!(
        native_trace.payload["audit_events"][0]["approval_decision"],
        "unavailable"
    );

    let duplicate = server
        .handle_json(
            &serde_json::json!({
                "method": "approval/decision",
                "id": 6,
                "params": response
                    .iter()
                    .find_map(|message| message.get("result"))
                    .expect("decision result")["decision"],
            })
            .to_string(),
        )
        .unwrap();
    assert_eq!(
        duplicate[0]["error"]["message"],
        "Pending approval not found"
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

        assert!(
            !response
                .iter()
                .any(|message| message["method"] == "item/agentMessage/delta")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("README.md")).expect("read readme"),
            "before"
        );
        let expected_status = match outcome {
            ApprovalOutcome::Defer => singularity_protocol::TurnStatus::Blocked,
            ApprovalOutcome::Allow | ApprovalOutcome::Deny => {
                singularity_protocol::TurnStatus::Failed
            }
        };
        let store = SessionStore::open(&db_path).expect("reopen store");
        assert_eq!(
            store.get_turn(&turn.turn_id).expect("turn").status,
            expected_status
        );
        let native_trace = store
            .list_trace(thread_id)
            .expect("trace list")
            .into_iter()
            .find(|event| event.component == "agent_loop")
            .expect("native trace");
        let expected_decision = match outcome {
            ApprovalOutcome::Allow => "unavailable",
            ApprovalOutcome::Deny => "denied",
            ApprovalOutcome::Defer => "deferred",
        };
        assert_eq!(
            native_trace.payload["audit_events"][0]["approval_decision"],
            expected_decision
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
            r#"{"method":"turn/start","id":2,"params":{"threadId":"missing","agentHost":"python","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();
    assert_eq!(
        missing_turn_thread[0]["error"]["message"],
        "Thread not found"
    );

    let request = ApprovalRequest::new("approval_duplicate", "session_1", "task_1", "write_file");
    let request_message = serde_json::json!({
        "method": "approval/request",
        "id": 3,
        "params": request,
    });
    server
        .handle_json(&request_message.to_string())
        .expect("first approval request");
    let duplicate = server.handle_json(&request_message.to_string()).unwrap();

    assert_eq!(duplicate[0]["error"]["code"], -32600);
    assert_eq!(duplicate[0]["error"]["message"], "Approval already exists");

    let missing_artifact = server
        .handle_json(r#"{"method":"artifact/fetch","id":4,"params":{"artifactId":"missing"}}"#)
        .unwrap();
    assert_eq!(
        missing_artifact[0]["error"]["message"],
        "Artifact not found"
    );
}

#[test]
fn turn_start_missing_thread_does_not_spawn_python_sidecar() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_probe.py"),
        "import os, pathlib\npathlib.Path(os.environ['SIDECAR_MARKER']).write_text('spawned')\n",
    )
    .expect("sidecar probe");
    let marker = dir.path().join("sidecar_spawned.txt");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_probe".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: vec![(
            "SIDECAR_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let missing_thread = server
        .handle_json(
            r#"{"method":"turn/start","id":2,"params":{"threadId":"missing","agentHost":"python","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();

    assert_eq!(missing_thread[0]["error"]["message"], "Thread not found");
    assert!(
        !marker.exists(),
        "sidecar marker should not be written for a missing Rust thread"
    );
}

#[test]
fn app_server_can_translate_python_sidecar_completion_when_enabled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let python_path = workspace_root().join("src");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "singularity.agent_host.sidecar".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(python_path),
        env: vec![(
            "SINGULARITY_SIDECAR_TEST_MODE".to_string(),
            "completed".to_string(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_result = result_message(&thread);
    let thread_id = thread_result["thread"]["thread_id"].as_str().unwrap();

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"complete through sidecar"}}]}}}}"#
        ))
        .unwrap();

    let turn_result = result_message(&turn);
    assert_eq!(turn_result["turn"]["agent_loop_status"], "completed");
    assert_eq!(turn_result["turn"]["status"], "completed");
    assert!(
        turn.iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    let response_index = turn
        .iter()
        .position(|message| message.get("result").is_some())
        .expect("turn/start response");
    let delta_index = turn
        .iter()
        .position(|message| message["method"] == "item/agentMessage/delta")
        .expect("agent delta");
    assert!(
        delta_index < response_index,
        "terminal item delta must be emitted before response so clients read it"
    );
    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":5}}}}"#
        ))
        .unwrap();
    assert!(
        trace[0]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["component"] == "python_sidecar")
    );
}

#[test]
fn app_server_resumes_python_sidecar_with_previous_session_and_thread_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_recording.py"),
        r#"
import json
import os
import pathlib
import sys

log_path = pathlib.Path(os.environ["SIDECAR_REQUEST_LOG"])
for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    with log_path.open("a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "params": params}, sort_keys=True) + "\n")
    session_id = params.get("sessionId") or "session_first"
    result = {
        "run_id": "run_fake",
        "session_id": session_id,
        "task_id": "task_fake",
        "status": "completed",
        "final_answer": f"{method} model={params.get('model', '')} session={session_id}",
        "trace_path": "run_fake",
        "events": [],
    }
    print(json.dumps({"id": message["id"], "result": result}), flush=True)
"#,
    )
    .expect("recording sidecar");
    let request_log = dir.path().join("sidecar_requests.jsonl");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_recording".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: vec![(
            "SIDECAR_REQUEST_LOG".to_string(),
            request_log.to_string_lossy().into_owned(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .unwrap();
    let resumed = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":4,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .unwrap();
    let resumed_text = resumed
        .iter()
        .filter_map(|message| message["params"]["delta"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(resumed_text.contains("agent/resume model=gpt-test session=session_first"));

    let requests = std::fs::read_to_string(request_log).expect("request log");
    let requests = requests
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("request"))
        .collect::<Vec<_>>();
    assert_eq!(requests[0]["method"], "agent/run");
    assert_eq!(requests[0]["params"]["model"], "gpt-test");
    assert_eq!(requests[1]["method"], "agent/resume");
    assert_eq!(requests[1]["params"]["sessionId"], "session_first");
    assert_eq!(requests[1]["params"]["model"], "gpt-test");
}

#[test]
fn app_server_sidecar_trace_payload_contains_only_safe_fields() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let python_path = workspace_root().join("src");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "singularity.agent_host.sidecar".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(python_path),
        env: vec![(
            "SINGULARITY_SIDECAR_TEST_MODE".to_string(),
            "completed".to_string(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"complete through sidecar"}}]}}}}"#
        ))
        .unwrap();
    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":5}}}}"#
        ))
        .unwrap();
    let event = trace[0]["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["component"] == "python_sidecar")
        .expect("python sidecar trace");
    let payload = event["payload"].as_object().expect("payload object");
    let keys = payload.keys().cloned().collect::<Vec<_>>();

    assert_eq!(
        keys,
        vec![
            "approval_count".to_string(),
            "component".to_string(),
            "model_turns".to_string(),
            "run_id".to_string(),
            "session_id".to_string(),
            "status".to_string(),
            "task_id".to_string(),
            "tool_calls".to_string(),
            "trace_path".to_string(),
        ]
    );
    let payload_text = event["payload"].to_string().to_lowercase();
    for marker in [
        "raw_prompt",
        "raw_response",
        "raw_arguments",
        "provider_response",
        "metadata",
        "api_key",
        "authorization",
        "password",
        "secret",
        "token",
    ] {
        assert!(
            !payload_text.contains(marker),
            "sidecar trace payload leaked {marker}: {payload_text}"
        );
    }
}

#[test]
fn app_server_redacts_sidecar_event_summary_before_trace_payload() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_leaky_event.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "safe final",
            "events": [{
                "event_id": "raw_prompt_event_sk-abcdefghijklmnopqrstuvwxyz",
                "event_type": "provider_response",
                "summary": "raw_prompt provider_response Authorization: Bearer abc123",
                "component": "raw_prompt",
                "severity": "secret",
                "sequence": 1
            }]
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_leaky_event".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    let serialized = serde_json::to_string(&trace).expect("serialize trace");

    assert!(serialized.contains("[redacted sensitive app-server output]"));
    assert!(serialized.contains("python_sidecar.event"));
    assert!(serialized.contains(r#""severity":"info""#));
    for marker in [
        "raw_prompt",
        "provider_response",
        "Authorization",
        "abc123",
        "sk-",
    ] {
        assert!(
            !serialized.contains(marker),
            "{marker} leaked to trace payload"
        );
    }
}

#[test]
fn app_server_redacts_sidecar_final_answer_before_item_delta() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_leaky_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "\"provider\" evaluator_only raw_prompt provider_response Authorization: Bearer abc123",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_leaky_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let delta = turn
        .iter()
        .find_map(|message| message["params"]["delta"].as_str())
        .expect("agent delta");
    let serialized = serde_json::to_string(&turn).expect("serialize turn messages");

    assert_eq!(delta, "[redacted sensitive app-server output]");
    for marker in [
        "\"provider\"",
        "evaluator_only",
        "raw_prompt",
        "provider_response",
        "Authorization",
        "abc123",
    ] {
        assert!(
            !serialized.contains(marker),
            "{marker} leaked to app-server messages"
        );
    }
}

#[test]
fn app_server_redacts_standalone_secret_values_in_final_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_standalone_secret_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "sk-abcdefghijklmnopqrstuvwxyz ghp_abcdefghijklmnopqrstuvwxyz123456 eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_standalone_secret_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let delta = turn
        .iter()
        .find_map(|message| message["params"]["delta"].as_str())
        .expect("agent delta");
    let serialized = serde_json::to_string(&turn).expect("serialize turn messages");

    assert_eq!(delta, "[redacted sensitive app-server output]");
    for marker in ["sk-", "ghp_", "eyJhbGci"] {
        assert!(
            !serialized.contains(marker),
            "{marker} leaked to app-server messages"
        );
    }
}

#[test]
fn app_server_keeps_non_secret_environment_variable_final_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_environment_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "The environment variable name is documented without a value.",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_environment_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let delta = turn
        .iter()
        .find_map(|message| message["params"]["delta"].as_str())
        .expect("agent delta");

    assert_eq!(
        delta,
        "The environment variable name is documented without a value."
    );
}

#[test]
fn app_server_keeps_safe_token_metrics_final_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_token_metrics_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "token count is 42 and token budget is 100",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_token_metrics_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let delta = turn
        .iter()
        .find_map(|message| message["params"]["delta"].as_str())
        .expect("agent delta");

    assert_eq!(delta, "token count is 42 and token budget is 100");
}

#[test]
fn app_server_does_not_emit_agent_delta_for_sidecar_transport_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_error.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "error": {
            "code": -32603,
            "message": "unrecognized sensitive payload shape abc123"
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_error".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let result = result_message(&turn);
    let serialized = serde_json::to_string(&turn).expect("serialize turn messages");

    assert_eq!(result["turn"]["status"], "failed");
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    assert!(!serialized.contains("abc123"));
}

#[test]
fn app_server_does_not_emit_agent_delta_for_sidecar_failed_final_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_failed_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "failed",
            "final_answer": "failure summary with provider_response abc123",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_failed_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let result = result_message(&turn);
    let serialized = serde_json::to_string(&turn).expect("serialize turn messages");

    assert_eq!(result["turn"]["status"], "failed");
    assert_eq!(result["turn"]["agent_loop_status"], "failed");
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    assert!(!serialized.contains("provider_response"));
    assert!(!serialized.contains("abc123"));
}

#[test]
fn app_server_does_not_emit_agent_delta_for_empty_completed_final_answer() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_empty_final.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({
        "id": message["id"],
        "result": {
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "status": "completed",
            "final_answer": "   ",
            "events": []
        }
    }), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_empty_final".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
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

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    let result = result_message(&turn);

    assert_eq!(result["turn"]["status"], "completed");
    assert_eq!(result["turn"]["agent_loop_status"], "completed");
    assert!(
        !turn
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
}

#[test]
fn turn_lifecycle_status_and_interrupt_cancel_active_sidecar_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_lifecycle.py"),
        r#"
import json
import os
import pathlib
import sys
import time

log_path = pathlib.Path(os.environ["SIDECAR_REQUEST_LOG"])
status_calls = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    with log_path.open("a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "params": params}, sort_keys=True) + "\n")
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/status":
        status = "completed" if status_calls else "running"
        status_calls += 1
        result = {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": status}
        if status == "completed":
            result["final_answer"] = "done"
        print(json.dumps({"id": message["id"], "result": result}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "status": "cancel_requested"}}), flush=True)
    else:
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "status": "failed"}}), flush=True)
"#,
    )
    .expect("lifecycle sidecar");
    let request_log = dir.path().join("sidecar_requests.jsonl");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_lifecycle".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: vec![(
            "SIDECAR_REQUEST_LOG".to_string(),
            request_log.to_string_lossy().into_owned(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_result = result_message(&turn);
    let turn_id = turn_result["turn"]["turn_id"].as_str().unwrap();

    assert_eq!(turn_result["turn"]["status"], "running");
    assert_eq!(turn_result["turn"]["agent_loop_status"], "running");

    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "running");
    assert_eq!(status_result["turn"]["agent_loop_status"], "running");

    let completed = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let completed_result = result_message(&completed);
    assert_eq!(completed_result["turn"]["status"], "completed");
    assert_eq!(completed_result["turn"]["agent_loop_status"], "completed");
    let status_delta = completed
        .iter()
        .find_map(|message| message["params"]["delta"].as_str())
        .expect("completed status emits agent message delta");
    assert_eq!(status_delta, "done");

    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":6,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    assert_eq!(
        trace[0]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["component"] == "python_sidecar")
            .count(),
        1
    );
    assert!(
        trace[0]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["payload"]["transition"] == "sidecar_started")
    );

    let after_terminal = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":7,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(
        result_message(&after_terminal)["turn"]["status"],
        "completed"
    );
    assert!(
        !after_terminal
            .iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
    );
    let requests = std::fs::read_to_string(request_log).expect("request log");
    let status_calls = requests
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("request"))
        .filter(|request| request["method"] == "agent/status")
        .count();
    assert_eq!(status_calls, 2);
}

#[test]
fn turn_lifecycle_interrupt_cancel_active_sidecar_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_interrupt.py"),
        r#"
import json
import os
import pathlib
import sys

log_path = pathlib.Path(os.environ["SIDECAR_REQUEST_LOG"])
for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    with log_path.open("a", encoding="utf-8") as log:
        log.write(json.dumps({"method": method, "params": params}, sort_keys=True) + "\n")
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    else:
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("interrupt sidecar");
    let request_log = dir.path().join("sidecar_requests.jsonl");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_interrupt".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: vec![(
            "SIDECAR_REQUEST_LOG".to_string(),
            request_log.to_string_lossy().into_owned(),
        )],
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"].as_str().unwrap();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    assert_eq!(interrupted[0]["result"]["status"], "interrupted");
    assert_eq!(
        interrupted[0]["result"]["agent_loop_status"],
        "cancel_requested"
    );
    let requests = std::fs::read_to_string(request_log).expect("request log");
    let methods = requests
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("request"))
        .map(|request| request["method"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert!(methods.contains(&"agent/cancel".to_string()));

    let trace = server
        .handle_json(&format!(
            r#"{{"method":"trace/tail","id":5,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    assert!(
        trace[0]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["payload"]["transition"] == "cancel_requested")
    );
}

#[test]
fn turn_lifecycle_cancel_not_found_does_not_record_interrupted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_not_found.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "error": {"code": -32004, "message": "Unknown active run: run_active"}}), flush=True)
"#,
    )
    .expect("cancel not found sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_not_found".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    assert!(
        interrupted[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown active run")
    );
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn remains");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Running);
    assert_eq!(turn.agent_loop_status, "running");
    assert_eq!(
        store
            .get_active_sidecar_run(&turn_id)
            .expect("active row remains")
            .status,
        "running"
    );
}

#[test]
fn turn_lifecycle_cancel_not_found_result_does_not_record_interrupted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_not_found_result.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "not_found"}}), flush=True)
"#,
    )
    .expect("cancel not found result sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_not_found_result".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    assert!(interrupted[0]["error"].is_object());
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn remains");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Running);
    assert_eq!(turn.agent_loop_status, "running");
    assert_eq!(
        store
            .get_active_sidecar_run(&turn_id)
            .expect("active row remains")
            .status,
        "running"
    );
}

#[test]
fn turn_lifecycle_repeated_interrupt_is_idempotent_while_cancel_pending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_repeat_interrupt.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
"#,
    )
    .expect("repeat interrupt sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_repeat_interrupt".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"].as_str().unwrap();

    for id in [4, 5] {
        let interrupted = server
            .handle_json(&format!(
                r#"{{"method":"turn/interrupt","id":{id},"params":{{"turnId":"{turn_id}"}}}}"#
            ))
            .unwrap();

        assert_eq!(interrupted[0]["result"]["status"], "interrupted");
        assert_eq!(
            interrupted[0]["result"]["agent_loop_status"],
            "cancel_requested"
        );
    }
}

#[test]
fn turn_lifecycle_drop_preserves_cancelled_turn_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_interrupt.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    else:
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("interrupt sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_interrupt".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let turn_id = {
        let mut server = AppServer::new(store).with_python_sidecar(config);
        server
            .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"method":"initialized","params":{}}"#)
            .unwrap();
        let thread = server
            .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
            .unwrap();
        let thread_id = result_message(&thread)["thread"]["thread_id"]
            .as_str()
            .unwrap();
        let turn = server
            .handle_json(&format!(
                r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
            ))
            .unwrap();
        let turn_id = result_message(&turn)["turn"]["turn_id"]
            .as_str()
            .unwrap()
            .to_string();
        server
            .handle_json(&format!(
                r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
            ))
            .unwrap();
        turn_id
    };

    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Interrupted);
    assert_eq!(turn.agent_loop_status, "cancelled");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
    assert!(
        store
            .list_trace(&turn.thread_id)
            .expect("trace list")
            .iter()
            .any(|event| event.payload["transition"] == "interrupted")
    );
}

#[test]
fn turn_lifecycle_status_error_after_cancel_keeps_interrupted_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_then_eof.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    elif method == "agent/status":
        sys.exit(0)
"#,
    )
    .expect("cancel then eof sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_then_eof".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "interrupted");
    assert_eq!(status_result["turn"]["agent_loop_status"], "cancelled");
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
}

#[test]
fn turn_lifecycle_status_success_after_cancel_keeps_interrupted_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_then_completed.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    elif method == "agent/status":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "completed", "final_answer": "late done"}}), flush=True)
"#,
    )
    .expect("cancel then completed sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_then_completed".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "interrupted");
    assert_eq!(status_result["turn"]["agent_loop_status"], "cancelled");
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
    let traces = store.list_trace(thread_id).expect("trace list");
    assert!(traces.iter().any(|event| {
        event.component == "python_sidecar" && event.payload["status"] == "cancelled"
    }));
    assert!(
        !traces
            .iter()
            .any(|event| event.component == "python_sidecar"
                && event.payload["status"] == "completed")
    );
}

#[test]
fn turn_lifecycle_status_failed_after_cancel_keeps_cancelled_mapping() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_then_failed.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    elif method == "agent/status":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "failed", "final_answer": "late failure"}}), flush=True)
"#,
    )
    .expect("cancel then failed sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_then_failed".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "interrupted");
    assert_eq!(status_result["turn"]["agent_loop_status"], "cancelled");
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
    let traces = store.list_trace(thread_id).expect("trace list");
    assert!(traces.iter().any(|event| {
        event.component == "python_sidecar" && event.payload["status"] == "cancelled"
    }));
    assert!(!traces
        .iter()
        .any(|event| event.component == "python_sidecar" && event.payload["status"] == "failed"));
}

#[test]
fn turn_lifecycle_status_after_cancel_requested_keeps_active_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_pending.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    elif method == "agent/status":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
"#,
    )
    .expect("cancel pending sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_pending".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "interrupted");
    assert_eq!(
        status_result["turn"]["agent_loop_status"],
        "cancel_requested"
    );
    let store = SessionStore::open(&db_path).expect("reopen store");
    let active = store
        .get_active_sidecar_run(&turn_id)
        .expect("active run remains while cancel is pending");
    assert_eq!(active.status, "cancel_requested");
}

#[test]
fn turn_lifecycle_status_running_after_cancel_keeps_cancel_requested_active_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_then_running.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "cancel_requested"}}), flush=True)
    elif method == "agent/status":
        print(json.dumps({"id": message["id"], "result": {"run_id": params["runId"], "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("cancel then running sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_then_running".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":5,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "interrupted");
    assert_eq!(
        status_result["turn"]["agent_loop_status"],
        "cancel_requested"
    );
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert_eq!(
        store
            .get_active_sidecar_run(&turn_id)
            .expect("active run remains while cancel unwinds")
            .status,
        "cancel_requested"
    );
}

#[test]
fn turn_lifecycle_cancel_transport_failure_does_not_record_cancelled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_error.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    if method == "agent/run":
        print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
    elif method == "agent/cancel":
        print(json.dumps({"id": message["id"], "error": {"code": -32603, "message": "raw_prompt provider_response Authorization: Bearer abc123"}}), flush=True)
"#,
    )
    .expect("cancel error sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_cancel_error".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":4,"params":{{"turnId":"{turn_id}"}}}}"#
        ))
        .unwrap();

    let serialized = serde_json::to_string(&interrupted).expect("serialize interrupt response");
    assert_eq!(
        interrupted[0]["error"]["message"],
        "[redacted sensitive app-server output]"
    );
    for marker in ["raw_prompt", "provider_response", "Authorization", "abc123"] {
        assert!(
            !serialized.contains(marker),
            "{marker} leaked to interrupt response"
        );
    }
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Running);
    assert_eq!(turn.agent_loop_status, "running");
    assert_eq!(
        store
            .get_active_sidecar_run(&turn_id)
            .expect("active run")
            .status,
        "running"
    );
}

#[test]
fn turn_lifecycle_active_row_without_process_handle_returns_durable_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .register_active_sidecar_run(
            &turn.turn_id,
            "run_orphan",
            "session_orphan",
            "task_orphan",
            "running",
        )
        .expect("active run");
    let mut server = AppServer::new(store);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();

    let status = server
        .handle_json(&format!(
            r#"{{"method":"turn/status","id":2,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();
    let interrupt = server
        .handle_json(&format!(
            r#"{{"method":"turn/interrupt","id":3,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();

    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "running");
    assert_eq!(status_result["turn"]["agent_loop_status"], "running");
    assert_eq!(interrupt[0]["result"]["status"], "running");
    assert_eq!(interrupt[0]["result"]["agent_loop_status"], "running");
    drop(server);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Running);
    assert_eq!(turn.agent_loop_status, "running");
}

#[test]
fn thread_delete_cleans_active_sidecar_run_before_deleting_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_running.py"),
        r#"
import json
import sys
for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("running sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_running".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    let deleted = server
        .handle_json(&format!(
            r#"{{"method":"thread/delete","id":4,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();

    assert_eq!(deleted[0]["result"]["deleted"], true);
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert!(store.get_thread(&thread_id).is_err());
    assert!(store.get_turn(&turn_id).is_err());
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
}

#[test]
fn server_shutdown_cleans_active_sidecar_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_running.py"),
        r#"
import json
import sys
for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("running sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_running".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut server = AppServer::new(store).with_python_sidecar(config);
    server
        .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();
    let turn = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
        ))
        .unwrap();
    let turn_id = result_message(&turn)["turn"]["turn_id"]
        .as_str()
        .unwrap()
        .to_string();

    let shutdown = server
        .handle_json(r#"{"method":"server/shutdown","id":4,"params":{}}"#)
        .unwrap();

    assert_eq!(shutdown[0]["result"]["shutdown"], true);
    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Failed);
    assert_eq!(turn.agent_loop_status, "failed");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
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
fn turn_lifecycle_drop_cleans_active_sidecar_run_on_app_server_exit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_running.py"),
        r#"
import json
import sys
for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({"id": message["id"], "result": {"run_id": "run_active", "session_id": "session_active", "task_id": "task_active", "status": "running"}}), flush=True)
"#,
    )
    .expect("running sidecar");
    let config = PythonSidecarConfig {
        python_bin: test_python_bin(),
        module: "sidecar_running".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let turn_id = {
        let mut server = AppServer::new(store).with_python_sidecar(config);
        server
            .handle_json(r#"{"method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"method":"initialized","params":{}}"#)
            .unwrap();
        let thread = server
            .handle_json(r#"{"method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#)
            .unwrap();
        let thread_id = result_message(&thread)["thread"]["thread_id"]
            .as_str()
            .unwrap();
        let turn = server
            .handle_json(&format!(
                r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"python","input":[{{"type":"text","text":"long run"}}]}}}}"#
            ))
            .unwrap();
        result_message(&turn)["turn"]["turn_id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let store = SessionStore::open(&db_path).expect("reopen store");
    let turn = store.get_turn(&turn_id).expect("turn");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Failed);
    assert_eq!(turn.agent_loop_status, "failed");
    assert!(store.get_active_sidecar_run(&turn_id).is_err());
    assert!(
        store
            .list_trace(&turn.thread_id)
            .expect("trace list")
            .iter()
            .any(|event| event.payload["transition"] == "cleanup")
    );
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
