//! AppServer protocol、approval continuation、recovery 和 sandbox 边界测试。

use singularity_app_server::{AppServer, AppServerError};
use singularity_model::ProviderConfigSnapshot;
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, PermissionResource, ToolId,
    WorkspaceRelativePath,
};
use singularity_store::{SessionStore, StoreError};
#[cfg(windows)]
use std::collections::VecDeque;
use std::io::Write;
#[cfg(windows)]
use std::io::{BufRead, BufReader};
#[cfg(windows)]
use std::net::TcpListener;
#[cfg(windows)]
use std::process::{Child, ChildStdin};
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::sync::mpsc::{self, Receiver};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::{Duration, Instant};

fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("valid tool id")
}

fn workspace_resource(value: &str) -> PermissionResource {
    PermissionResource::WorkspacePath(
        WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
    )
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn app_server(store: SessionStore) -> AppServer {
    AppServer::new(store, ProviderConfigSnapshot::capture(|_| None))
}

// Request workers must use the typed reopen of an initialized file store;
// they cannot silently create an unrelated in-memory database.
#[test]
fn request_worker_reopen_requires_initialized_file_store() {
    let server = app_server(SessionStore::open(":memory:").expect("open store"));

    assert!(matches!(
        server.turn_worker(),
        Err(AppServerError::Store(StoreError::InvalidState(message)))
            if message.contains("trusted store reopen")
    ));
}

#[test]
fn request_worker_reopens_the_initialized_file_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let server = app_server(store);
    let mut worker = server.turn_worker().expect("trusted request worker reopen");

    let response = worker
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/list","id":1,"params":{}}"#)
        .expect("thread list");
    assert_eq!(
        response[0]["result"]["threads"][0]["thread_id"],
        thread.thread_id
    );
}

fn configured_app_server(store: SessionStore) -> AppServer {
    AppServer::new(
        store,
        ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_MODEL" => Some("test-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key".to_string()),
            _ => None,
        }),
    )
}

fn expected_eval_blocker_kind() -> &'static str {
    if cfg!(windows) {
        "workspace_preparation"
    } else {
        "environment"
    }
}

fn expected_pre_agent_blocked_stage_statuses() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("blocked", "skipped")
    } else {
        ("skipped", "blocked")
    }
}

fn approval_checkpoint(request: &ApprovalRequest, tool_call_id: &str) -> serde_json::Value {
    serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": tool_call_id,
        "tool_name": &request.action,
        "raw_arguments": "{}",
        "resources": &request.resources,
        "checkpoint_version": 1,
        "messages": [{"role":"assistant","content":"","tool_calls":[{"tool_call_id":tool_call_id,"tool_name":&request.action,"arguments":{},"raw_arguments":"{}","parse_status":"valid","validation_errors":[]}]}],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {
            "workspace_mutated": false,
            "verified_after_last_mutation": false,
            "successful_command_count": 0,
            "required_command_counts": {},
            "satisfied_command_counts": {},
            "unresolved_failures": []
        },
        "last_completion_error": null
    })
}
#[test]
fn app_server_enforces_initialize_and_emits_item_events() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);

    let not_initialized = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":1,"params":{}}"#)
        .unwrap();
    assert_eq!(not_initialized[0]["error"]["message"], "Not initialized");

    let unknown = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/unknown","id":11,"params":{}}"#)
        .unwrap();
    assert_eq!(unknown[0]["error"]["code"], -32601);
    assert_eq!(unknown[0]["error"]["message"], "Method not found");

    let initialized = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":2,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    assert_eq!(initialized[0]["result"]["platformFamily"], "local");

    let before_initialized = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":30,"params":{}}"#)
        .unwrap();
    assert_eq!(before_initialized[0]["error"]["message"], "Not initialized");

    let duplicate = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":3,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    assert_eq!(duplicate[0]["error"]["message"], "Already initialized");

    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let capabilities = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"server/capabilities","id":31,"params":{}}"#)
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
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":32,"params":{"eventTypes":["thread/started","turn/started"]}}"#,
        )
        .unwrap();
    assert_eq!(
        subscription[0]["result"]["eventTypes"],
        serde_json::json!(["thread/started", "turn/started"])
    );

    let thread = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":4,"params":{"model":"gpt-test","sandboxMode":"read-only","approvalPolicy":"never"}}"#)
        .unwrap();
    let thread_result = result_message(&thread);
    let thread_id = thread_result["thread"]["thread_id"].as_str().unwrap();
    assert_eq!(thread_result["thread"]["sandboxMode"], "read-only");
    assert_eq!(thread_result["thread"]["approvalPolicy"], "never");
    assert_eq!(
        thread_result["thread"]["cwd"],
        std::env::current_dir()
            .expect("current dir")
            .canonicalize()
            .expect("canonical current dir")
            .to_string_lossy()
            .as_ref()
    );
    assert!(
        thread
            .iter()
            .any(|message| message["method"] == "thread/started")
    );

    let list = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/list","id":41,"params":{}}"#)
        .unwrap();
    assert_eq!(list[0]["result"]["threads"][0]["thread_id"], thread_id);

    let read = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":42,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(read[0]["result"]["thread"]["thread_id"], thread_id);

    let turn = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":5,"params":{{"threadId":"{thread_id}","agentHost":"alternate","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();
    assert_eq!(turn[0]["error"]["code"], -32602);
    assert_eq!(turn[0]["error"]["message"], "Invalid params");

    let missing_trace_list = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"trace/list","id":6,"params":{"runId":"missing"}}"#,
        )
        .unwrap();
    assert_eq!(
        missing_trace_list[0]["error"]["message"],
        "Trace run not found"
    );

    let missing_trace_show = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"trace/show","id":7,"params":{"eventId":"missing"}}"#,
        )
        .unwrap();
    assert_eq!(
        missing_trace_show[0]["error"]["message"],
        "Trace event not found"
    );

    let trace_tail = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"trace/tail","id":8,"params":{{"runId":"{thread_id}","limit":1}}}}"#
        ))
        .unwrap();
    assert_eq!(
        trace_tail[0]["result"]["events"].as_array().unwrap().len(),
        1
    );
    let trace_tail_with_offset = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"trace/tail","id":82,"params":{{"runId":"{thread_id}","limit":1,"offset":1}}}}"#
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
            r#"{{"jsonrpc":"2.0","method":"trace/list","id":81,"params":{{"runId":"{thread_id}","limit":1,"offset":99}}}}"#
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
            r#"{{"jsonrpc":"2.0","method":"thread/archive","id":43,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(archived[0]["result"]["thread"]["status"], "archived");

    let rejected_turn = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":431,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"must resume first"}}]}}}}"#
        ))
        .unwrap();
    assert_eq!(
        rejected_turn[0]["error"]["message"],
        "Thread is archived; resume it before starting a turn"
    );

    let invalid_resume = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":433,"params":{{"threadId":"{thread_id}","sandboxMode":"workspace-write"}}}}"#
        ))
        .unwrap();
    assert_eq!(invalid_resume[0]["error"]["code"], -32602);
    let unchanged = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":434,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(unchanged[0]["result"]["thread"]["status"], "archived");
    assert_eq!(unchanged[0]["result"]["thread"]["sandboxMode"], "read-only");
    assert_eq!(unchanged[0]["result"]["thread"]["approvalPolicy"], "never");

    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":432,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(resumed[0]["result"]["thread"]["status"], "active");
    assert_eq!(resumed[0]["result"]["thread"]["sandboxMode"], "read-only");
    assert_eq!(resumed[0]["result"]["thread"]["approvalPolicy"], "never");

    let forked = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/fork","id":44,"params":{{"threadId":"{thread_id}","model":"gpt-fork"}}}}"#
        ))
        .unwrap();
    assert_eq!(forked[0]["result"]["sourceThreadId"], thread_id);
    assert_eq!(forked[0]["result"]["thread"]["model"], "gpt-fork");
    assert_eq!(
        forked[0]["result"]["thread"]["cwd"],
        thread_result["thread"]["cwd"]
    );
    assert_eq!(forked[0]["result"]["thread"]["sandboxMode"], "read-only");
    assert_eq!(forked[0]["result"]["thread"]["approvalPolicy"], "never");

    let overridden_fork = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/fork","id":45,"params":{{"threadId":"{thread_id}","sandboxMode":"workspace-write","approvalPolicy":"on-request"}}}}"#
        ))
        .unwrap();
    assert_eq!(
        overridden_fork[0]["result"]["thread"]["sandboxMode"],
        "workspace-write"
    );
    assert_eq!(
        overridden_fork[0]["result"]["thread"]["approvalPolicy"],
        "on-request"
    );

    let deleted = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/delete","id":46,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(deleted[0]["result"]["deleted"], true);
}

