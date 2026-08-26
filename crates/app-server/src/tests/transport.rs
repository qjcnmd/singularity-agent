use super::framing::read_bounded_line_with_limit;
use super::supervisor::{run_server_with_io, run_turn_request};
use super::*;
use crate::{
    AppServer, AppServerCancellationHandle, AppServerError, ProviderFailureKind, TurnFailureCause,
    TurnFailureStage,
};
use serde_json::{Value, json};
use singularity_agent::session::{SessionManager, SessionMetadataKind};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, Provider, ProviderConfigSnapshot, ProviderError,
    ProviderProtocolContract,
};
use singularity_protocol::{JsonRpcId, JsonRpcMessage};
use std::io;
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
}

fn test_output_channel(capacity: usize) -> (mpsc::Sender<Value>, mpsc::Receiver<Value>) {
    mpsc::channel(capacity)
}

fn test_cancellation_handle() -> AppServerCancellationHandle {
    let snapshot = ProviderConfigSnapshot::capture(|_| None, shared_provider_runtime_handle());
    AppServer::new(snapshot, ".singularity/sessions").cancellation_handle()
}

/// transport 测试共享的注入 runtime：provider 异步执行一律由上层提供。
/// 本模块同时被 lib 与 bin 两个 crate root 编译，不能引用 lib 的 tests 模块。
fn shared_provider_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("shared test provider runtime")
        })
        .handle()
        .clone()
}

