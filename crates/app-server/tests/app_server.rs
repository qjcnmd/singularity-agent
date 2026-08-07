//! AppServer protocol、approval continuation、recovery 和 sandbox 边界测试。

use singularity_agent::AgentRecoveryMetrics;
use singularity_app_server::{AppServer, AppServerError};
use singularity_core::CancellationToken;
use singularity_model::{ModelUsage, ProviderAttemptMetadata, ProviderConfigSnapshot};
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, PermissionResource, ToolId,
    WorkspaceRelativePath,
};
use singularity_protocol::ItemKind;
#[cfg(windows)]
use singularity_protocol::{ConversationRole, TraceMetricSampleKind};
use singularity_sandbox::{
    CommandEnvironmentPolicy, CommandRequest, CommandResult, CommandScriptRequest,
    ExecutableAvailability, SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities,
    SandboxPreflightFact, SandboxPreflightOutcome, SandboxPreflightReport, WorkspaceMutation,
};
use singularity_store::{RegisterArtifactRefParams, SessionStore, StoreError};
#[cfg(windows)]
use std::collections::VecDeque;
use std::io::Write;
#[cfg(windows)]
use std::io::{BufRead, BufReader, Read};
#[cfg(windows)]
use std::net::{TcpListener, TcpStream};
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
    AppServer::new(
        store,
        ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            None,
            None,
        ),
    )
}

struct CompletedSandboxBackend;

impl SandboxBackend for CompletedSandboxBackend {
    fn name(&self) -> &'static str {
        "app_server_integration_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn preflight(
        &self,
        _workspace: &std::path::Path,
        _cancellation: &CancellationToken,
    ) -> SandboxPreflightReport {
        SandboxPreflightReport {
            outcome: SandboxPreflightOutcome::Supported,
            error_code: None,
            profile: "workspace_write_network_denied".to_string(),
            backend: self.name().to_string(),
            missing_capabilities: Vec::new(),
            os: "test".to_string(),
            arch: "test".to_string(),
            kernel: None,
            filesystem: None,
            overlayfs: SandboxPreflightFact::NotApplicable,
            user_namespace: SandboxPreflightFact::NotApplicable,
            mount_namespace: SandboxPreflightFact::NotApplicable,
            pid_namespace: SandboxPreflightFact::NotApplicable,
            network_namespace: SandboxPreflightFact::NotApplicable,
            no_new_privs: SandboxPreflightFact::NotApplicable,
            seccomp: SandboxPreflightFact::NotApplicable,
            landlock: SandboxPreflightFact::NotApplicable,
            transactional_workspace: SandboxPreflightFact::Passed,
            network_denied: SandboxPreflightFact::Passed,
            protected_paths: SandboxPreflightFact::Passed,
        }
    }

    fn probe_executable(
        &self,
        _workspace: &std::path::Path,
        _executable: &str,
        _environment: &CommandEnvironmentPolicy,
    ) -> ExecutableAvailability {
        ExecutableAvailability::Available
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "command ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "command ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
    }
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
        ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL" => Some("test-model".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            None,
            None,
        ),
    )
}

#[test]
fn configured_provider_drops_cleanly_inside_app_server_runtime() {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test Tokio runtime");
        let runtime_handle = runtime.handle().clone();
        runtime.block_on(async move {
            let provider_snapshot = ProviderConfigSnapshot::capture(
                |name| match name {
                    "SINGULARITY_MODEL" => Some("drop-test-model".to_string()),
                    "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                    "SINGULARITY_API_KEY" => Some("drop-test-key".to_string()),
                    _ => None,
                },
                Some(runtime_handle),
                None,
            );
            assert!(provider_snapshot.configuration().configured);
            let store = SessionStore::open(":memory:").expect("open store");
            drop(AppServer::new(store, provider_snapshot));
        });
    }));

    assert!(
        result.is_ok(),
        "configured provider drop panicked: {result:?}"
    );
}