#[test]
fn legacy_threads_without_an_absolute_workspace_fail_closed_on_resume_and_turn_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let store = SessionStore::open(&db_path).expect("reopen store");
    let missing = store.create_thread(None, None).expect("missing cwd thread");
    store
        .update_thread_status(
            &missing.thread_id,
            singularity_protocol::ThreadStatus::Archived,
        )
        .expect("archive missing cwd thread");
    let relative = store
        .create_thread(None, Some("relative-workspace"))
        .expect("relative cwd thread");
    store
        .update_thread_status(
            &relative.thread_id,
            singularity_protocol::ThreadStatus::Archived,
        )
        .expect("archive relative cwd thread");
    let active_missing = store
        .create_thread(None, None)
        .expect("active missing cwd thread");
    drop(store);

    for thread_id in [&missing.thread_id, &relative.thread_id] {
        let response = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"thread/resume","id":2,"params":{{"threadId":"{thread_id}"}}}}"#
            ))
            .expect("resume response");
        assert!(
            response[0]["error"]["message"]
                .as_str()
                .expect("error message")
                .contains("absolute workspace")
        );
    }

    let turn = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{}","input":[{{"type":"text","text":"do not run"}}]}}}}"#,
            active_missing.thread_id
        ))
        .expect("turn response");
    assert!(
        turn[0]["error"]["message"]
            .as_str()
            .expect("turn error")
            .contains("absolute workspace")
    );

    let store = SessionStore::open(&db_path).expect("reopen store");
    assert_eq!(
        store
            .get_thread(&missing.thread_id)
            .expect("missing thread")
            .status,
        singularity_protocol::ThreadStatus::Archived
    );
    assert_eq!(
        store
            .get_thread(&relative.thread_id)
            .expect("relative thread")
            .status,
        singularity_protocol::ThreadStatus::Archived
    );
}

#[test]
fn thread_read_reports_invalid_params_and_keeps_the_connection_usable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    let started = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{}}"#)
        .expect("thread start");
    let thread_id = result_message(&started)["thread"]["thread_id"]
        .as_str()
        .expect("thread id");

    for request in [
        r#"{"jsonrpc":"2.0","method":"thread/read","id":3,"params":{"limit":1}}"#.to_string(),
        format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":4,"params":{{"threadId":"{thread_id}","limit":"bad"}}}}"#
        ),
        format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":5,"params":{{"threadId":"{thread_id}","unknown":true}}}}"#
        ),
    ] {
        let response = server
            .handle_json(&request)
            .expect("invalid params response");
        assert_eq!(response[0]["error"]["code"], -32602);
    }

    let valid = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":6,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .expect("valid read after invalid params");
    assert_eq!(valid[0]["result"]["thread"]["thread_id"], thread_id);
}

