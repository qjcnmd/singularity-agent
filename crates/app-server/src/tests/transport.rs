use super::framing::read_bounded_line_with_limit;
use super::supervisor::{run_server_with_io, run_turn_request};
use super::*;
use crate::state_paths::{
    FILE_BACKED_STORE_REQUIRED, ensure_home_outside_repo, resolve_app_server_state_paths,
};
use serde_json::{Value, json};
use singularity_agent::agent::AgentError;
use singularity_agent::session::{SessionManager, SessionMetadataKind};
use singularity_app_server::{AppServer, AppServerError, TurnFailureCause, TurnFailureStage};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, Provider, ProviderConfigSnapshot, ProviderError,
    ProviderProtocolContract,
};
use singularity_protocol::{JsonRpcId, JsonRpcMessage};
use singularity_store::{SessionStore, StoreError};
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test Tokio runtime")
        .block_on(future)
}

#[derive(Default)]
struct VecWriter(Vec<u8>);

impl AsyncWrite for VecWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.extend_from_slice(buffer);
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Clone, Default)]
struct CancellationProbe {
    requests: Arc<AtomicUsize>,
}

impl CancellationProbe {
    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl ExecutionStop for CancellationProbe {
    fn request_execution_stop(&self) {
        self.requests.fetch_add(1, Ordering::SeqCst);
    }

    fn execution_stop_requested(&self) -> bool {
        self.requests.load(Ordering::SeqCst) > 0
    }
}

fn test_output_channel(capacity: usize) -> (mpsc::Sender<Value>, mpsc::Receiver<Value>) {
    mpsc::channel(capacity)
}

fn progress_event() -> Value {
    JsonRpcMessage::notification(
        "item/agentMessage/delta",
        serde_json::json!({
            "item": {"item_id": "item_progress"},
            "delta": "progress",
            "event": singularity_protocol::EventMetadata {
                class: singularity_protocol::EventClass::Progress,
                delivery: singularity_protocol::EventDelivery::BestEffort,
            },
        }),
    )
    .expect("progress event")
    .to_wire_value()
}

#[test]
fn home_inside_current_repository_is_rejected_before_state_preparation() {
    let directory = tempfile::tempdir().expect("temp dir");
    let repo = directory.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("git marker");
    let inside = repo.join("nested").join("home");
    let outside = directory.path().join("elsewhere").join("home");
    let nested_cwd = repo.join("src").join("nested");
    std::fs::create_dir_all(&nested_cwd).expect("nested cwd");

    // home 位于仓库边界内（含尚不存在的尾部组件）→ 拒绝。
    let error = ensure_home_outside_repo(&inside, &nested_cwd).expect_err("inside rejected");
    assert!(error.contains("must not be inside"), "{error}");
    // 仓库外 home → 通过。
    ensure_home_outside_repo(&outside, &nested_cwd).expect("outside accepted");
    // 无 `.git` 边界时以 cwd 为边界。若测试临时目录本身位于另一个
    // Git 仓库（例如本机 D:\Temp\.git）内，则该前提不成立，跳过这段。
    let plain = directory.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain cwd");
    if singularity_core::find_workspace_root(&plain).expect("find plain root") == plain {
        let error = ensure_home_outside_repo(&plain.join("home"), &plain).expect_err("cwd inside");
        assert!(error.contains("must not be inside"), "{error}");
        ensure_home_outside_repo(&outside, &plain).expect("outside cwd accepted");
    }
}

#[test]
fn state_path_rejects_sqlite_uri_before_state_preparation() {
    for path in [
        ":memory:",
        " :MEMORY: ",
        "file::memory:?cache=shared",
        "file:memory-db?mode=memory&cache=shared",
        "file:memory-db?cache=shared&mode=MEMORY",
        "file:memory-db?mode=ro",
        "file:///state/rust-app-server.sqlite3",
        "FILE://localhost/state/rust-app-server.sqlite3",
    ] {
        let error = resolve_app_server_state_paths(path).expect_err("memory store rejected");
        assert_eq!(error, FILE_BACKED_STORE_REQUIRED);
    }
    assert!(resolve_app_server_state_paths("state/rust-app-server.sqlite3").is_ok());
}

#[test]
fn prepared_state_paths_use_the_canonical_directory() {
    let directory = tempfile::tempdir().expect("state directory");
    let configured = directory.path().join("nested").join("sessions.sqlite3");
    let db_path = prepare_app_server_state_paths(configured.to_str().expect("configured path"))
        .expect("prepared state path");
    let canonical_parent = std::fs::canonicalize(configured.parent().expect("parent"))
        .expect("canonical state directory");
    assert_eq!(
        Path::new(&db_path).parent(),
        Some(canonical_parent.as_path())
    );
    assert!(!Path::new(&db_path).exists());
}

#[test]
fn state_path_rejects_database_hard_link_before_store_open() {
    let directory = tempfile::tempdir().expect("state directory");
    let parent = directory.path().join("state");
    std::fs::create_dir(&parent).expect("create state directory");
    let source = directory.path().join("source.sqlite3");
    let database = parent.join("sessions.sqlite3");
    std::fs::write(&source, b"not a sqlite database").expect("source file");
    std::fs::hard_link(&source, &database).expect("database hard link");

    let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
        .expect_err("hard-linked database rejected");
    assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
}

#[test]
fn sqlite_uri_rejection_has_no_directory_side_effect() {
    let directory = tempfile::tempdir().expect("state directory");
    let missing_parent = directory.path().join("must-not-be-created");
    let configured = format!("file:{}?mode=memory", missing_parent.display());
    let error = prepare_app_server_state_paths(&configured)
        .expect_err("SQLite URI rejected before preparation");
    assert_eq!(error, FILE_BACKED_STORE_REQUIRED);
    assert!(!missing_parent.exists());
}

#[cfg(unix)]
#[test]
fn state_path_rejects_database_symlink_before_store_open() {
    let directory = tempfile::tempdir().expect("state directory");
    let parent = directory.path().join("state");
    std::fs::create_dir(&parent).expect("create state directory");
    let source = directory.path().join("source.sqlite3");
    let database = parent.join("sessions.sqlite3");
    std::fs::write(&source, b"not a sqlite database").expect("source file");
    std::os::unix::fs::symlink(&source, &database).expect("database symlink");

    let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
        .expect_err("symlinked database rejected");
    assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
}

#[cfg(windows)]
#[test]
fn state_path_rejects_database_reparse_link_before_store_open() {
    let directory = tempfile::tempdir().expect("state directory");
    let parent = directory.path().join("state");
    std::fs::create_dir(&parent).expect("create state directory");
    let source = directory.path().join("source.sqlite3");
    let database = parent.join("sessions.sqlite3");
    std::fs::write(&source, b"not a sqlite database").expect("source file");
    match std::os::windows::fs::symlink_file(&source, &database) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(1314) => return,
        Err(error) => panic!("database reparse link: {error}"),
    }

