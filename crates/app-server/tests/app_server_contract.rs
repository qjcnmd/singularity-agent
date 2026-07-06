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
            r#"{{"method":"turn/start","id":5,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"hello"}}]}}}}"#
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
            r#"{"method":"turn/start","id":2,"params":{"threadId":"missing","input":[{"type":"text","text":"hello"}]}}"#,
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
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"complete through sidecar"}}]}}}}"#
        ))
        .unwrap();

    let turn_result = result_message(&turn);
    assert_eq!(turn_result["turn"]["agent_loop_status"], "completed");
    assert_eq!(turn_result["turn"]["status"], "completed");
    assert!(
        turn.iter()
            .any(|message| message["method"] == "item/agentMessage/delta")
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
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .unwrap();
    let resumed = server
        .handle_json(&format!(
            r#"{{"method":"turn/start","id":4,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"second"}}]}}}}"#
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
fn app_server_sidecar_trace_projection_contains_only_safe_fields() {
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
            r#"{{"method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"complete through sidecar"}}]}}}}"#
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
            "component".to_string(),
            "run_id".to_string(),
            "session_id".to_string(),
            "status".to_string(),
            "task_id".to_string(),
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