fn approval_checkpoint(request: &ApprovalRequest, tool_call_id: &str) -> serde_json::Value {
    serde_json::json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": tool_call_id,
        "tool_name": &request.action,
        "raw_arguments": r#"{"changes":[{"path":"README.md","expected":"before","replacement":"after"}]}"#,
        "resources": &request.resources,
        "checkpoint_version": 7,
        "project_instructions_digest": null,
        "messages": [{"role":"assistant","content":"","tool_calls":[{"tool_call_id":tool_call_id,"tool_name":&request.action,"arguments":{"changes":[{"path":"README.md","expected":"before","replacement":"after"}]},"raw_arguments":r#"{"changes":[{"path":"README.md","expected":"before","replacement":"after"}]}"#,"parse_status":"valid","validation_errors":[]}]}],
        "tool_result_occurrences": [],
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
        "repair_attempts": 0,
        "last_completion_error": null,
        "recovery_metrics": AgentRecoveryMetrics::default(),
        "model_usage": ModelUsage::default(),
        "provider_attempts": ProviderAttemptMetadata::default(),
        "context_trace": null,
        "seen_tool_call_fingerprints": [],
        "completed_tool_call_fingerprints": [],
        "last_repair_failure": null
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
    assert_eq!(subscription[0]["method"], "event/gap");
    assert_eq!(
        subscription[0]["params"]["event"]["gap"]["reason"],
        "cursor_not_replayed"
    );
    assert!(
        subscription[0]["params"]["event"]["cursor"]
            .as_u64()
            .is_some_and(|cursor| cursor > 0)
    );
    let subscription_result = result_message(&subscription);
    assert_eq!(
        subscription_result["eventTypes"],
        serde_json::json!(["thread/started", "turn/started"])
    );
    assert_eq!(
        subscription_result["cursor"],
        subscription[0]["params"]["event"]["cursor"]
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
    let thread_started = thread
        .iter()
        .find(|message| message["method"] == "thread/started")
        .expect("thread started event");
    assert_eq!(thread_started["params"]["event"]["class"], "state");
    assert_eq!(thread_started["params"]["event"]["delivery"], "reliable");
    assert_eq!(
        thread_started["params"]["event"]["recoveryQuery"]["method"],
        "thread/read"
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
    let trace_metrics = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"trace/metrics","id":83,"params":{{"runId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(trace_metrics[0]["result"]["metrics"]["runId"], thread_id);
    assert_eq!(
        trace_metrics[0]["result"]["metrics"]["metrics"]
            .as_array()
            .unwrap()
            .len(),
        30
    );
    assert!(
        trace_metrics[0]["result"]["metrics"]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|metric| metric["name"] == "tool_success_rate_bps")
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
fn event_subscription_is_inactive_until_explicit_and_rejects_invalid_cursor() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let mut server = app_server(store);
    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        )
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let before_subscribe = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#,
        )
        .expect("thread start");
    assert!(
        before_subscribe
            .iter()
            .all(|message| message["method"] != "thread/started")
    );
    assert!(
        before_subscribe
            .iter()
            .any(|message| message["result"].is_object())
    );

    let invalid = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":3,"params":{"eventTypes":[],"cursor":0}}"#,
        )
        .expect("invalid cursor response");
    assert_eq!(invalid[0]["error"]["code"], -32602);
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
        assert_eq!(
            response[0]["error"]["message"]
                .as_str()
                .expect("error message"),
            "workspace capability unavailable",
            "response={response:?}"
        );
    }

    let turn = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{}","input":[{{"type":"text","text":"do not run"}}]}}}}"#,
            active_missing.thread_id
        ))
        .expect("turn response");
    assert!(
        turn[0]["error"]["message"].as_str().expect("turn error")
            == "workspace capability unavailable"
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
fn app_server_batch_shutdown_stays_with_stdin_owner_when_unknown_method_is_present() {
    let dir = tempfile::tempdir().expect("temp directory");
    let mut child = Command::new(app_server_bin())
        .current_dir(dir.path())
        .env(
            "SINGULARITY_APP_SERVER_DB",
            dir.path().join("sessions.sqlite3"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let mut stdin = child.stdin.take().expect("app-server stdin");
    for line in [
        r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"[{"jsonrpc":"2.0","method":"eval/run","id":2,"params":{"manifest":"missing-evaluation-manifest.json","runId":"batch-eval"}},{"jsonrpc":"2.0","method":"server/shutdown","id":3,"params":{}}]"#,
    ] {
        writeln!(stdin, "{line}").expect("write app-server request");
        stdin.flush().expect("flush app-server request");
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let exited = loop {
        if child.try_wait().expect("poll app-server").is_some() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if !exited {
        child.kill().expect("kill stuck app-server");
        drop(stdin);
        child.wait().expect("reap stuck app-server");
        panic!("batch server/shutdown was not owned by the stdin server");
    }
    drop(stdin);

    let output = child.wait_with_output().expect("wait for app-server");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 app-server output");
    let shutdown = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            message
                .as_array()
                .and_then(|responses| responses.iter().find(|response| response["id"] == 3))
                .cloned()
        })
        .expect("batch shutdown response");
    assert_eq!(shutdown["result"]["shutdown"], true);
}

#[test]
fn app_server_reuses_one_provider_snapshot_for_capability_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL" => Some("snapshot-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("https://snapshot.example/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("snapshot-secret".to_string()),
            _ => None,
        },
        None,
        None,
    );
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

#[test]
fn app_server_reports_default_agent_loop_backend_capability() {
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
}

#[test]
fn app_server_does_not_expose_development_evaluation_method() {
    let store = SessionStore::open(":memory:").expect("open store");
    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let response = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"eval/run","id":2,"params":{"manifest":"manifest.json","runId":"run"}}"#)
        .expect("unknown method response");

    assert_eq!(response[0]["error"]["code"], -32601);
    assert_eq!(response[0]["error"]["message"], "Method not found");
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
        tool_id("patch"),
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
        tool_id("patch"),
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
            format!(
                "thread already has an active or pending turn {}; use sg turn resume/pause/input {}",
                turn.turn_id, turn.turn_id
            )
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
    subscribe_events(&mut server);

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
        tool_id("patch"),
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
    assert_eq!(error, "agent loop execution failed");
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
            tool_id("patch"),
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
    subscribe_events(&mut server);
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
fn artifact_fetch_returns_only_registered_bound_artifacts_and_hides_deleted_threads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
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
    let digest = format!("sha256:{}", "d".repeat(64));
    let artifact = store
        .register_artifact_ref(RegisterArtifactRefParams {
            run_id: &thread.thread_id,
            item_id: Some(&item.item_id),
            kind: "file",
            uri: "artifact://safe/result.txt",
            content_digest: &digest,
            summary: "safe result",
            metadata: serde_json::json!({"path": "safe/result.txt"}),
        })
        .expect("artifact");
    assert!(matches!(
        store.register_artifact_ref(RegisterArtifactRefParams {
            run_id: "synthetic_run",
            item_id: None,
            kind: "file",
            uri: "artifact://synthetic/result.txt",
            content_digest: &digest,
            summary: "must fail",
            metadata: serde_json::json!({}),
        }),
        Err(StoreError::NotFound(message)) if message == "artifact run synthetic_run"
    ));
    store
        .update_turn_status(&turn.turn_id, singularity_protocol::TurnStatus::Completed)
        .expect("complete turn");

    let mut server = app_server(store);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    let fetched = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"artifact/fetch","id":2,"params":{{"artifactId":"{}"}}}}"#,
            artifact.artifact_id
        ))
        .expect("fetch artifact");
    assert_eq!(
        fetched[0]["result"]["artifact"]["artifactId"],
        artifact.artifact_id
    );

    let deleter = SessionStore::open(&db_path).expect("reopen store");
    deleter
        .delete_thread(&thread.thread_id)
        .expect("delete thread");
    let deleted = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"artifact/fetch","id":3,"params":{{"artifactId":"{}"}}}}"#,
            artifact.artifact_id
        ))
        .expect("fetch deleted artifact");
    assert_eq!(deleted[0]["error"]["message"], "Artifact not found");
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
    let writer_visible = traces
        .iter()
        .flat_map(|trace| trace.metric_samples.iter())
        .filter(|sample| sample.kind == TraceMetricSampleKind::WriterVisible)
        .map(|sample| sample.count)
        .sum::<u64>();
    assert!(
        writer_visible > 0,
        "turn-bound stdout frames were not traced"
    );
}