#[test]
fn app_server_binary_reports_only_redacted_provider_configuration() {
    let dir = tempfile::tempdir().expect("temp dir");
    let api_key = "sentinel-provider-api-key";
    let base_url = "https://sentinel-provider.example/v1";
    let model = "sentinel-provider-model";
    let mut child = Command::new(env!("CARGO_BIN_EXE_singularity_app_server"))
        .current_dir(dir.path())
        .env(
            "SINGULARITY_APP_SERVER_DB",
            dir.path().join("sessions.sqlite3"),
        )
        .env("SINGULARITY_API_KEY", api_key)
        .env("SINGULARITY_BASE_URL", base_url)
        .env("SINGULARITY_MODEL", model)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let mut stdin = child.stdin.take().expect("app-server stdin");
    for line in [
        r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"agent/capability","id":2,"params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"server/shutdown","id":3,"params":{}}"#,
    ] {
        writeln!(stdin, "{line}").expect("write app-server request");
    }
    drop(stdin);

    let output = child.wait_with_output().expect("wait for app-server");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 app-server output");
    let capability = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["id"] == 2)
        .expect("agent capability response");
    let provider = &capability["result"]["providerConfiguration"];

    assert_eq!(provider["source"], "process_env");
    assert!(
        provider["snapshotId"]
            .as_str()
            .is_some_and(|value| value.starts_with("provider_snapshot_"))
    );
    assert_eq!(provider["configured"], true);
    assert!(provider["blocker"].is_null());
    assert_eq!(provider["apiKeyPresent"], true);
    assert_eq!(provider["baseUrlPresent"], true);
    assert_eq!(provider["modelPresent"], true);
    for sentinel in [api_key, base_url, model] {
        assert!(!stdout.contains(sentinel));
    }
}

#[test]
fn app_server_reuses_one_provider_snapshot_for_capability_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let snapshot = ProviderConfigSnapshot::capture(|name| match name {
        "SINGULARITY_MODEL" => Some("snapshot-model".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://snapshot.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("snapshot-secret".to_string()),
        _ => None,
    });
    let expected_snapshot_id = snapshot.snapshot_id().to_string();
    let mut server = AppServer::new(store, snapshot);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    for id in [2, 3] {
        let capability = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"agent/capability","id":{id},"params":{{}}}}"#
            ))
            .unwrap();
        let provider = &capability[0]["result"]["providerConfiguration"];
        assert_eq!(provider["snapshotId"], expected_snapshot_id);
        assert_eq!(provider["configured"], true);
        assert!(provider["blocker"].is_null());
    }
}
#[cfg(windows)]
#[test]
fn app_server_reports_agent_loop_capability_as_available() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let capability = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"agent/capability","id":2,"params":{}}"#)
        .unwrap();

    assert_eq!(capability[0]["result"]["agentLoop"]["available"], true);
    assert_eq!(capability[0]["result"]["agentLoop"]["status"], "completed");
    assert!(
        capability[0]["result"]["agentLoop"]["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let provider = &capability[0]["result"]["providerConfiguration"];
    assert!(provider["source"].is_null() || provider["source"].is_string());
    assert!(
        provider["snapshotId"]
            .as_str()
            .is_some_and(|value| value.starts_with("provider_snapshot_"))
    );
    assert!(provider["configured"].is_boolean());
    assert!(provider["blocker"].is_null() || provider["blocker"].is_string());
    assert!(provider["apiKeyPresent"].is_boolean());
    assert!(provider["baseUrlPresent"].is_boolean());
    assert!(provider["modelPresent"].is_boolean());
}

#[cfg(not(windows))]
#[test]
fn app_server_reports_default_agent_loop_backend_as_unavailable_off_windows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let capability = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"agent/capability","id":2,"params":{}}"#)
        .unwrap();

    assert_eq!(capability[0]["result"]["agentLoop"]["available"], false);
    assert_eq!(capability[0]["result"]["agentLoop"]["status"], "blocked");
    assert!(
        capability[0]["result"]["agentLoop"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker == "strict_command_sandbox_unavailable")
    );
}

#[test]
fn app_server_eval_run_writes_blocked_agent_loop_result_artifacts_without_fallback() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    let manifest = dir.path().join("eval.json");
    let output_root = dir.path().join("eval-output");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v5",
  "trial_count": 1,
  "tasks": [
    {
      "task_id": "fixture_eval",
      "description": "Exercise a blocked typed evaluation task.",
      "capabilities": ["provider_failure_attribution"],
      "workspace": {
        "source": {"type": "local", "path": "missing-source"}
      },
      "agent": {
        "instructions": "Finish the task.",
        "allowed_paths": ["README.md"],
        "required_tool_capabilities": [
          {"capability": "workspace_read", "minimum_version": 1}
        ]
      },
      "evaluator": {
        "baseline": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "public": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "hidden": {"commands": [{"argv": ["git", "status", "--porcelain"]}]}
      }
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "jsonrpc": "2.0", "method": "eval/run",
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

    assert_eq!(result["runner"], "agent_loop");
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["blocker"], expected_eval_blocker_kind());
    assert_eq!(result["evaluation_passed"], false);
    assert_eq!(result["tasks"][0]["trials"][0]["agent_completed"], false);
    assert_eq!(result["tasks"][0]["trials"][0]["tests_passed"], false);
    assert_eq!(
        result["tasks"][0]["trial_diagnostics"][0]["smoke_command_satisfied"],
        true
    );
    assert_eq!(
        result["tasks"][0]["trial_diagnostics"][0]["local_process_fallback_count"],
        0
    );
    let result_path = result["result_path"].as_str().expect("result path");
    let report_path = result["report_path"].as_str().expect("report path");
    let evidence_path = result["evidence_path"].as_str().expect("evidence path");
    assert!(std::path::Path::new(result_path).exists());
    assert!(std::path::Path::new(report_path).exists());
    assert!(std::path::Path::new(evidence_path).exists());
    let (expected_baseline_status, _) = expected_pre_agent_blocked_stage_statuses();
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(result_path).expect("result json"))
            .expect("result payload");
    assert_eq!(payload["schema_version"], "evaluation.result/v6");
    assert_eq!(payload["status"], "blocked");
    assert_eq!(
        payload["tasks"][0]["blocker"]["kind"],
        expected_eval_blocker_kind()
    );
    assert_eq!(
        payload["tasks"][0]["trials"][0]["stages"]["baseline"]["status"],
        expected_baseline_status
    );
    assert_eq!(
        payload["tasks"][0]["trials"][0]["stages"]["public"]["status"],
        "skipped"
    );
    assert_eq!(payload["summary"]["agent_scored_trial_count"], 0);
    let evidence_json = std::fs::read_to_string(evidence_path).expect("evidence json");
    assert!(!evidence_json.contains(&dir.path().to_string_lossy().to_string()));
    assert!(!evidence_json.contains("missing-source"));
    let evidence: serde_json::Value =
        serde_json::from_str(&evidence_json).expect("evidence payload");
    assert_eq!(evidence["schema_version"], "evaluation.evidence/v2");
    assert_eq!(evidence["denominator_task_count"], 1);
    assert_eq!(evidence["denominator_trial_count"], 1);
    assert_eq!(evidence["tasks"][0]["trials"][0]["allowlist"], "unknown");
    assert_eq!(
        evidence["tasks"][0]["trials"][0]["local_process_fallback_unknown_count"],
        0
    );
}

