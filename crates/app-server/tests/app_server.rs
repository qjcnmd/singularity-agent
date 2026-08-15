//! AppServer protocol、recovery 和 sandbox 边界测试。

use singularity_app_server::{AppServer, AppServerError};
use singularity_model::ProviderConfigSnapshot;
#[cfg(windows)]
use singularity_protocol::ConversationRole;
use singularity_store::{SessionStore, StoreError};
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

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn app_server(store: SessionStore) -> AppServer {
    // 隔离 trust 存储：挂载独立临时 trust home，避免读取/写入真实用户 trust.json。
    let trust_home = Box::leak(Box::new(tempfile::tempdir().expect("trust home")));
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
        ),
    )
    .with_trust_home(trust_home.path())
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
    // 单 worker 传输无 cursor/gap：订阅只确认结果，事件全量发。
    assert_eq!(subscription.len(), 1);
    let subscription_result = result_message(&subscription);
    assert_eq!(
        subscription_result["eventTypes"],
        serde_json::json!(["thread/started", "turn/started"])
    );
    assert_eq!(subscription_result["cursor"], 0);

    let thread = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"thread/start","id":4,"params":{"model":"gpt-test"}}"#,
        )
        .unwrap();
    let thread_result = result_message(&thread);
    let thread_id = thread_result["thread"]["thread_id"].as_str().unwrap();
    assert_eq!(thread_result["thread"]["model"], "gpt-test");
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
    assert!(
        thread_started["params"]["event"]
            .get("recoveryQuery")
            .is_none()
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

    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":432,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(resumed[0]["result"]["thread"]["status"], "active");

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

    let overridden_fork = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/fork","id":45,"params":{{"threadId":"{thread_id}","sandboxMode":"workspace-write","approvalPolicy":"on-request"}}}}"#
        ))
        .unwrap();
    assert_eq!(overridden_fork[0]["error"]["code"], -32602);

    let deleted = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/delete","id":46,"params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .unwrap();
    assert_eq!(deleted[0]["result"]["deleted"], true);
}

#[test]
fn event_subscribe_returns_result_without_gap_or_type_filtering() {
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

    // 事件全量发：订阅前的 thread/start 也携带 thread/started 事件。
    let before_subscribe = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"thread/start","id":2,"params":{"model":"gpt-test"}}"#,
        )
        .expect("thread start");
    assert!(
        before_subscribe
            .iter()
            .any(|message| message["method"] == "thread/started")
    );

    // cursor 参数不再参与校验；订阅返回单一结果且不发射 event/gap。
    let subscription = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":3,"params":{"eventTypes":[],"cursor":0}}"#,
        )
        .expect("subscription result");
    assert_eq!(subscription.len(), 1);
    assert_eq!(subscription[0]["result"]["cursor"], 0);
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

    let turn_error = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{}","input":[{{"type":"text","text":"do not run"}}]}}}}"#,
            active_missing.thread_id
        ))
        .expect_err("turn start without workspace must fail");
    // 信任门控前置检查在创建 turn 前即失败（比旧路径更早，不留幻影 turn 行）。
    assert!(matches!(turn_error, AppServerError::Workspace(_)));

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

    let unknown_method = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"approval/request","id":3,"params":{}}"#)
        .unwrap();
    assert_eq!(unknown_method[0]["error"]["code"], -32601);
    assert_eq!(unknown_method[0]["error"]["message"], "Method not found");

    let unknown_artifact = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"artifact/fetch","id":4,"params":{"artifactId":"missing"}}"#,
        )
        .unwrap();
    assert_eq!(unknown_artifact[0]["error"]["code"], -32601);
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
    assert_eq!(
        terminal["result"]["turn"]["status"], "completed",
        "turn/start terminal: {terminal}"
    );
    assert_eq!(terminal["result"]["turn"]["agent_loop_status"], "completed");
    output.recv_method("turn/completed", Duration::from_secs(2));
    let request_bodies = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("provider request sequence");
    assert_eq!(
        request_bodies.len(),
        1,
        "static capability contract issues exactly one streaming request"
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
    // 跨进程 interrupt 不再投递（无持久化轮询监视器）；secondary 只做 workspace
    // 串行化验证后退出，由 primary 在同一 stdio 上进程内直连取消。
    shutdown_process(
        &mut secondary,
        &mut secondary_input,
        &mut secondary_output,
        12,
    );

    send_json(
        &mut primary_input,
        serde_json::json!({
            "jsonrpc": "2.0", "method": "turn/interrupt",
            "id": 12,
            "params": {"turnId": turn_id}
        }),
    );
    let interrupt = primary_output.recv_id(12, Duration::from_secs(2));
    assert_eq!(interrupt["result"]["status"], "cancel_requested");
    assert_eq!(interrupt["result"]["agent_loop_status"], "cancel_requested");

    let terminal = primary_output.recv_id(3, Duration::from_secs(2));
    assert_eq!(terminal["result"]["turn"]["status"], "interrupted");
    assert_eq!(terminal["result"]["turn"]["agent_loop_status"], "cancelled");
    release.send(()).expect("release provider");
    provider_worker.join().expect("provider worker joins");
    shutdown_process(&mut primary, &mut primary_input, &mut primary_output, 13);
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
                "item/agentMessage/delta"
            ]}
        }),
    );
    let subscription = output.recv_id(99, Duration::from_secs(2));
    assert!(subscription.get("result").is_some());
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
        let (mut stream, _) = listener
            .accept()
            .expect("accept streaming provider request");
        let request_body = read_http_json_body(&mut stream);
        let request: serde_json::Value =
            serde_json::from_str(&request_body).expect("provider request json");
        assert!(
            request["stream"] == true,
            "static capability contract must issue one streaming request"
        );
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
        stream.flush().expect("flush streaming completion");
        // 保留连接直到 child 消费完 SSE 响应：若在 child 的解码回调
        // （store 写）尚未消费完时关闭 socket，Windows 会以 RST 终止连接，
        // child 的 body read 报 provider_response_body_read_failed。
        // 不能阻塞等待 EOF（child 需 worker 关闭才能得到 EOF，会死锁），
        // 因此给足解码/持久化延迟余量后再关闭。
        thread::sleep(Duration::from_millis(750));
        requests_tx
            .send(vec![request])
            .expect("send request sequence");
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