#[cfg(windows)]
#[test]
fn app_server_streams_real_responses_provider_deltas_and_persists_the_final_message() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let (base_url, requests, provider_worker) = streaming_responses_provider();
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
                "input": [{"type": "text", "text": "stream a short answer"}]
            }
        }),
    );
    output.recv_method("turn/started", Duration::from_secs(2));
    let first = output.recv_method("item/agentMessage/delta", Duration::from_secs(5));
    assert_eq!(first["params"]["delta"], "streamed ");
    let second = output.recv_method("item/agentMessage/delta", Duration::from_secs(2));
    assert_eq!(second["params"]["delta"], "answer");

    let terminal = output.recv_id(3, Duration::from_secs(2));
    assert_eq!(terminal["result"]["turn"]["status"], "completed");
    assert_eq!(terminal["result"]["turn"]["agent_loop_status"], "completed");
    output.recv_method("turn/completed", Duration::from_secs(2));
    let request_bodies = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("provider request sequence");
    assert!(
        request_bodies.len() >= 2,
        "capability probe and stream request"
    );
    assert!(
        request_bodies.iter().any(|body| body["stream"] == true),
        "production provider must issue a streaming Responses request"
    );
    provider_worker.join().expect("provider worker joins");
    shutdown_process(&mut child, &mut input, &mut output, 4);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let history = store
        .read_thread_history(&thread_id, None, 8)
        .expect("thread history");
    assert!(history.messages.iter().any(|message| {
        message.role == ConversationRole::Assistant && message.content == "streamed answer"
    }));
    let traces = store.list_trace(&thread_id).expect("provider trace");
    assert!(traces.iter().any(|event| {
        event.span_kind == Some(singularity_protocol::TraceSpanKind::ProviderAttempt)
            && event.span_phase == Some(singularity_protocol::TraceSpanPhase::End)
            && event.time_to_first_token_ms.is_some()
    }));
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
        tool_id("patch"),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let mut checkpoint = approval_checkpoint(&request, "call_1");
    let arguments = serde_json::json!({
        "changes": [{
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }]
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
    send_json(
        input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "event/subscribe", "id": 99,
            "params": {"eventTypes": [
                "thread/started", "turn/started", "turn/completed",
                "item/started", "item/completed",
                "item/agentMessage/delta", "approval/requested"
            ]}
        }),
    );
    let subscription = output.recv_id(99, Duration::from_secs(2));
    assert!(subscription.get("result").is_some());
}