#[test]
fn app_server_eval_run_reports_smoke_not_run_when_blocked_before_agent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    let manifest = dir.path().join("eval.json");
    let output_root = dir.path().join("eval-output");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v5",
  "trial_count": 1,
  "tasks": [
    {
      "task_id": "fixture_eval",
      "description": "Exercise a blocked typed evaluation smoke command.",
      "capabilities": ["required_verification"],
      "workspace": {
        "source": {"type": "local", "path": "missing-source"}
      },
      "agent": {
        "instructions": "Finish the task.",
        "allowed_paths": ["README.md"],
        "required_tool_capabilities": [
          {"capability": "workspace_read", "minimum_version": 1},
          {"capability": "command_execution", "minimum_version": 1}
        ],
        "smoke_commands": [
          {"argv": ["git", "status", "--short"], "timeout_seconds": 30}
        ]
      },
      "evaluator": {
        "baseline": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "public": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "hidden": {"commands": [{"argv": ["git", "status", "--porcelain"]}]}
      }
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "jsonrpc": "2.0", "method": "eval/run",
        "id": 2,
        "params": {
            "manifest": manifest,
            "runId": "eval_smoke_not_run",
            "outputRoot": output_root,
        }
    })
    .to_string();
    let response = server.handle_json(&request).unwrap();
    let result = result_message(&response);

    assert_eq!(result["status"], "blocked");
    assert_eq!(
        result["tasks"][0]["blocker"]["kind"],
        expected_eval_blocker_kind()
    );
    assert_eq!(
        result["tasks"][0]["trial_diagnostics"][0]["smoke_command_satisfied"],
        false
    );
    let (_, expected_agent_status) = expected_pre_agent_blocked_stage_statuses();
    assert_eq!(
        result["tasks"][0]["trials"][0]["stages"]["agent"]["status"],
        expected_agent_status
    );
}

#[cfg(not(windows))]
#[test]
fn app_server_eval_run_fails_closed_when_agent_loop_capability_is_unavailable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    let manifest = dir.path().join("eval.json");
    let output_root = dir.path().join("eval-output");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v5",
  "trial_count": 1,
  "tasks": [
    {
      "task_id": "fixture_eval",
      "description": "Exercise a blocked typed evaluation task.",
      "capabilities": ["sandbox_enforcement"],
      "workspace": {
        "source": {"type": "local", "path": "missing-source"}
      },
      "agent": {
        "instructions": "Finish the task.",
        "allowed_paths": ["README.md"],
        "required_tool_capabilities": [
          {"capability": "workspace_read", "minimum_version": 1}
        ]
      },
      "evaluator": {
        "baseline": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "public": {"commands": [{"argv": ["git", "status", "--short"]}]},
        "hidden": {"commands": [{"argv": ["git", "status", "--porcelain"]}]}
      }
    }
  ]
}"#,
    )
    .expect("manifest");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let request = serde_json::json!({
        "jsonrpc": "2.0", "method": "eval/run",
        "id": 2,
        "params": {
            "manifest": manifest,
            "runId": "eval_agent_loop_unavailable",
            "outputRoot": output_root,
        }
    })
    .to_string();
    let response = server.handle_json(&request).unwrap();
    let result = result_message(&response);

    assert_eq!(result["runner"], "agent_loop");
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["blocker"], "environment");
    assert_eq!(result["tasks"][0]["trials"][0]["agent_completed"], false);
    assert_eq!(result["tasks"][0]["blocker"]["kind"], "environment");
}

#[test]
fn app_server_rejects_public_agent_host_selector() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"alternate","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert_eq!(response[0]["error"]["code"], -32602);
    assert_eq!(response[0]["error"]["message"], "Invalid params");
}

#[test]
fn public_agent_host_rejection_does_not_create_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"alternate","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert_eq!(response[0]["error"]["code"], -32602);
    assert_eq!(response[0]["error"]["message"], "Invalid params");
    let trace = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"trace/tail","id":4,"params":{{"runId":"{thread_id}","limit":10}}}}"#
        ))
        .unwrap();
    let serialized = serde_json::to_string(&trace).expect("serialize trace");
    assert!(!serialized.contains("turn started"));
}

#[test]
fn turn_start_rejects_agent_host_selector_before_turn_creation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();
    let thread = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{}}"#)
        .unwrap();
    let thread_id = result_message(&thread)["thread"]["thread_id"]
        .as_str()
        .unwrap();

    let response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","agentHost":"alternate","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .unwrap();

    assert_eq!(response[0]["error"]["code"], -32602);
    assert_eq!(response[0]["error"]["message"], "Invalid params");
}