    let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
        .expect_err("reparse-linked database rejected");
    assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
}

#[test]
fn bounded_output_queue_backpressures_without_stopping_execution() {
    let (outputs, mut receiver) = test_output_channel(1);
    let cancellation = CancellationProbe::default();
    send_output(&outputs, &cancellation, serde_json::json!({"first": true}))
        .expect("first output fits");

    let (attempted_sender, attempted_receiver) = std_mpsc::channel();
    let sender_outputs = outputs.clone();
    let sender_cancellation = cancellation.clone();
    let sender = thread::spawn(move || {
        attempted_sender.send(()).expect("send attempted");
        send_output(
            &sender_outputs,
            &sender_cancellation,
            serde_json::json!({"second": true}),
        )
        .expect("bounded send");
    });
    attempted_receiver.recv().expect("send attempt");
    assert!(!sender.is_finished(), "full queue must backpressure");
    assert_eq!(cancellation.request_count(), 0);
    assert_eq!(
        receiver.blocking_recv().expect("first output")["first"],
        true
    );
    sender.join().expect("sender");
    assert_eq!(
        receiver.blocking_recv().expect("second output")["second"],
        true
    );
    assert_eq!(cancellation.request_count(), 0);
}

#[test]
fn bounded_output_queue_unblocks_a_worker_when_shutdown_is_requested() {
    let (outputs, _receiver) = test_output_channel(1);
    let cancellation = CancellationProbe::default();
    send_output(&outputs, &cancellation, serde_json::json!({"first": true}))
        .expect("first output fits");

    let (attempted_sender, attempted_receiver) = std_mpsc::channel();
    let sender_outputs = outputs.clone();
    let sender_cancellation = cancellation.clone();
    let sender = thread::spawn(move || {
        attempted_sender.send(()).expect("send attempted");
        send_output(
            &sender_outputs,
            &sender_cancellation,
            serde_json::json!({"second": true}),
        )
    });
    attempted_receiver.recv().expect("send attempt");
    assert!(
        !sender.is_finished(),
        "full queue must initially backpressure"
    );
    cancellation.request_execution_stop();
    assert_eq!(
        sender.join().expect("sender"),
        Err("stdout transport stopping".to_string())
    );
}