fn subscribe_events(server: &mut AppServer) {
    let response = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":99,"params":{"eventTypes":["thread/started","turn/started","turn/completed","item/started","item/completed","item/agentMessage/delta","approval/requested"]}}"#,
        )
        .expect("event subscription");
    assert!(
        response
            .iter()
            .any(|message| message.get("result").is_some())
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

#[cfg(windows)]
fn streaming_responses_provider() -> (
    String,
    Receiver<Vec<serde_json::Value>>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind streaming provider");
    let address = listener.local_addr().expect("streaming provider address");
    let (requests_tx, requests_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = listener
                .accept()
                .expect("accept streaming provider request");
            let request_body = read_http_json_body(&mut stream);
            let request: serde_json::Value =
                serde_json::from_str(&request_body).expect("provider request json");
            requests.push(request.clone());
            if request["stream"] == true {
                let completed = serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "response_app_server_stream",
                        "object": "response",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "streamed answer"}]
                        }],
                        "usage": {
                            "input_tokens": 3,
                            "output_tokens": 2,
                            "total_tokens": 5,
                            "input_tokens_details": {"cached_tokens": 0},
                            "output_tokens_details": {"reasoning_tokens": 0}
                        }
                    }
                });
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"streamed \"}}\n\n"
                )
                .expect("write first streaming delta");
                stream.flush().expect("flush first streaming delta");
                write!(
                    stream,
                    "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}}\n\n"
                )
                .expect("write second streaming delta");
                stream.flush().expect("flush second streaming delta");
                write!(stream, "event: response.completed\ndata: {completed}\n\n")
                    .expect("write streaming completion");
                requests_tx.send(requests).expect("send request sequence");
                break;
            }
            let response = responses_capability_probe_response(&request)
                .expect("non-stream request must be a capability probe");
            write_json_response(&mut stream, &response);
        }
    });
    (
        format!("http://{address}/v1/responses"),
        requests_rx,
        worker,
    )
}