#[test]
fn approval_defer_remains_pending_while_allow_and_deny_are_consumed() {
    for outcome in [
        ApprovalOutcome::Allow,
        ApprovalOutcome::Deny,
        ApprovalOutcome::Defer,
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let mut server = app_server(store);
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
            .unwrap();

        let center_without_records = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"approval/center","id":21,"params":{}}"#)
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

        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("reopen store");
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
        store
            .create_approval_with_trace(&request, "approval", "approval requested")
            .expect("approval");
        drop(store);

        let approvals = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"approval/list","id":22,"params":{}}"#)
            .unwrap();
        assert_eq!(
            approvals[0]["result"]["approvals"][0]["request_id"],
            "approval_1"
        );
        let center = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"approval/center","id":23,"params":{}}"#)
            .unwrap();
        assert_eq!(
            center[0]["result"]["pendingApprovals"][0]["request_id"],
            "approval_1"
        );

        let decision = ApprovalDecision::new("approval_1", outcome, "operator decision");
        let decision_message = serde_json::json!({
            "jsonrpc": "2.0", "method": "approval/decision",
            "id": 3,
            "params": decision,
        });
        let decision_result = server.handle_json(&decision_message.to_string()).unwrap();
        assert_eq!(
            decision_result[0]["result"]["decision"]["request_id"],
            "approval_1"
        );
        let center_after_decision = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"approval/center","id":24,"params":{}}"#)
            .unwrap();
        if outcome == ApprovalOutcome::Defer {
            assert_eq!(
                center_after_decision[0]["result"]["pendingApprovals"][0]["request_id"],
                "approval_1"
            );
            assert!(
                center_after_decision[0]["result"]["decisions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let repeated = server.handle_json(&decision_message.to_string()).unwrap();
            assert_eq!(
                repeated[0]["result"]["decision"]["request_id"],
                "approval_1"
            );
        } else {
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
}

#[test]
fn approval_decision_allow_without_pending_tool_call_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("README.md"), "before").expect("readme");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
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
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
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
                "jsonrpc": "2.0", "method": "approval/decision",
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
fn pending_approval_prevents_thread_archive_and_delete() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    let store = SessionStore::open(&db_path).expect("reopen store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            singularity_protocol::TurnStatus::Blocked,
            "blocked",
        )
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        "approval_archived",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(serde_json::json!({
                "request_id": &request.request_id,
                "thread_id": &thread.thread_id,
                "turn_id": &turn.turn_id,
                "tool_call_id": "call_1",
                "tool_name": "edit",
                "raw_arguments": "{}",
                "resources": &request.resources,
                "checkpoint_version": 1,
                "messages": [],
                "tool_results": [],
                "used_approval_grants": [],
                "approval_count": 1,
                "model_turns": 1,
                "completion": {}
            })),
            "approval",
            "approval requested",
        )
        .expect("approval");
    drop(store);

    for method in ["thread/archive", "thread/delete"] {
        let response = server
            .handle_json(
                &serde_json::json!({
                    "jsonrpc": "2.0", "method": method,
                    "id": 4,
                    "params": {"threadId": &thread.thread_id},
                })
                .to_string(),
            )
            .expect("lifecycle response");

        assert_eq!(
            response[0]["error"]["message"],
            "Thread already has an active or pending turn"
        );
    }
    let store = SessionStore::open(&db_path).expect("reopen store");
    assert_eq!(
        store
            .list_pending_approvals()
            .expect("pending approvals")
            .iter()
            .map(|request| request.request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["approval_archived"]
    );
}

#[test]
fn allow_resume_precondition_failure_is_terminalized_without_replay() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("readme");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = configured_app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let store = SessionStore::open(&db_path).expect("reopen store");
    let thread = store
        .create_thread(Some("test-model"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    // Deliberately omit the durable user-input item. On Windows the Allow is claimed first,
    // then resume reaches this ordinary runtime inconsistency before any tool executes. On
    // unsupported platforms, the capability gate fails closed before that seam is reached.
    let turn = store
        .create_turn(&thread.thread_id, "blocked")
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            singularity_protocol::TurnStatus::Blocked,
            "blocked",
        )
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        "approval_resume_error",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(approval_checkpoint(&request, "call_1")),
            "approval",
            "approval requested",
        )
        .expect("approval");
    drop(store);

    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "operator approved",
    );
    let response = server
        .handle_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "method": "approval/decision",
                "id": 4,
                "params": decision,
            })
            .to_string(),
        )
        .expect("allow error converges in current process");
    assert!(response.iter().any(|message| {
        message["method"] == "turn/completed" && message["params"]["turn"]["status"] == "failed"
    }));
    assert_eq!(
        response.last().expect("decision response")["result"]["decision"]["outcome"],
        "allow"
    );

    let store = SessionStore::open(&db_path).expect("reopen store");
    let failed_turn = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(failed_turn.status, singularity_protocol::TurnStatus::Failed);
    assert_eq!(failed_turn.agent_loop_status, "failed");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
    assert_eq!(store.list_approval_decisions().expect("decisions").len(), 1);
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    let terminal_trace = store
        .list_trace(&thread.thread_id)
        .expect("trace list")
        .into_iter()
        .find(|trace| trace.component == "agent_loop" && trace.payload["status"] == "failed")
        .expect("terminal trace");
    let error = terminal_trace.payload["error"]
        .as_str()
        .expect("terminal trace error");
    #[cfg(windows)]
    assert!(error.contains("user input"), "error={error}");
    #[cfg(not(windows))]
    assert!(
        error.contains("agent loop turn could not resume"),
        "error={error}"
    );
    assert_eq!(
        std::fs::read_to_string(file_path).expect("readme"),
        "before"
    );
}

#[test]
fn unavailable_workspace_only_blocks_allow_decisions() {
    for outcome in [
        ApprovalOutcome::Allow,
        ApprovalOutcome::Deny,
        ApprovalOutcome::Defer,
    ] {
        let outcome_label = match outcome {
            ApprovalOutcome::Allow => "allow",
            ApprovalOutcome::Deny => "deny",
            ApprovalOutcome::Defer => "defer",
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("open store");
        let mut server = app_server(store);
        server
                .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
                .expect("initialize");
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
            .expect("initialized");

        let store = SessionStore::open(&db_path).expect("reopen store");
        let thread = store
            .create_thread(Some("test-model"), Some(&workspace.to_string_lossy()))
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
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_workspace_missing_{outcome_label}"),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id("edit"),
        )
        .with_tool_call_id("call_1")
        .with_resources([workspace_resource("README.md")]);
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(approval_checkpoint(&request, "call_1")),
                "approval",
                "approval requested",
            )
            .expect("approval");
        std::fs::remove_dir(&workspace).expect("remove workspace");
        drop(store);

        let decision =
            ApprovalDecision::new(request.request_id.clone(), outcome, "operator decision");
        let response = server
            .handle_json(
                &serde_json::json!({
                    "jsonrpc": "2.0", "method": "approval/decision",
                    "id": 4,
                    "params": decision,
                })
                .to_string(),
            )
            .expect("decision response");
        let store = SessionStore::open(&db_path).expect("reopen store");

        match outcome {
            ApprovalOutcome::Allow => {
                assert!(response[0]["error"]["message"].is_string());
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
                    singularity_protocol::TurnStatus::Blocked
                );
            }
            ApprovalOutcome::Deny => {
                assert_eq!(
                    response.last().expect("decision response")["result"]["decision"]["outcome"],
                    "deny"
                );
                assert!(
                    !store
                        .has_pending_tool_call(&request.request_id)
                        .expect("pending")
                );
                assert_eq!(store.list_approval_decisions().expect("decisions").len(), 1);
                assert_eq!(
                    store.get_turn(&turn.turn_id).expect("turn").status,
                    singularity_protocol::TurnStatus::Failed
                );
            }
            ApprovalOutcome::Defer => {
                assert_eq!(response[0]["result"]["decision"]["outcome"], "defer");
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
                    singularity_protocol::TurnStatus::Blocked
                );
            }
        }
    }
}