struct DisconnectedWriter;

impl AsyncWrite for DisconnectedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stdout closed",
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn stdout_writer_disconnect_stops_all_execution() {
    let (outputs, mut receiver) = test_output_channel(1);
    let cancellation = CancellationProbe::default();
    send_output(
        &outputs,
        &cancellation,
        serde_json::json!({"kind": "control"}),
    )
    .expect("control fits");
    drop(outputs);

    let mut stdout = DisconnectedWriter;
    let error = block_on(write_output_queue(
        &mut receiver,
        &mut stdout,
        &cancellation,
    ))
    .expect_err("writer disconnect is transport-fatal");

    assert!(error.starts_with("failed to write response:"));
    assert_eq!(cancellation.request_count(), 1);
}

#[test]
fn writer_drains_events_and_controls_in_send_order() {
    let (outputs, mut receiver) = test_output_channel(4);
    let cancellation = CancellationProbe::default();
    send_output(&outputs, &cancellation, progress_event()).expect("event fits");
    send_output(
        &outputs,
        &cancellation,
        serde_json::json!({"kind": "control"}),
    )
    .expect("control fits");
    drop(outputs);

    let mut stdout = VecWriter::default();
    block_on(write_output_queue(
        &mut receiver,
        &mut stdout,
        &cancellation,
    ))
    .expect("writer drains frames");
    let values = String::from_utf8(stdout.0)
        .expect("writer output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL frame"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0]["method"], "item/agentMessage/delta");
    assert_eq!(values[1]["kind"], "control");
    assert_eq!(cancellation.request_count(), 0);
}

#[test]
fn bounded_line_reader_strips_line_endings() {
    let mut reader = tokio::io::BufReader::new(&b"hello\r\nworld\n"[..]);
    assert_eq!(
        block_on(read_bounded_line_with_limit(&mut reader, 32))
            .expect("first line")
            .as_deref(),
        Some("hello")
    );
    assert_eq!(
        block_on(read_bounded_line_with_limit(&mut reader, 32))
            .expect("second line")
            .as_deref(),
        Some("world")
    );
    assert!(
        block_on(read_bounded_line_with_limit(&mut reader, 32))
            .expect("eof")
            .is_none()
    );
}

#[test]
fn bounded_line_reader_rejects_oversized_frames() {
    let mut reader = tokio::io::BufReader::new(&b"123456789\n"[..]);
    let error = block_on(read_bounded_line_with_limit(&mut reader, 4))
        .expect_err("oversized frame must fail closed");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("exceeds 4 bytes"));
}

#[test]
fn bounded_line_reader_accepts_frame_at_limit() {
    let mut reader = tokio::io::BufReader::new(&b"1234\n"[..]);
    assert_eq!(
        block_on(read_bounded_line_with_limit(&mut reader, 4))
            .expect("frame at limit")
            .as_deref(),
        Some("1234")
    );
}

#[test]
fn streaming_worker_maps_invalid_params_without_exposing_diagnostics() {
    let response = request_error_value(
        Some(JsonRpcId::Number(7)),
        &AppServerError::InvalidParams("secret-shaped diagnostic".to_string()),
    );

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 7);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["message"], "Invalid params");
    assert!(!response.to_string().contains("secret-shaped"));
}