#[cfg(windows)]
fn read_http_json_body(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read provider request line");
    assert!(request_line.contains("/v1/responses"));
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read provider header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().expect("provider content length");
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("read provider body");
    String::from_utf8(body).expect("provider request utf8")
}

#[cfg(windows)]
fn responses_capability_probe_response(request: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = request.get("tools")?.as_array()?;
    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    if !names.contains(&"singularity_capability_probe_a") {
        return None;
    }
    let continuation = request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call_output")
        });
    let strict = tools
        .iter()
        .any(|tool| tool.get("strict").and_then(serde_json::Value::as_bool) == Some(true));
    let arguments = if strict {
        serde_json::json!({"probe": "schema_sentinel_alpha", "values": [7, 7]})
    } else {
        serde_json::json!({})
    };
    let mut output = vec![serde_json::json!({
        "type": "function_call",
        "call_id": if continuation { "probe_call_continuation" } else { "probe_call_a" },
        "name": "singularity_capability_probe_a",
        "arguments": arguments.to_string()
    })];
    if !continuation
        && names.contains(&"singularity_capability_probe_b")
        && request["parallel_tool_calls"] == true
    {
        output.push(serde_json::json!({
            "type": "function_call",
            "call_id": "probe_call_b",
            "name": "singularity_capability_probe_b",
            "arguments": arguments.to_string()
        }));
    }
    Some(serde_json::json!({
        "id": if continuation { "capability_probe_continuation_response" } else { "capability_probe_response" },
        "object": "response",
        "status": "completed",
        "output": output,
        "usage": {
            "input_tokens": 2,
            "output_tokens": 1,
            "total_tokens": 3,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    }))
}