fn progress_event() -> Value {
    JsonRpcMessage::notification(
        "item/agentMessage/delta",
        serde_json::json!({
            "item": {"itemId": "item_progress"},
            "delta": "progress",
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
    let error = singularity_core::ensure_singularity_home_outside_workspace(&inside, &nested_cwd)
        .expect_err("inside rejected");
    assert!(error.contains("must not be inside"), "{error}");
    // 仓库外 home → 通过。
    singularity_core::ensure_singularity_home_outside_workspace(&outside, &nested_cwd)
        .expect("outside accepted");
    // 无 `.git` 边界时以 cwd 为边界。若测试临时目录本身位于另一个
    // Git 仓库（例如本机 D:\Temp\.git）内，则该前提不成立，跳过这段。
    let plain = directory.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain cwd");
    if singularity_core::find_workspace_root(&plain).expect("find plain root") == plain {
        let error = singularity_core::ensure_singularity_home_outside_workspace(
            &plain.join("home"),
            &plain,
        )
        .expect_err("cwd inside");
        assert!(error.contains("must not be inside"), "{error}");
        singularity_core::ensure_singularity_home_outside_workspace(&outside, &plain)
            .expect("outside cwd accepted");
    }
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
fn bounded_output_queue_backpressures_async_sender_without_stopping_execution() {
    // async 入口走纯 `send().await`：队列满时 future 保持 pending 形成背压，
    // writer 正常消费后完成，全程不触发全局停止。
    let (outputs, mut receiver) = test_output_channel(1);
    let cancellation = test_cancellation_handle();
    block_on(async move {
        send_output_async(
            outputs.clone(),
            cancellation.clone(),
            serde_json::json!({"first": true}),
        )
        .await
        .expect("first output fits");

        let sender = tokio::spawn(send_output_async(
            outputs,
            cancellation.clone(),
            serde_json::json!({"second": true}),
        ));
        // 队列仍满：让步后 sender 必须还在等待，且未触发停止。
        tokio::task::yield_now().await;
        assert!(!sender.is_finished(), "full queue must backpressure");
        assert!(!cancellation.execution_stop_requested());
        assert_eq!(receiver.recv().await.expect("first output")["first"], true);
        sender.await.expect("sender task").expect("bounded send");
        assert_eq!(
            receiver.recv().await.expect("second output")["second"],
            true
        );
        assert!(!cancellation.execution_stop_requested());
    });
}

#[test]
fn bounded_output_queue_unblocks_a_worker_when_receiver_closes() {
    // 同步入队没有 stop 标志轮询逃逸；唯一解除阻塞的路径是 writer 侧消失：
    // receiver drop 后阻塞的 blocking_send 以 channel 关闭失败并触发全局停止。
    let (outputs, receiver) = test_output_channel(1);
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
    drop(receiver);
    assert_eq!(
        sender.join().expect("sender"),
        Err("stdout transport unavailable".to_string())
    );
    assert_eq!(cancellation.request_count(), 1);
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
fn streaming_worker_maps_invalid_params_with_diagnostics() {
    // 流式 lane 与 ordinary lane 一致透出可行动的错误原因。
    let response = request_error_value(
        Some(JsonRpcId::Number(7)),
        &AppServerError::InvalidParams("invalid model selector: base-model#unknown".to_string()),
    );
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 7);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(
        response["error"]["message"],
        "invalid model selector: base-model#unknown"
    );
}

#[test]
fn streaming_worker_preserves_invalid_params_diagnostic() {
    let response = request_error_value(
        Some(JsonRpcId::Number(8)),
        &AppServerError::InvalidParams("provider: unsupported selector".to_string()),
    );
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 8);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(
        response["error"]["message"],
        "provider: unsupported selector"
    );
}

#[test]
fn transport_error_exposes_store_agent_and_workspace_text() {
    let cases: Vec<(&str, AppServerError)> = vec![
        (
            "locked by another process",
            AppServerError::Store("locked by another process".to_string()),
        ),
        ("provider: unavailable", {
            AppServerError::TurnExecution {
                stage: TurnFailureStage::AgentLoop,
                cause: TurnFailureCause::Provider(ProviderFailureKind::Unknown),
                original: Some("provider: unavailable".to_string()),
            }
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
            failure: crate::TurnTerminalizationFailure::Store,
            original: Some(original.to_string()),
        },
    );

    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], original);
}

#[test]
fn turn_start_prepare_failure_returns_direct_error_response() {
    // 准备阶段（project instructions 真 I/O 错误：AGENTS.md 不是常规文件）失败：
    // turn/start 直接回错误响应，不发 turn/started、不写 turn_started、
    // 也不制造 turn/error 终态事件。预算超限不在此路径——超限走截断 + 告警。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("AGENTS.md")).expect("AGENTS.md as a directory");
    let sessions_dir = temp.path().join("sessions");
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("test-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key".to_string()),
            _ => None,
        },
        shared_provider_runtime_handle(),
    );
    let mut server = AppServer::new(snapshot, &sessions_dir);
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
    let session_id = started[1]["result"]["thread"]["threadId"]
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
    let claim = match server.claim_turn(message) {
        Ok(crate::TurnClaim::Accepted(claim)) => claim,
        Ok(other) => panic!("claim must accept the turn: {other:?}"),
        Err(error) => panic!("claim failed: {error}"),
    };
    run_turn_request(server, outputs, cancellation, claim).expect("turn worker");
    let mut values = Vec::new();
    while let Some(value) = receiver.blocking_recv() {
        values.push(value);
    }
    assert_eq!(
        values.len(),
        1,
        "only the direct error response: {values:?}"
    );
    assert_eq!(values[0]["id"], 3);
    assert_eq!(values[0]["error"]["code"], -32603);
    assert!(
        values[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("project_instruction_unsupported_file_type")),
        "error must carry the project instruction cause: {values:?}"
    );
    // JSONL 无 turn 痕迹：直接错误响应是唯一事实。
    let record = singularity_runtime::read_thread_summary(&sessions_dir, &session_id)
        .expect("session record");
    assert_eq!(
        record.status, None,
        "prepare failure must not activate session"
    );
    let session_on_disk =
        SessionManager::open_existing(&sessions_dir.join(format!("{session_id}.jsonl")))
            .expect("open session file");
    assert!(
        session_on_disk.metadata_entries().is_empty(),
        "prepare failure must not persist turn_started"
    );
}