#[test]
fn transport_error_exposes_store_agent_and_workspace_text() {
    let cases: Vec<(&str, AppServerError)> = vec![
        (
            "locked by another process",
            AppServerError::Store(StoreError::InvalidState(
                "locked by another process".to_string(),
            )),
        ),
        ("provider unavailable", {
            AppServerError::Agent(AgentError::Loop("provider unavailable".to_string()))
        }),
        ("workspace error: write denied", {
            AppServerError::Workspace("write denied".to_string())
        }),
    ];

    for (expected, error) in cases {
        let response = transport_error_value(Some(JsonRpcId::Number(7)), &error);
        assert_eq!(response["error"]["code"], -32603, "{expected}");
        let message = response["error"]["message"].as_str().expect("message");
        assert_ne!(message, "Internal error", "real text must not be masked");
        assert!(
            message.contains(expected),
            "expected real text {expected:?}, got {message:?}"
        );
    }
}

#[test]
fn transport_error_carries_original_turn_execution_text() {
    let original = "agent loop failed: provider returned 500";
    let response = transport_error_value(
        Some(JsonRpcId::Number(7)),
        &AppServerError::TurnExecution {
            stage: TurnFailureStage::AgentLoop,
            cause: TurnFailureCause::Internal,
            original: Some(original.to_string()),
        },
    );

    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], original);
}

#[test]
fn transport_error_carries_original_terminalization_text() {
    let original = "terminal metadata write failed: database locked";
    let response = transport_error_value(
        Some(JsonRpcId::Number(7)),
        &AppServerError::TurnTerminalization {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
            failure: singularity_app_server::TurnTerminalizationFailure::Store,
            original: Some(original.to_string()),
        },
    );

    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], original);
}

#[test]
fn turn_failure_events_are_queued_before_rpc_error_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        workspace.join("AGENTS.md"),
        vec![b'x'; singularity_core::PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1],
    )
    .expect("oversize AGENTS.md");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key".to_string()),
            _ => None,
        },
        None,
    );
    let mut server = AppServer::new(store, snapshot).with_sessions_dir(&sessions_dir);
    server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
    let started = server
        .handle_json(
            &json!({
                "jsonrpc": "2.0",
                "method": "thread/start",
                "id": 2,
                "params": {"cwd": workspace},
            })
            .to_string(),
        )
        .expect("thread/start");
    let session_id = started[1]["result"]["thread"]["thread_id"]
        .as_str()
        .expect("thread id")
        .to_string();
    let message: JsonRpcMessage = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "turn/start",
        "id": 3,
        "params": {
            "threadId": session_id,
            "input": [{"type": "text", "text": "run"}],
        }
    }))
    .expect("turn/start");
    let cancellation = server.cancellation_handle();
    let (outputs, mut receiver) = mpsc::channel(16);
    run_turn_request(server, message, outputs, cancellation).expect("turn worker");
    let mut values = Vec::new();
    while let Some(value) = receiver.blocking_recv() {
        values.push(value);
    }
    let rpc_response = values
        .iter()
        .position(|value| value["id"] == 3 && value["result"].is_object())
        .expect("RPC running response");
    let turn_error = values
        .iter()
        .position(|value| value["method"] == "turn/error")
        .expect("turn/error event");
    assert!(
        rpc_response < turn_error,
        "running RPC response must precede background terminal event: {values:?}"
    );
    assert_eq!(values[rpc_response]["result"]["turn"]["status"], "running");
    assert_eq!(
        values[turn_error]["params"]["error"]["cause"],
        "project_instructions"
    );
    assert!(
        values[turn_error]["params"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("project_instruction_file_too_large"))
    );
}

#[test]
fn transport_error_redacts_sensitive_diagnostic_text() {
    let error = AppServerError::Workspace(
        "cannot open SINGULARITY_API_KEY=sk-shape must-not-leak".to_string(),
    );
    let response = transport_error_value(Some(JsonRpcId::Number(7)), &error);

    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], "Internal error");
    assert!(!response.to_string().contains("sk-shape"));
}

#[test]
fn stdout_writer_join_obeys_the_shutdown_deadline() {
    let error = block_on(async {
        let (_release_sender, release_receiver) = tokio::sync::oneshot::channel::<()>();
        let mut writer = tokio::spawn(async move {
            let _ = release_receiver.await;
            Ok::<(), String>(())
        });
        match tokio::time::timeout(Duration::from_millis(20), &mut writer).await {
            Err(_) => {
                writer.abort();
                Err("timed out waiting for stdout writer during shutdown".to_string())
            }
            Ok(result) => result
                .map_err(|error| error.to_string())
                .and_then(|result| result),
        }
    })
    .expect_err("stalled writer must not outlive the deadline");

    assert_eq!(error, "timed out waiting for stdout writer during shutdown");
}