#[test]
fn interrupting_a_pending_approval_atomically_invalidates_the_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    let store = SessionStore::open(&db_path).expect("reopen store");
    let thread = store
        .create_thread(Some("test-model"), Some(&workspace.to_string_lossy()))
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
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        "approval_interrupted",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(approval_checkpoint(&request, "call_1")),
            "approval",
            "approval requested",
        )
        .expect("approval");
    drop(store);

    let response = server
        .handle_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "method": "turn/interrupt",
                "id": 4,
                "params": {"turnId": &turn.turn_id},
            })
            .to_string(),
        )
        .expect("interrupt response");
    assert!(
        response.iter().any(|message| {
            message["method"] == "turn/completed"
                && message["params"]["turn"]["status"] == "interrupted"
        }),
        "{response:#?}"
    );
    let interrupt_response = response.last().expect("interrupt response");
    assert_eq!(interrupt_response["result"]["status"], "interrupted");
    assert_eq!(
        interrupt_response["result"]["agent_loop_status"],
        "cancelled"
    );

    let store = SessionStore::open(&db_path).expect("reopen store");
    let interrupted = store.get_turn(&turn.turn_id).expect("turn");
    assert_eq!(
        interrupted.status,
        singularity_protocol::TurnStatus::Interrupted
    );
    assert_eq!(interrupted.agent_loop_status, "cancelled");
    assert!(
        !store
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    assert!(
        store
            .list_approval_decisions()
            .expect("decisions")
            .is_empty()
    );
    store
        .recover_unowned_workspace_executions()
        .expect("recovery");
}

#[test]
fn approval_decision_deny_defer_and_mismatched_resource_do_not_resume_agent_loop_turn() {
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
        let mut server = app_server(store);
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
            .unwrap();
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
            .unwrap();
        let thread = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{{"cwd":{}}}}}"#,
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
            thread_id.to_string(),
            turn.turn_id.clone(),
            tool_id("edit"),
        )
        .with_tool_call_id("call_1")
        .with_resources([workspace_resource(request_resource)]);
        store
            .create_approval_with_trace(&request, "approval", "approval requested")
            .expect("approval");
        drop(store);
        let decision =
            ApprovalDecision::new(request.request_id.clone(), outcome, "operator decision");

        let response = server
            .handle_json(
                &serde_json::json!({
                    "jsonrpc": "2.0", "method": "approval/decision",
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
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let missing_turn_thread = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{"threadId":"missing","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();
    assert_eq!(
        missing_turn_thread[0]["error"]["message"],
        "Thread not found"
    );

    let request = ApprovalRequest::new(
        "approval_public",
        "thread_1",
        "turn_1",
        tool_id("write_file"),
    );
    let request_message = serde_json::json!({
        "jsonrpc": "2.0", "method": "approval/request",
        "id": 3,
        "params": request,
    });
    let public_request = server.handle_json(&request_message.to_string()).unwrap();

    assert_eq!(public_request[0]["error"]["code"], -32005);
    assert_eq!(
        public_request[0]["error"]["message"],
        "approval/request is internal to the AgentLoop approval history"
    );

    let missing_artifact = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"artifact/fetch","id":4,"params":{"artifactId":"missing"}}"#)
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
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let missing_thread = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{"threadId":"missing","input":[{"type":"text","text":"hello"}]}}"#,
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
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .unwrap();
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .unwrap();

    let interrupted = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/interrupt","id":2,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();
    let status = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/status","id":3,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .unwrap();

    assert_eq!(interrupted[0]["result"]["status"], "completed");
    let status_result = result_message(&status);
    assert_eq!(status_result["turn"]["status"], "completed");
    assert_eq!(status_result["turn"]["agent_loop_status"], "completed");
}

// These real stdio tests require the production strict Windows sandbox
// capability. Non-Windows keeps the fail-closed capability response and uses
// the in-process interruption coverage above for platform-independent state.
#[cfg(windows)]
#[test]
fn app_server_streams_turn_started_and_interrupts_an_inflight_provider_on_same_stdio() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let (base_url, accepted, release, provider_worker) = hanging_provider();
    let (mut child, mut input, mut output) = spawn_app_server(&db_path, &workspace, &base_url);
    initialize_process(&mut input, &mut output);
    let thread_id = start_process_thread(&mut input, &mut output, &workspace, 2);

    send_json(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "wait for cancellation"}]
            }
        }),
    );
    let started = output.recv_method("turn/started", Duration::from_secs(2));
    let turn_id = started["params"]["turn"]["turn_id"]
        .as_str()
        .expect("started turn id")
        .to_string();
    accepted
        .recv_timeout(Duration::from_secs(2))
        .expect("provider request started");

    send_json(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/interrupt",
            "id": 4,
            "params": {"turnId": turn_id}
        }),
    );
    let interrupt = output.recv_id(4, Duration::from_secs(2));
    assert_eq!(interrupt["result"]["status"], "cancel_requested");
    assert_eq!(interrupt["result"]["agent_loop_status"], "cancel_requested");
    let terminal = output.recv_id(3, Duration::from_secs(2));
    assert_eq!(terminal["result"]["turn"]["status"], "interrupted");
    assert_eq!(terminal["result"]["turn"]["agent_loop_status"], "cancelled");

    release.send(()).expect("release provider");
    provider_worker.join().expect("provider worker joins");
    shutdown_process(&mut child, &mut input, &mut output, 5);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let persisted = store.get_turn(&turn_id).expect("persisted turn");
    assert_eq!(
        persisted.status,
        singularity_protocol::TurnStatus::Interrupted
    );
    assert_eq!(persisted.agent_loop_status, "cancelled");
    let traces = store.list_trace(&thread_id).expect("turn trace");
    let terminal_trace = traces
        .iter()
        .find(|trace| trace.component == "agent_loop")
        .expect("terminal agent trace");
    assert_eq!(terminal_trace.payload["status"], "cancelled");
    assert!(
        !terminal_trace
            .payload
            .to_string()
            .contains("late completion")
    );
}