#[cfg(windows)]
fn write_json_response(stream: &mut TcpStream, body: &serde_json::Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write provider json response");
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

/// 从 JSON-RPC turn/start 入口开始，经过真实 Store、Checkpoint、Approval、
/// AgentLoop、ToolBroker 和 WorkspaceTools 的确定性 Approval Resume E2E。
#[test]
fn approval_resume_workspace_write_e2e_from_json_rpc_entry() {
    use singularity_model::{ModelTurnResponse, ProviderProtocolContract};
    use std::sync::{Arc, Mutex};

    struct SequenceProvider {
        responses: Vec<ModelTurnResponse>,
        seen_requests: Arc<Mutex<Vec<singularity_model::ModelTurnRequest>>>,
        negotiation_count: Arc<std::sync::atomic::AtomicU64>,
    }

    impl singularity_model::Provider for SequenceProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
        }

        fn negotiate_tool_capabilities(
            &self,
            _model_preferences: &singularity_model::ModelPreferences,
            _cancellation: &singularity_core::CancellationToken,
        ) -> Result<singularity_model::ProviderProtocolNegotiation, singularity_model::ProviderError>
        {
            let observed_at_unix_ms = self
                .negotiation_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            Ok(singularity_model::ProviderProtocolNegotiation {
                contract: ProviderProtocolContract::default(),
                metadata: singularity_model::ProviderCapabilityMetadata {
                    api_protocol: singularity_model::ProviderApiProtocol::Declared,
                    profile: singularity_model::ProviderCapabilityProfile::Declared,
                    cache_hit: false,
                    profile_attempts: 0,
                    fallback_count: 0,
                    probe_usage: ModelUsage::default(),
                    probe_attempt_metadata: ProviderAttemptMetadata::default(),
                    cache_observations: vec![
                        singularity_model::ProviderCapabilityCacheObservation {
                            api_protocol: singularity_model::ProviderApiProtocol::Declared,
                            outcome: singularity_model::ProviderCapabilityCacheLookupResult::Miss,
                            observed_at_unix_ms,
                            model_turn_ordinal: None,
                            parent_occurrence_id: None,
                        },
                    ],
                },
            })
        }

        fn complete(
            &self,
            request: &singularity_model::ModelTurnRequest,
            _cancellation: &singularity_core::CancellationToken,
        ) -> Result<ModelTurnResponse, singularity_model::ProviderError> {
            let mut seen = self.seen_requests.lock().expect("lock");
            let idx = seen.len();
            seen.push(request.clone());
            let mut response = self
                .responses
                .get(idx)
                .unwrap_or_else(|| self.responses.last().expect("response"))
                .clone();
            response.request_id = request.request_id.clone();
            Ok(response)
        }

        fn complete_observed(
            &self,
            request: &singularity_model::ModelTurnRequest,
            cancellation: &singularity_core::CancellationToken,
            on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent) -> bool,
        ) -> Result<ModelTurnResponse, singularity_model::ProviderError> {
            let attempt_index = self.seen_requests.lock().expect("lock").len() as u32 + 1;
            let started_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_millis() as u64;
            let started = singularity_model::ProviderAttemptStarted {
                operation_phase: singularity_model::ProviderAttemptOperationPhase::Completion,
                provider_name: "test_sequence".to_string(),
                model_name: "gpt-test".to_string(),
                actual_api_protocol: singularity_model::ProviderApiProtocol::Declared,
                attempt_index,
                started_at_unix_ms,
            };
            if !on_attempt(singularity_model::ProviderAttemptEvent::Started(
                started.clone(),
            )) {
                return Err(singularity_model::ProviderError::from_model_error(
                    singularity_model::ModelError::new(
                        singularity_model::ModelErrorKind::UnknownProviderError,
                        "provider attempt observer rejected start",
                    ),
                ));
            }
            let result = self.complete(request, cancellation);
            let ended_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_millis() as u64;
            let (terminal_status, error_category, usage) = match &result {
                Ok(response) => (
                    singularity_model::ProviderAttemptStatus::Ok,
                    None,
                    Some(response.usage.clone()),
                ),
                Err(error) => (
                    singularity_model::ProviderAttemptStatus::Error,
                    Some(error.error.category()),
                    None,
                ),
            };
            let finished = singularity_model::ProviderAttemptOccurrence {
                operation_phase: started.operation_phase,
                provider_name: started.provider_name,
                model_name: started.model_name,
                actual_api_protocol: started.actual_api_protocol,
                attempt_index: started.attempt_index,
                terminal_status,
                started_at_unix_ms,
                ended_at_unix_ms,
                attempt_duration_ms: ended_at_unix_ms.saturating_sub(started_at_unix_ms),
                request_send_to_headers_ms: Some(0),
                queue_duration_ms: None,
                time_to_first_text_delta_ms: None,
                retry_scheduled: false,
                retry_backoff_ms: None,
                error_category,
                error_stage: None,
                diagnostic_code: None,
                usage,
                model_turn_ordinal: None,
                parent_occurrence_id: None,
            };
            if !on_attempt(singularity_model::ProviderAttemptEvent::Finished(finished)) {
                return Err(singularity_model::ProviderError::from_model_error(
                    singularity_model::ModelError::new(
                        singularity_model::ModelErrorKind::UnknownProviderError,
                        "provider attempt observer rejected finish",
                    ),
                ));
            }
            result
        }
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("write readme");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");

    // 固定模型返回序列：patch 触发 approval，随后 command 产生 Unchanged 证据，最后返回答案。
    let mut patch_response = ModelTurnResponse::completed("req_1", "resp_1", "");
    patch_response
        .tool_calls
        .push(singularity_model::ModelToolCall {
            tool_call_id: "call_patch_1".to_string(),
            tool_name: "patch".to_string(),
            raw_arguments: serde_json::json!({
                "changes": [{
                    "path": "README.md",
                    "expected": "before",
                    "replacement": "after"
                }]
            })
            .to_string(),
            arguments: serde_json::json!({
                "changes": [{
                    "path": "README.md",
                    "expected": "before",
                    "replacement": "after"
                }]
            }),
            parse_status: singularity_model::ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });

    let mut verify_response = ModelTurnResponse::completed("req_2", "resp_2", "");
    verify_response
        .tool_calls
        .push(singularity_model::ModelToolCall {
            tool_call_id: "call_verify_1".to_string(),
            tool_name: "command".to_string(),
            raw_arguments: serde_json::json!({
                "command": "type README.md",
                "cwd": ".",
                "timeout_seconds": 30
            })
            .to_string(),
            arguments: serde_json::json!({
                "command": "type README.md",
                "cwd": ".",
                "timeout_seconds": 30
            }),
            parse_status: singularity_model::ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });

    let final_response =
        ModelTurnResponse::completed("req_3", "resp_3", "Task completed: README.md updated.");

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = SequenceProvider {
        responses: vec![patch_response, verify_response, final_response],
        seen_requests: Arc::clone(&seen_requests),
        negotiation_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };

    let mut server = app_server(store)
        .with_test_provider(Arc::new(provider))
        .with_sandbox_backend(CompletedSandboxBackend);
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#)
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    subscribe_events(&mut server);

    // 创建 workspace-write + on-request approval 线程
    let thread_response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{{"cwd":{},"sandboxMode":"workspace-write","approvalPolicy":"on-request"}}}}"#,
            serde_json::to_string(&workspace.to_string_lossy()).expect("cwd")
        ))
        .expect("thread/start");
    let thread_id = result_message(&thread_response)["thread"]["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_string();

    // turn/start 触发模型调用，模型请求 edit → approval required → turn blocked
    let _turn_response = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{thread_id}","input":[{{"type":"text","text":"Edit README.md changing before to after"}}]}}}}"#
        ))
        .unwrap_or_else(|error| panic!("turn/start failed: {error:?}"));

    // 验证 turn 进入 blocked 状态并有 pending approval
    let store = SessionStore::open(&db_path).expect("reopen store");
    let pending = store.list_pending_approvals().expect("pending approvals");
    assert_eq!(pending.len(), 1, "exactly one pending approval expected");
    let approval_request = &pending[0];
    assert_eq!(approval_request.thread_id, thread_id);
    assert_eq!(
        approval_request.action.as_str(),
        "patch",
        "approval must be for the patch tool"
    );

    // 从 pending approval 获取 turn_id 并验证 turn 状态
    let turn_id = approval_request.turn_id.clone();
    let turn = store.get_turn(&turn_id).expect("turn exists");
    assert_eq!(turn.status, singularity_protocol::TurnStatus::Blocked);
    assert_eq!(turn.agent_loop_status, "blocked");
    drop(store);

    // approval/decision allow → 恢复 agent loop → 执行 edit → verification → final answer
    let decision = ApprovalDecision::new(
        approval_request.request_id.clone(),
        ApprovalOutcome::Allow,
        "operator approved patch",
    );
    let _decision_response = server
        .handle_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "method": "approval/decision",
                "id": 4,
                "params": decision,
            })
            .to_string(),
        )
        .expect("approval/decision");

    // 验证 turn 完成
    let store = SessionStore::open(&db_path).expect("reopen store");
    let completed_turn = store.get_turn(&turn_id).expect("turn");
    let trace = store.list_trace(&thread_id).expect("trace");
    if completed_turn.status != singularity_protocol::TurnStatus::Completed {
        let diagnostics = trace
            .iter()
            .map(|event| (&event.component, &event.summary, &event.payload))
            .collect::<Vec<_>>();
        eprintln!("approval resume diagnostics: {diagnostics:#?}");
    }
    assert_eq!(
        completed_turn.status,
        singularity_protocol::TurnStatus::Completed,
        "turn must reach Completed after approval resume"
    );
    assert_eq!(completed_turn.agent_loop_status, "completed");

    let checkpoint = store
        .get_turn_checkpoint(&turn_id)
        .expect("turn checkpoint")
        .expect("approval continuation checkpoint");
    assert_eq!(
        checkpoint["completion"]["workspace_mutated"], true,
        "approval resume must persist mutation evidence after the approved tool result"
    );
    assert!(
        checkpoint["tool_result_occurrences"]
            .as_array()
            .expect("tool result occurrences")
            .iter()
            .any(|occurrence| occurrence["result"]["tool_call_id"] == "call_patch_1"),
        "approval resume checkpoint must retain the approved tool result"
    );

    // 验证 workspace 文件已被修改
    assert_eq!(
        std::fs::read_to_string(&file_path).expect("read readme"),
        "after",
        "workspace file must be modified by the approved patch"
    );

    // 验证 approval 已消费
    assert!(store.list_pending_approvals().expect("pending").is_empty());
    assert_eq!(store.list_approval_decisions().expect("decisions").len(), 1);

    // 验证 trace 包含 agent_loop 完成记录
    assert!(
        trace.iter().any(|event| {
            event.component == "agent_loop" && event.payload["status"] == "completed"
        }),
        "trace must contain completed agent_loop event"
    );

    // 验证 provider 收到了恢复后的请求（tool result 在 history 中）
    let requests = seen_requests.lock().expect("requests");
    assert_eq!(
        requests.len(),
        3,
        "patch, verification, and finalization requests"
    );
    let approved_edit_result = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == singularity_model::ModelRole::Tool
                && message.tool_call_id.as_deref() == Some("call_patch_1")
        })
        .expect("approved patch result must precede the resumed model request");
    let approved_edit_payload: serde_json::Value =
        serde_json::from_str(&approved_edit_result.content).expect("edit result payload");
    assert_eq!(approved_edit_payload["ok"], true);
    let command_result = requests[2]
        .messages
        .iter()
        .find(|message| {
            message.role == singularity_model::ModelRole::Tool
                && message.tool_call_id.as_deref() == Some("call_verify_1")
        })
        .expect("verification command result must precede finalization");
    let command_payload: serde_json::Value =
        serde_json::from_str(&command_result.content).expect("command result payload");
    assert_eq!(command_payload["ok"], true);

    let provider_events = trace
        .iter()
        .filter(|event| {
            event.span_kind == Some(singularity_protocol::TraceSpanKind::ProviderAttempt)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        provider_events.len(),
        6,
        "three attempts need Start and End"
    );
    let mut provider_pairs = std::collections::BTreeMap::new();
    for event in provider_events {
        let span_id = event.span_id.as_deref().expect("provider span id");
        let pair = provider_pairs.entry(span_id).or_insert((None, None));
        match event.span_phase {
            Some(singularity_protocol::TraceSpanPhase::Start) => pair.0 = Some(event),
            Some(singularity_protocol::TraceSpanPhase::End) => pair.1 = Some(event),
            None => panic!("provider span phase"),
        }
    }
    assert_eq!(provider_pairs.len(), 3);
    for (_span_id, (start, end)) in provider_pairs {
        let start = start.expect("provider Start");
        let end = end.expect("provider End");
        let start_projection = start.span_projection.as_ref().expect("Start projection");
        let end_projection = end.span_projection.as_ref().expect("End projection");
        assert_eq!(start_projection.provider_name, end_projection.provider_name);
        assert_eq!(start_projection.model_name, end_projection.model_name);
        assert_eq!(start_projection.protocol, end_projection.protocol);
        assert_eq!(
            start_projection.operation_phase,
            end_projection.operation_phase
        );
        assert_eq!(start_projection.attempt_index, end_projection.attempt_index);
        assert_eq!(start_projection.retry_count, end_projection.retry_count);
        assert_eq!(
            end.span_status,
            Some(singularity_protocol::TraceSpanStatus::Ok)
        );
    }
    let verification_statuses = trace
        .iter()
        .filter(|event| event.span_phase == Some(singularity_protocol::TraceSpanPhase::End))
        .filter_map(|event| event.span_projection.as_ref())
        .filter_map(|projection| projection.verification.as_ref())
        .filter_map(|verification| verification.status)
        .collect::<Vec<_>>();
    assert!(
        verification_statuses
            .contains(&singularity_protocol::TraceVerificationStatus::CommandPassed)
    );
    assert!(
        verification_statuses.contains(&singularity_protocol::TraceVerificationStatus::GatePassed)
    );
    assert_eq!(
        trace
            .iter()
            .flat_map(|event| &event.metric_samples)
            .filter(|sample| {
                sample.kind
                    == singularity_protocol::TraceMetricSampleKind::ProviderCapabilityCacheMiss
            })
            .map(|sample| sample.count)
            .sum::<u64>(),
        2,
        "initial and resumed capability-cache observations must both persist"
    );
}

fn result_message(messages: &[serde_json::Value]) -> &serde_json::Value {
    messages
        .iter()
        .find_map(|message| message.get("result"))
        .expect("json-rpc result")
}