struct TestProvider {
    response: ModelTurnResponse,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    started: std_mpsc::SyncSender<()>,
    release: Mutex<std_mpsc::Receiver<()>>,
}

impl Provider for TestProvider {
    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("lock");
        seen_requests.push(request.clone());
        self.started.send(()).expect("signal provider start");
        self.release
            .lock()
            .expect("release lock")
            .recv()
            .expect("release provider");
        let mut response = self.response.clone();
        response.request_id = request.request_id.clone();
        Ok(response)
    }

    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }
}

#[test]
fn terminal_storage_fail_stop_over_stdio_supervisor() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let index_path = temp.path().join("index.sqlite3");
    let store = SessionStore::open(&index_path).expect("store");

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let (provider_started_tx, provider_started_rx) = std_mpsc::sync_channel(1);
    let (provider_release_tx, provider_release_rx) = std_mpsc::sync_channel(1);
    let provider = Arc::new(TestProvider {
        response: ModelTurnResponse::completed("request_1", "response_1", "all done"),
        seen_requests: Arc::clone(&seen_requests),
        started: provider_started_tx,
        release: Mutex::new(provider_release_rx),
    });

    let session_id = "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d";
    let session = SessionManager::create_with_id(&workspace, &sessions_dir, session_id)
        .expect("create session");
    let session_path = session.path().to_path_buf();
    let original_session_permissions = std::fs::metadata(&session_path)
        .expect("session metadata before storage fault")
        .permissions();
    store
        .insert_session(&singularity_store::SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: session.path().to_string_lossy().to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: None,
            model: Some("gpt-test".to_string()),
            status: None,
            created_at: "2026-08-20T00:00:00Z".to_string(),
            updated_at: "2026-08-20T00:00:00Z".to_string(),
            token_usage: json!({}),
        })
        .expect("insert session");
    drop(session);

    let server_sessions_dir = sessions_dir.clone();
    let server_provider = Arc::clone(&provider);
    let server_session_path = session_path.clone();
    let server_original_permissions = original_session_permissions.clone();
    let (server_result, wire_bytes) = block_on(async move {
        let handle = tokio::runtime::Handle::current();
        let snapshot = ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            Some(handle),
        );

        let server = AppServer::new(store, snapshot)
            .with_sessions_dir(&server_sessions_dir)
            .with_test_provider(server_provider as Arc<dyn Provider + Send + Sync>);

        let (server_stdin, mut client_stdin) = tokio::io::duplex(65536);
        let (client_stdout, server_stdout) = tokio::io::duplex(65536);
        let mut reader = tokio::io::BufReader::new(server_stdin);
        let mut client_reader = tokio::io::BufReader::new(client_stdout);

        let server_task =
            tokio::spawn(
                async move { run_server_with_io(server, &mut reader, server_stdout).await },
            );

        // 1. Send initialize
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1,\"params\":{\"clientInfo\":{\"name\":\"test\",\"title\":\"Test\",\"version\":\"0.1.0\"}}}\n")
            .await
            .expect("write initialize");

        // 2. Wait for initialize response
        let mut init_response_line = String::new();
        client_reader
            .read_line(&mut init_response_line)
            .await
            .expect("read initialize response");
        assert!(init_response_line.contains("\"result\""));

        // 3. Send initialized
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}\n")
            .await
            .expect("write initialized");

        // A request/response barrier proves that initialized was processed before turn/start.
        client_stdin
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"agent/capability\",\"id\":99,\"params\":{}}\n",
            )
            .await
            .expect("write capability barrier");
        let mut capability_response_line = String::new();
        client_reader
            .read_line(&mut capability_response_line)
            .await
            .expect("read capability barrier");
        assert!(capability_response_line.contains("\"result\""));

        // 4. Send turn/start
        let turn_start_line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"turn/start\",\"id\":2,\"params\":{{\"threadId\":\"{session_id}\",\"input\":[{{\"type\":\"text\",\"text\":\"run task\"}}]}}}}\n"
        );
        client_stdin
            .write_all(turn_start_line.as_bytes())
            .await
            .expect("write turn/start");

        tokio::task::spawn_blocking(move || provider_started_rx.recv())
            .await
            .expect("join provider start wait")
            .expect("provider started");
        let mut permissions = server_original_permissions;
        permissions.set_readonly(true);
        std::fs::set_permissions(&server_session_path, permissions)
            .expect("make session read-only for terminalization fault");
        provider_release_tx
            .send(())
            .expect("release provider after storage fault setup");

        let mut wire_bytes = Vec::new();
        wire_bytes.extend_from_slice(init_response_line.as_bytes());
        let _ = client_reader.read_to_end(&mut wire_bytes).await;
        let server_res = server_task.await.expect("join server task");
        drop(client_stdin);
        (server_res, wire_bytes)
    });

    // 5. turn worker 错误使 supervisor 结束 stdio 连接
    assert!(
        server_result.is_err(),
        "supervisor must exit with error when turn worker experiences fatal terminal storage failure"
    );

    std::fs::set_permissions(&session_path, original_session_permissions)
        .expect("restore session writability after storage fault");

    // 6. 客户端观察到 EOF/连接关闭并读取全部 wire frames
    let wire_output = String::from_utf8(wire_bytes).expect("UTF-8 wire output");
    let messages: Vec<Value> = wire_output
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect();

    // 3. wire 可以收到安全、无敏感数据的连接级 fatal storage diagnostic
    let fatal_diagnostic = messages.iter().find(|msg| {
        msg["method"] == "agent/diagnostic"
            && msg["params"]["severity"] == "error"
            && msg["params"]["code"] == "storage_fatal"
    });
    assert!(
        fatal_diagnostic.is_some(),
        "wire must receive sanitized storage_fatal diagnostic: {wire_output}"
    );
    let diag_str = fatal_diagnostic.unwrap().to_string();
    assert!(
        !diag_str.contains("test-key") && !diag_str.contains("sk-"),
        "diagnostic must not leak sensitive data"
    );

    // 4. 不产生 item/completed、item/failed、turn/completed 或 turn/error 终态事件
    assert!(
        !messages.iter().any(|msg| msg["method"] == "turn/completed"),
        "must not produce turn/completed"
    );
    assert!(
        !messages.iter().any(|msg| msg["method"] == "turn/error"),
        "must not produce turn/error"
    );
    assert!(
        !messages
            .iter()
            .any(|msg| msg["method"] == "item/completed" || msg["method"] == "item/failed"),
        "must not produce item/completed or item/failed"
    );

    // 2. 已经写入的 turn_started 保留在 JSONL
    let session_on_disk = SessionManager::open_existing(&session_path).expect("open session file");
    let entries = session_on_disk.metadata_entries();
    assert!(
        entries
            .iter()
            .any(|entry| entry.kind() == SessionMetadataKind::TurnStarted),
        "turn_started must be persisted in JSONL"
    );
    assert!(
        !entries.iter().any(|entry| {
            entry.kind() == SessionMetadataKind::TurnCompleted
                || entry.kind() == SessionMetadataKind::TurnInterrupted
        }),
        "terminal metadata must not have been persisted during double fault"
    );

    let initial_requests = seen_requests.lock().unwrap().len();
    assert_eq!(
        initial_requests, 1,
        "provider was called exactly once during turn"
    );

    // 7. 用同一 session 文件创建新 app-server 实例
    let new_store = SessionStore::open(&index_path).expect("open store again");
    let new_snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key".to_string()),
            _ => None,
        },
        None,
    );
    let mut new_server = AppServer::new(new_store, new_snapshot)
        .with_sessions_dir(&sessions_dir)
        .with_test_provider(Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>);

    new_server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"initialize","id":10,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        )
        .expect("initialize new server");
    new_server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized new server");

    // 8. reopen/resume 收敛为 interrupted
    let resume_response = new_server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":11,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("thread/resume succeeds");
    assert_eq!(
        resume_response[0]["result"]["thread"]["lastTurnStatus"], "interrupted",
        "resume converges uncompleted turn to interrupted"
    );

    // 9. 不重执行工具或 Provider 调用
    assert_eq!(
        seen_requests.lock().unwrap().len(),
        initial_requests,
        "reopen/resume must not re-execute provider calls or tools"
    );
}