#[cfg(windows)]
#[test]
fn app_server_serializes_shared_workspace_across_processes_and_observes_interrupt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let (base_url, accepted, release, provider_worker) = hanging_provider();
    let (mut primary, mut primary_input, mut primary_output) =
        spawn_app_server(&db_path, &workspace, &base_url);
    initialize_process(&mut primary_input, &mut primary_output);
    let thread_id = start_process_thread(&mut primary_input, &mut primary_output, &workspace, 2);
    send_json(
        &mut primary_input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "wait for external cancellation"}]
            }
        }),
    );
    let started = primary_output.recv_method("turn/started", Duration::from_secs(2));
    let turn_id = started["params"]["turn"]["turn_id"]
        .as_str()
        .expect("started turn id")
        .to_string();
    accepted
        .recv_timeout(Duration::from_secs(2))
        .expect("provider request started");

    let (mut secondary, mut secondary_input, mut secondary_output) =
        spawn_app_server(&db_path, &workspace, &base_url);
    initialize_process(&mut secondary_input, &mut secondary_output);
    let secondary_thread_id =
        start_process_thread(&mut secondary_input, &mut secondary_output, &workspace, 10);
    send_json(
        &mut secondary_input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/start",
            "id": 11,
            "params": {
                "threadId": secondary_thread_id,
                "input": [{"type": "text", "text": "must not overlap the active turn"}]
            }
        }),
    );
    let rejected = secondary_output.recv_id(11, Duration::from_secs(2));
    assert_eq!(
        rejected["error"]["message"],
        "Workspace already has an active or pending turn"
    );
    send_json(
        &mut secondary_input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/interrupt",
            "id": 12,
            "params": {"turnId": turn_id}
        }),
    );
    let interrupt = secondary_output.recv_id(12, Duration::from_secs(2));
    assert_eq!(interrupt["result"]["status"], "cancel_requested");
    shutdown_process(
        &mut secondary,
        &mut secondary_input,
        &mut secondary_output,
        13,
    );

    let terminal = primary_output.recv_id(3, Duration::from_secs(2));
    assert_eq!(terminal["result"]["turn"]["status"], "interrupted");
    assert_eq!(terminal["result"]["turn"]["agent_loop_status"], "cancelled");
    release.send(()).expect("release provider");
    provider_worker.join().expect("provider worker joins");
    shutdown_process(&mut primary, &mut primary_input, &mut primary_output, 6);
}

#[cfg(windows)]
#[test]
fn app_server_approval_continuation_keeps_interrupt_and_shutdown_responsive() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("README.md"), "before").expect("readme");
    let db_path = dir.path().join("sessions.sqlite3");
    let (base_url, accepted, release, provider_worker) = hanging_provider();

    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "blocked",
            serde_json::json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "approval turn",
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
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id("edit"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let mut checkpoint = approval_checkpoint(&request, "call_1");
    let arguments = serde_json::json!({
        "path": "README.md",
        "expected": "before",
        "replacement": "after"
    });
    checkpoint["raw_arguments"] = serde_json::json!(arguments.to_string());
    checkpoint["messages"][0]["tool_calls"][0]["arguments"] = arguments.clone();
    checkpoint["messages"][0]["tool_calls"][0]["raw_arguments"] =
        serde_json::json!(arguments.to_string());
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("approval");
    drop(store);

    let (mut child, mut input, mut output) = spawn_app_server(&db_path, &workspace, &base_url);
    initialize_process(&mut input, &mut output);
    send_json(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "approval/decision",
            "id": 3,
            "params": {
                "request_id": request.request_id,
                "decision_id": "decision_approval_continuation",
                "outcome": "allow",
                "reason": "operator approved"
            }
        }),
    );
    accepted
        .recv_timeout(Duration::from_secs(2))
        .expect("approval continuation reached provider");

    send_json(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/interrupt",
            "id": 4,
            "params": {"turnId": turn.turn_id}
        }),
    );
    let interrupt = output.recv_id(4, Duration::from_secs(2));
    assert_eq!(interrupt["result"]["status"], "cancel_requested");
    assert_eq!(interrupt["result"]["agent_loop_status"], "cancel_requested");

    send_json(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "server/shutdown",
            "id": 5,
            "params": {}
        }),
    );
    let shutdown = output.recv_id(5, Duration::from_secs(2));
    assert_eq!(shutdown["result"]["shutdown"], true);

    release.send(()).expect("release provider");
    let decision = output.recv_id(3, Duration::from_secs(7));
    assert_eq!(decision["result"]["decision"]["outcome"], "allow");
    drop(input);
    let status = child.wait().expect("wait app-server");
    assert!(status.success(), "app-server exited with {status}");
    provider_worker.join().expect("provider worker joins");

    let store = SessionStore::open(&db_path).expect("reopen store");
    let persisted = store.get_turn(&turn.turn_id).expect("persisted turn");
    assert_eq!(
        persisted.status,
        singularity_protocol::TurnStatus::Interrupted
    );
    assert_eq!(persisted.agent_loop_status, "cancelled");
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
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":\"quoted-id\",\"params\":\"bad\"}\n")
        .expect("write invalid params");
    drop(stdin);
    let output = child.wait_with_output().expect("app-server output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let first_line = stdout.lines().next().expect("error line");
    let value: serde_json::Value = serde_json::from_str(first_line).expect("valid json error");

    assert_eq!(value["id"], "quoted-id");
    assert_eq!(value["error"]["code"], -32602);
    assert_eq!(value["error"]["message"], "Invalid params");
}