#[test]
fn transport_error_preserves_workspace_diagnostic_text() {
    let error = AppServerError::Workspace("cannot open provider: local".to_string());
    let response = transport_error_value(Some(JsonRpcId::Number(7)), &error);

    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        response["error"]["message"],
        "workspace error: cannot open provider: local"
    );
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

/// 重连后新 turn 用的非阻塞 provider：直接返回固定响应，仅计数调用。
struct ReopenProvider {
    response: ModelTurnResponse,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl Provider for ReopenProvider {
    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("lock");
        seen_requests.push(request.clone());
        let mut response = self.response.clone();
        response.request_id = request.request_id.clone();
        Ok(response)
    }

    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }
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
    // 真实文件系统只读故障：Provider 开始后锁死会话 JSONL 再放行。首个失败
    // 点可能是执行期 item append 或终态 metadata append（SessionManager 每次
    // 写都重开文件）；两者同属 terminalize/persist_failure_state 的 fail-stop
    // 合同，断言只依赖该合同：无假终态事件、storage_fatal 诊断、连接 EOF、
    // Provider 恰好执行一次、重连后由 JSONL 恢复路径收敛。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
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
                "SINGULARITY_MODEL" => Some("test-model".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            handle,
        );

        let server = AppServer::new(snapshot, &server_sessions_dir)
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
                b"{\"jsonrpc\":\"2.0\",\"method\":\"provider/status\",\"id\":99,\"params\":{}}\n",
            )
            .await
            .expect("write status barrier");
        let mut status_response_line = String::new();
        client_reader
            .read_line(&mut status_response_line)
            .await
            .expect("read status barrier");
        assert!(status_response_line.contains("\"result\""));

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
    drop(session_on_disk);

    let initial_requests = seen_requests.lock().unwrap().len();
    assert_eq!(
        initial_requests, 1,
        "provider was called exactly once during turn"
    );

    // 7. 用同一 session 文件创建新 app-server 实例。
    let new_snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("test-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key".to_string()),
            _ => None,
        },
        shared_provider_runtime_handle(),
    );
    let mut new_server =
        AppServer::new(new_snapshot, &sessions_dir).with_test_provider(Arc::new(ReopenProvider {
            response: ModelTurnResponse::completed("reopen_1", "reopen_response", "done"),
            seen_requests: Arc::clone(&seen_requests),
        }));

    new_server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"initialize","id":10,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        )
        .expect("initialize new server");
    new_server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized new server");

    // 8. reopen via turn/start：收敛残留 turn 并运行新 turn
    let reopen_response = new_server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":11,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"reopen"}}]}}}}"#
        ))
        .expect("turn/start after fail-stop");
    assert!(
        reopen_response
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "reopened turn must complete: {reopen_response:?}"
    );

    // 9. 重开不重放旧 turn 的 provider 调用；新 turn 恰好调用一次
    assert_eq!(
        seen_requests.lock().unwrap().len(),
        initial_requests + 1,
        "reopen must not re-execute the old turn's provider calls"
    );
}