/// TCP 回环端到端：在临时目录启动 app-server daemon（TCP 监听、端口 0），从 stderr
/// 读回实际端口，建立单连接并走 initialize → initialized → server/capabilities →
/// server/shutdown。daemon 语义：shutdown 结束当前连接但不退出进程（空闲自停接管），
/// 因此测试用短空闲超时验证"断开后 daemon 在空闲窗口内自动退出"。
#[test]
fn app_server_serves_json_lines_over_tcp_loopback() {
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const LISTENING_PREFIX: &str = "SINGULARITY_APP_SERVER_LISTENING ";
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let stderr_path = dir.path().join("app-server-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
    let mut child = Command::new(app_server_bin())
        .current_dir(dir.path())
        .env("SINGULARITY_APP_SERVER_DB", &db_path)
        .env("SINGULARITY_APP_SERVER_LISTEN", "tcp://127.0.0.1:0")
        .env("SINGULARITY_APP_SERVER_IDLE_TIMEOUT_MS", "500")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn app-server in TCP mode");

    let addr = wait_for_listen_address(&stderr_path, LISTENING_PREFIX);
    assert_eq!(
        addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );

    let stream = TcpStream::connect(addr).expect("connect to app-server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let mut connection = LineReader::new(stream);

    // 单连接单流（不 clone）。按 id 匹配 recv_id 消费帧，因此 notification（如 initialized
    // 无响应）、事件帧与合帧都能正确对齐，不假设"每请求恰好一帧"。
    write_line(&mut connection.stream, br#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"tcp-e2e","title":"TCP E2E","version":"0.1.0"}}}"#);
    let init = connection.recv_id(1);
    assert_eq!(init["result"]["platformFamily"], "local");

    // initialized 是 notification，通常无响应；不读帧，直接发 capabilities。
    write_line(
        &mut connection.stream,
        br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    );
    write_line(
        &mut connection.stream,
        br#"{"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}}"#,
    );
    let capabilities = connection.recv_id(2);
    assert!(
        capabilities["result"]["transports"]
            .as_array()
            .expect("capability transports")
            .iter()
            .any(|transport| transport["transport"] == "stdio" && transport["available"] == true),
        "tcp connects to the same static capability contract: {capabilities}"
    );

    write_line(
        &mut connection.stream,
        br#"{"jsonrpc":"2.0","method":"server/shutdown","id":3,"params":{}}"#,
    );
    let shutdown = connection.recv_id(3);
    assert_eq!(shutdown["result"]["shutdown"], true);

    // shutdown 只结束当前连接；daemon 不立即退出，而是由空闲自停接手。连接关闭后，
    // daemon 应在空闲超时（此处 500ms）后正常退出。
    drop(connection);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("poll app-server") {
            Some(status) => {
                assert!(status.success(), "app-server exited with {status}");
                break;
            }
            None => {
                assert!(
                    Instant::now() < deadline,
                    "daemon did not idle-stop after connection close"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 从 daemon 的 stderr 日志文件轮询读取 `SINGULARITY_APP_SERVER_LISTENING <addr>`
/// announce 行，返回实际回环地址。stderr 重定向到文件（父进程不读管道），先给缓冲
/// 余量再读端口，避免对端读就绪尚未登记时首帧丢失。
fn wait_for_listen_address(
    stderr_path: &std::path::Path,
    listening_prefix: &str,
) -> std::net::SocketAddr {
    use std::time::Duration as StdDuration;
    use std::time::Instant as StdInstant;
    std::thread::sleep(StdDuration::from_millis(50));
    let deadline = StdInstant::now() + StdDuration::from_secs(5);
    loop {
        let contents = std::fs::read_to_string(stderr_path).expect("read stderr");
        if let Some(line) = contents
            .lines()
            .find(|line| line.starts_with(listening_prefix))
        {
            return line
                .trim()
                .strip_prefix(listening_prefix)
                .expect("listening address line")
                .parse::<std::net::SocketAddr>()
                .expect("listening socket address");
        }
        assert!(
            StdInstant::now() < deadline,
            "timed out waiting for listen address; stderr={contents:?}"
        );
        std::thread::sleep(StdDuration::from_millis(10));
    }
}

/// TCP daemon 多连接：同一 daemon 接受第二个连接，且每连接独立走 initialize/initialized
/// 握手（第二个连接必须先握手才能调用受保护方法）；announce 只打一次。
#[test]
fn tcp_daemon_serves_second_connection_with_independent_handshake() {
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    const LISTENING_PREFIX: &str = "SINGULARITY_APP_SERVER_LISTENING ";
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let stderr_path = dir.path().join("app-server-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
    let mut child = Command::new(app_server_bin())
        .current_dir(dir.path())
        .env("SINGULARITY_APP_SERVER_DB", &db_path)
        .env("SINGULARITY_APP_SERVER_LISTEN", "tcp://127.0.0.1:0")
        .env("SINGULARITY_APP_SERVER_IDLE_TIMEOUT_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn app-server in TCP mode");
    let addr = wait_for_listen_address(&stderr_path, LISTENING_PREFIX);

    // 第一个连接：握手 + 访问受保护方法后断开。
    {
        let stream = TcpStream::connect(addr).expect("connect first");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut conn = LineReader::new(stream);
        write_line(
            &mut conn.stream,
            br#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"c1","title":"C1","version":"0.1.0"}}}"#,
        );
        assert_eq!(conn.recv_id(1)["result"]["platformFamily"], "local");
        write_line(
            &mut conn.stream,
            br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        );
        write_line(
            &mut conn.stream,
            br#"{"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}}"#,
        );
        assert!(conn.recv_id(2)["result"]["transports"].is_array());
    }

    // 第二个连接：握手是 per-connection 的，必须重新 initialize 才能访问受保护方法；
    // 未握手时 server/capabilities 应返回 not_initialized，而不是沿用第一个连接的状态。
    let stream = TcpStream::connect(addr).expect("connect second");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut conn = LineReader::new(stream);
    // 未握手直接调用受保护方法 → -32002 not_initialized。
    write_line(
        &mut conn.stream,
        br#"{"jsonrpc":"2.0","method":"server/capabilities","id":10,"params":{}}"#,
    );
    assert_eq!(conn.recv_id(10)["error"]["code"], -32002);
    // 握手后同一方法成功。
    write_line(
        &mut conn.stream,
        br#"{"jsonrpc":"2.0","method":"initialize","id":11,"params":{"clientInfo":{"name":"c2","title":"C2","version":"0.1.0"}}}"#,
    );
    assert_eq!(conn.recv_id(11)["result"]["platformFamily"], "local");
    write_line(
        &mut conn.stream,
        br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    );
    write_line(
        &mut conn.stream,
        br#"{"jsonrpc":"2.0","method":"server/capabilities","id":12,"params":{}}"#,
    );
    assert!(conn.recv_id(12)["result"]["transports"].is_array());

    // announce 只打一次：stderr 文件始终只有一行 announce。
    let stderr = std::fs::read_to_string(&stderr_path).expect("read stderr");
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with(LISTENING_PREFIX))
            .count(),
        1,
        "daemon must announce the listen address only once"
    );

    // 关闭第二连接后显式 shutdown 进程（短 idle 已设为 60s，这里直接 kill 回收避免等待）。
    drop(conn);
    let _ = child.kill();
    let _ = child.wait();
}

/// TCP daemon 空闲自停：建立连接并断开后不立即退出，但在无新连接的空闲窗口内自动退出。
#[test]
fn tcp_daemon_idle_stops_once_no_connection_arrives() {
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const LISTENING_PREFIX: &str = "SINGULARITY_APP_SERVER_LISTENING ";
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let stderr_path = dir.path().join("app-server-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("stderr file");
    let mut child = Command::new(app_server_bin())
        .current_dir(dir.path())
        .env("SINGULARITY_APP_SERVER_DB", &db_path)
        .env("SINGULARITY_APP_SERVER_LISTEN", "tcp://127.0.0.1:0")
        .env("SINGULARITY_APP_SERVER_IDLE_TIMEOUT_MS", "400")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn app-server in TCP mode");
    let addr = wait_for_listen_address(&stderr_path, LISTENING_PREFIX);

    // 建立并立即关闭连接：daemon 回到 accept 状态，不应退出（连接被接受但不触发退出）。
    {
        let stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("read timeout");
        // 不发送任何请求即关闭：run_with_io 读到 EOF 返回，daemon 回到 accept 循环。
        drop(stream);
    }

    // 短暂断言进程仍存活（空闲窗口内不退出）。
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        child.try_wait().expect("poll").is_none(),
        "daemon must survive within the idle window"
    );

    // 超过空闲窗口后应自动退出（exit 0），无需任何连接。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("poll app-server") {
            Some(status) => {
                assert!(status.success(), "idle daemon exited with {status}");
                break;
            }
            None => {
                assert!(
                    Instant::now() < deadline,
                    "daemon did not idle-stop after the idle window"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 写一条以 `\n` 结尾的 JSON-Lines 帧；`Write` 经 `std::io::Write` 提供。
fn write_line(stream: &mut std::net::TcpStream, frame: &[u8]) {
    std::io::Write::write_all(stream, frame).expect("write app-server frame");
    std::io::Write::write_all(stream, b"\n").expect("terminate app-server frame");
    std::io::Write::flush(stream).expect("flush app-server frame");
}

/// JSON-Lines 行读取器：维护跨调用累积缓冲，`\n` 处切出完整帧（空行跳过），
/// 返回 id 匹配的响应帧；事件/notification 确认/合帧都能正确应对。
struct LineReader {
    stream: std::net::TcpStream,
    pending: Vec<u8>,
}

impl LineReader {
    fn new(stream: std::net::TcpStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
        }
    }

    /// 读下一条完整（非空）JSON-Lines 帧。
    fn next_frame(&mut self) -> serde_json::Value {
        loop {
            if let Some(newline) = self.pending.iter().position(|b| *b == b'\n') {
                if newline == 0 {
                    self.pending.drain(..1);
                    continue;
                }
                let frame: serde_json::Value =
                    serde_json::from_slice(&self.pending[..newline]).expect("valid JSON frame");
                self.pending.drain(..=newline);
                return frame;
            }
            let mut chunk = [0u8; 1024];
            let n = std::io::Read::read(&mut self.stream, &mut chunk)
                .expect("read app-server frame bytes");
            assert!(n != 0, "app-server closed the connection before a response");
            self.pending.extend_from_slice(&chunk[..n]);
        }
    }

    /// 逐帧读取直到拿到 id 匹配的响应帧；非匹配帧丢弃。
    fn recv_id(&mut self, id: i64) -> serde_json::Value {
        loop {
            let frame = self.next_frame();
            if frame["id"] == id {
                return frame;
            }
        }
    }
}

fn result_message(messages: &[serde_json::Value]) -> &serde_json::Value {
    messages
        .iter()
        .find_map(|message| message.get("result"))
        .expect("json-rpc result")
}