#[test]
fn turn_status_recovers_an_unowned_running_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("orphaned running turn");
    let mut server = app_server(store);
    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        )
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/status","id":2,"params":{{"turnId":"{}"}}}}"#,
            turn.turn_id
        ))
        .expect("turn status");

    assert_eq!(response[0]["result"]["turn"]["status"], "interrupted");
    assert_eq!(
        response[0]["result"]["turn"]["agent_loop_status"],
        "interrupted"
    );
    let trace = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"trace/list","id":3,"params":{{"runId":"{}"}}}}"#,
            thread.thread_id
        ))
        .expect("trace list");
    assert!(
        trace[0]["result"]["events"]
            .as_array()
            .is_some_and(|events| {
                events.iter().any(|event| {
                    event["session_id"] == turn.turn_id
                        && event["payload"]["recovery_reason"] == "execution_owner_lost"
                })
            })
    );
}

#[test]
fn app_server_exits_when_stdout_transport_is_lost() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let mut child = Command::new(app_server_bin())
        .env("SINGULARITY_APP_SERVER_DB", &db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let mut stdin = child.stdin.take().expect("stdin");
    drop(child.stdout.take().expect("stdout"));
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1,\"params\":{\"clientInfo\":{\"name\":\"test\",\"title\":\"Test\",\"version\":\"0.1.0\"}}}\n",
        )
        .expect("write initialize");
    stdin.flush().expect("flush initialize");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll app-server") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().expect("kill app-server with lost stdout");
            child.wait().expect("reap app-server with lost stdout");
            panic!("app-server continued running after stdout transport was lost");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    assert!(!status.success(), "lost stdout must be fatal");
}

#[test]
fn app_server_reports_startup_errors_without_panicking() {
    let dir = tempfile::tempdir().expect("temp dir");
    let invalid_db_path = dir.path().join("database-directory");
    std::fs::create_dir(&invalid_db_path).expect("create invalid database path");

    let output = Command::new(app_server_bin())
        .env("SINGULARITY_APP_SERVER_DB", &invalid_db_path)
        .output()
        .expect("run app-server");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("app-server error:"), "stderr={stderr}");
    assert!(!stderr.contains("panicked at"), "stderr={stderr}");
}

#[cfg(windows)]
struct JsonOutput {
    receiver: Receiver<serde_json::Value>,
    buffered: VecDeque<serde_json::Value>,
}

#[cfg(windows)]
impl JsonOutput {
    fn recv_id(&mut self, id: i64, timeout: Duration) -> serde_json::Value {
        self.recv_where(timeout, |message| message["id"] == id)
    }

    fn recv_method(&mut self, method: &str, timeout: Duration) -> serde_json::Value {
        self.recv_where(timeout, |message| message["method"] == method)
    }

    fn recv_where(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        if let Some(index) = self.buffered.iter().position(&predicate) {
            return self.buffered.remove(index).expect("buffered message");
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for app-server message");
            let message = self
                .receiver
                .recv_timeout(remaining)
                .expect("app-server output message");
            if predicate(&message) {
                return message;
            }
            self.buffered.push_back(message);
        }
    }
}

#[cfg(windows)]
fn spawn_app_server(
    db_path: &std::path::Path,
    workspace: &std::path::Path,
    base_url: &str,
) -> (Child, ChildStdin, JsonOutput) {
    let mut child = Command::new(app_server_bin())
        .current_dir(workspace)
        .env("SINGULARITY_APP_SERVER_DB", db_path)
        .env("SINGULARITY_MODEL_PROVIDER", "openai_compatible")
        .env("SINGULARITY_MODEL", "gpt-test")
        .env("SINGULARITY_BASE_URL", base_url)
        .env("SINGULARITY_API_KEY", "test-secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let input = child.stdin.take().expect("app-server stdin");
    let stdout = child.stdout.take().expect("app-server stdout");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read app-server output");
            sender
                .send(serde_json::from_str(&line).expect("app-server json line"))
                .expect("send app-server output");
        }
    });
    (
        child,
        input,
        JsonOutput {
            receiver,
            buffered: VecDeque::new(),
        },
    )
}

#[cfg(windows)]
fn initialize_process(input: &mut ChildStdin, output: &mut JsonOutput) {
    send_json(
        input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "initialize",
            "id": 1,
            "params": {"clientInfo": {"name": "test", "title": "Test", "version": "0.1.0"}}
        }),
    );
    let initialized = output.recv_id(1, Duration::from_secs(2));
    assert!(initialized.get("result").is_some());
    send_json(
        input,
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
}

#[cfg(windows)]
fn start_process_thread(
    input: &mut ChildStdin,
    output: &mut JsonOutput,
    workspace: &std::path::Path,
    id: i64,
) -> String {
    send_json(
        input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "thread/start",
            "id": id,
            "params": {"model": "gpt-test", "cwd": workspace}
        }),
    );
    output.recv_id(id, Duration::from_secs(2))["result"]["thread"]["thread_id"]
        .as_str()
        .expect("thread id")
        .to_string()
}

#[cfg(windows)]
fn send_json(input: &mut impl Write, message: serde_json::Value) {
    writeln!(input, "{message}").expect("write app-server request");
    input.flush().expect("flush app-server request");
}

#[cfg(windows)]
fn shutdown_process(child: &mut Child, input: &mut ChildStdin, output: &mut JsonOutput, id: i64) {
    send_json(
        input,
        serde_json::json!({"jsonrpc": "2.0", "method": "server/shutdown", "id": id, "params": {}}),
    );
    assert_eq!(
        output.recv_id(id, Duration::from_secs(2))["result"]["shutdown"],
        true
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("poll app-server") {
            assert!(status.success(), "app-server exited with {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill stuck app-server");
            child.wait().expect("reap stuck app-server");
            panic!("app-server did not exit after shutdown");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn hanging_provider() -> (
    String,
    Receiver<()>,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
    let address = listener.local_addr().expect("provider address");
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        accepted_tx.send(()).expect("signal provider request");
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("release hanging provider");
        let body = r#"{
            "id":"late_response",
            "choices":[{"message":{"role":"assistant","content":"late completion"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
    });
    (format!("http://{address}"), accepted_rx, release_tx, worker)
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