#[test]
fn turn_start_runs_on_streaming_lane_without_initialized_notification() {
    // 1a 回归：就绪点前移到 initialize 请求处理完成。客户端收到 initialize
    // 回执后（不等待、也不发送 initialized）立即发 turn/start，请求直接走
    // turn lane 流式路径并开始执行。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let session_id = "aa1b2c3d-e5f6-4a7b-8c9d-0e1f2a3b4c5d";
    let session = SessionManager::create_with_id(&workspace, &sessions_dir, session_id)
        .expect("create session");
    drop(session);

    let (provider_started_tx, provider_started_rx) = std_mpsc::sync_channel(1);
    let (provider_release_tx, provider_release_rx) = std_mpsc::sync_channel(1);
    let provider = Arc::new(TestProvider {
        response: ModelTurnResponse::completed("request_1", "response_1", "all done"),
        seen_requests: Arc::new(Mutex::new(Vec::new())),
        started: provider_started_tx,
        release: Mutex::new(provider_release_rx),
    });

    let server_provider = Arc::clone(&provider);
    let sessions_dir_inside = sessions_dir.clone();
    let result = block_on(async move {
        let handle = tokio::runtime::Handle::current();
        let snapshot = ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("test-model".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            handle,
        );
        let server = AppServer::new(snapshot, &sessions_dir_inside)
            .with_test_provider(server_provider as Arc<dyn Provider + Send + Sync>);

        let (server_stdin, mut client_stdin) = tokio::io::duplex(65536);
        let (client_stdout, server_stdout) = tokio::io::duplex(65536);
        let mut reader = tokio::io::BufReader::new(server_stdin);
        let mut client_reader = tokio::io::BufReader::new(client_stdout);
        let server_task =
            tokio::spawn(
                async move { run_server_with_io(server, &mut reader, server_stdout).await },
            );

        // 1. Send initialize and read its response.
        client_stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"id\":1,\"params\":{\"clientInfo\":{\"name\":\"test\",\"title\":\"Test\",\"version\":\"0.1.0\"}}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        client_reader
            .read_line(&mut line)
            .await
            .expect("read initialize response");
        assert!(line.contains("\"result\""), "{line}");

        // 2. Immediately send turn/start — no initialized notification, no sleep.
        let turn_start_line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"turn/start\",\"id\":2,\"params\":{{\"threadId\":\"{session_id}\",\"input\":[{{\"type\":\"text\",\"text\":\"run task\"}}]}}}}\n"
        );
        client_stdin
            .write_all(turn_start_line.as_bytes())
            .await
            .expect("write turn/start");

        // 3. The provider being called proves the turn lane accepted the request.
        tokio::task::spawn_blocking(move || provider_started_rx.recv())
            .await
            .expect("join provider start wait")
            .expect("provider started without initialized notification");
        let mut running_line = String::new();
        let mut saw_started_notification = false;
        loop {
            running_line.clear();
            client_reader
                .read_line(&mut running_line)
                .await
                .expect("read streaming frames");
            assert!(!running_line.is_empty(), "connection ended early");
            let value: Value = serde_json::from_str(&running_line).expect("JSONL frame");
            if value["method"] == "turn/started" {
                saw_started_notification = true;
            }
            if value["id"] == 2 && value["result"].is_object() {
                break;
            }
        }
        assert!(
            saw_started_notification,
            "turn/started must precede running response"
        );
        assert!(
            running_line.contains("\"status\":\"running\""),
            "running response: {running_line}"
        );

        // 4. Release the provider and drain to the terminal event.
        provider_release_tx.send(()).expect("release provider");
        loop {
            running_line.clear();
            client_reader
                .read_line(&mut running_line)
                .await
                .expect("read terminal frames");
            assert!(
                !running_line.is_empty(),
                "connection ended before turn/completed"
            );
            let value: Value = serde_json::from_str(&running_line).expect("JSONL frame");
            if value["method"] == "turn/completed" {
                assert_eq!(value["params"]["turn"]["status"], "completed");
                break;
            }
        }
        drop(client_stdin);
        let server_result = server_task.await.expect("join server task");
        (saw_started_notification, server_result)
    });

    let (saw_started, server_result) = result;
    assert!(saw_started);
    assert!(
        server_result.is_ok(),
        "supervisor must exit cleanly after stdin EOF: {server_result:?}"
    );
    // 重启后按 JSONL 投影看到 completed 终态。
    let reopened = singularity_runtime::read_thread_summary(&sessions_dir, session_id)
        .expect("project session");
    assert_eq!(
        reopened.status,
        Some(singularity_runtime::ThreadStatus::Completed)
    );
    let session_on_disk =
        SessionManager::open_existing(&sessions_dir.join(format!("{session_id}.jsonl")))
            .expect("open session file");
    assert_eq!(session_on_disk.metadata_entries().len(), 3);
}
