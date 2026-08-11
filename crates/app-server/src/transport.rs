//! `AppServer` 的标准输入输出（stdio）传输层。
//!
//! 输入由 Tokio 单一 owner 读取；请求工作和传输队列均有界，由单一异步写入方串行化
//! JSON 行输出。state/gap 事件可靠阻塞，progress 事件可丢弃但必须记录 gap，控制响应保持独立。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cap_fs_ext::{FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde_json::Value;
use singularity_app_server::{
    AppServer, AppServerCancellationHandle, AppServerError, AppServerOutput,
    OutputOrderCoordinator, TransportTraceBinding,
};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR, Timestamp};
use singularity_model::{PROVIDER_CAPABILITY_CACHE_FILE_NAME, ProviderConfigSnapshot};
use singularity_protocol::{
    EventClass, EventDelivery, EventGap, EventGapReason, EventMetadata, JsonRpcBatchItem,
    JsonRpcId, JsonRpcMessage, JsonRpcPayload, Method, TraceEvent, TraceMetricSample,
    TraceMetricSampleKind, parse_json_rpc_payload,
};
use singularity_store::SessionStore;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const MAX_REQUEST_WORKERS: usize = 16;
const CONTROL_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 256;
const FILE_BACKED_STORE_REQUIRED: &str =
    "app-server requires a file-backed SINGULARITY_APP_SERVER_DB";
const SAFE_FILE_BACKED_STATE_REQUIRED: &str =
    "app-server requires a canonical regular file-backed state database";
const CACHE_TEMP_FILE_PREFIX: &str = ".provider-capability-cache.json.tmp-";
const CACHE_KEY_LOCK_FILE_PREFIX: &str = ".provider-capability-cache.key-lock-";

#[derive(Clone)]
struct OutputChannels {
    control: mpsc::Sender<QueuedOutput>,
    event: mpsc::Sender<QueuedOutput>,
    pending_event_gap: Arc<Mutex<Option<PendingEventGap>>>,
    send_lock: Arc<Mutex<()>>,
    order_state: OutputOrderCoordinator,
    trace_sink: Option<TransportTraceSink>,
}

#[derive(Clone)]
struct TransportTraceSink {
    store: Arc<Mutex<SessionStore>>,
    trace_session_id: uuid::Uuid,
}

impl TransportTraceSink {
    fn new(store: SessionStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            trace_session_id: uuid::Uuid::new_v4(),
        }
    }

    fn append(
        &self,
        binding: &TransportTraceBinding,
        event_id: String,
        summary: &str,
        kind: TraceMetricSampleKind,
    ) -> Result<(), String> {
        let mut event = TraceEvent::for_turn(
            format!("{event_id}_{}", self.trace_session_id.simple()),
            binding.thread_id.clone(),
            binding.turn_id.clone(),
            "transport",
            summary,
        );
        event.timestamp = Some(Timestamp::now_utc().to_string());
        event.payload = serde_json::json!({"observation": "stdio_transport"});
        event.metric_samples = vec![TraceMetricSample { kind, count: 1 }];
        self.store
            .lock()
            .map_err(|_| "transport trace store poisoned".to_string())?
            .append_trace(&event)
            .map_err(|error| format!("transport trace persistence failed: {error}"))
    }
}

#[derive(Debug)]
struct QueuedOutput {
    order: u64,
    to_order: u64,
    message: Value,
    trace_binding: Option<TransportTraceBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEventGap {
    from_cursor: u64,
    to_cursor: u64,
    from_order: u64,
    to_order: u64,
    trace_binding: Option<TransportTraceBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Control,
    Event,
    ReliableEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputSendStatus {
    Enqueued,
    EventDropped,
}

trait ExecutionStop: Send + Sync {
    fn request_execution_stop(&self);
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }
}

/// 在单一 Tokio runtime 内运行 stdio 控制面；所有同步 AppServer 工作都跨 blocking 边界。
pub(super) async fn run(runtime_handle: tokio::runtime::Handle) -> Result<(), String> {
    let configured_db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    let server = tokio::task::spawn_blocking(move || {
        initialize_app_server(&configured_db_path, runtime_handle)
    })
    .await
    .map_err(|error| format!("app-server startup task failed: {error}"))??;
    let cancellation = server.cancellation_handle();
    let trace_sink = TransportTraceSink::new(
        server
            .transport_trace_store()
            .map_err(|error| format!("failed to open transport trace store: {error}"))?,
    );
    let (control_tx, mut control_rx) = mpsc::channel::<QueuedOutput>(CONTROL_QUEUE_CAPACITY);
    let (event_tx, mut event_rx) = mpsc::channel::<QueuedOutput>(EVENT_QUEUE_CAPACITY);
    let outputs = OutputChannels {
        control: control_tx,
        event: event_tx,
        pending_event_gap: Arc::new(Mutex::new(None)),
        send_lock: Arc::new(Mutex::new(())),
        order_state: server.output_order_coordinator(),
        trace_sink: Some(trace_sink),
    };
    let writer_cancellation = cancellation.clone();
    let writer_order_state = outputs.order_state.clone();
    let writer_trace_sink = outputs.trace_sink.clone();
    let mut writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        write_output_queue(
            &mut control_rx,
            &mut event_rx,
            &mut stdout,
            &writer_cancellation,
            writer_order_state,
            writer_trace_sink,
        )
        .await
    });
    let mut writer_done = false;
    let mut writer_result = None;
    let mut writer_timeout = false;
    let mut server = Some(server);
    let mut workers = JoinSet::<Result<(), String>>::new();
    let mut active_workers = 0usize;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut terminal_error = None;

    loop {
        tokio::select! {
            biased;
            result = &mut writer, if !writer_done => {
                writer_done = true;
                writer_result = Some(result);
                terminal_error = Some(match writer_result.as_ref().expect("writer result") {
                    Ok(Ok(())) => "stdout writer stopped unexpectedly".to_string(),
                    Ok(Err(error)) => error.clone(),
                    Err(error) => format!("stdout writer task failed: {error}"),
                });
                break;
            }
            result = workers.join_next(), if active_workers > 0 => {
                active_workers = active_workers.saturating_sub(1);
                match result {
                    Some(Ok(Ok(()))) | None => {}
                    Some(Ok(Err(error))) => {
                        terminal_error = Some(error);
                        break;
                    }
                    Some(Err(error)) => {
                        terminal_error = Some(format!("request worker task failed: {error}"));
                        break;
                    }
                }
            }
            line = lines.next_line() => {
                let Some(line) = (match line {
                    Ok(line) => line,
                    Err(error) => {
                        terminal_error = Some(format!("failed to read stdin: {error}"));
                        break;
                    }
                }) else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                let payload = match parse_json_rpc_payload(&line) {
                    Ok(payload) => payload,
                    Err(_) => {
                        if let Err(error) = send_output_async(
                            outputs.clone(),
                            cancellation.clone(),
                            JsonRpcMessage::parse_error().to_wire_value(),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                };
                if !matches!(payload, JsonRpcPayload::Single(_)) {
                    let batch_outputs = outputs.clone();
                    let batch_cancellation = cancellation.clone();
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let mut server = current_server;
                        let result = dispatch_batch(
                            &mut server,
                            payload,
                            &batch_outputs,
                            &batch_cancellation,
                        );
                        (server, result)
                    });
                    let (next_server, result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("batch dispatch task failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    if let Err(error) = result {
                        terminal_error = Some(error);
                        break;
                    }
                    if server.as_ref().expect("stdio server owner").shutdown_requested() {
                        break;
                    }
                    continue;
                }
                let JsonRpcPayload::Single(item) = payload else {
                    unreachable!("non-single JSON-RPC payload reached single dispatcher")
                };
                let message = match item {
                    JsonRpcBatchItem::Message(message) => message,
                    JsonRpcBatchItem::Invalid { id } => {
                        if let Err(error) = send_output_async(
                            outputs.clone(),
                            cancellation.clone(),
                            JsonRpcMessage::invalid_request(id).to_wire_value(),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                };
                let request_id = message.id().cloned();
                if is_request_worker_method(&message)
                    && server
                        .as_ref()
                        .expect("stdio server owner")
                        .ready_for_turn_worker()
                {
                    if active_workers >= MAX_REQUEST_WORKERS {
                        if let Err(error) = send_output_async(
                            outputs.clone(),
                            cancellation.clone(),
                            request_capacity_error_value(request_id),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let worker = current_server.turn_worker();
                        (current_server, worker)
                    });
                    let (next_server, worker_result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("request worker setup failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    match worker_result {
                        Ok(worker) => {
                            let worker_outputs = outputs.clone();
                            let worker_cancellation = cancellation.clone();
                            workers.spawn_blocking(move || {
                                run_request_worker(
                                    worker,
                                    message,
                                    worker_outputs,
                                    worker_cancellation,
                                )
                            });
                            active_workers += 1;
                        }
                        Err(error) => {
                            if let Err(error) = send_output_async(
                                outputs.clone(),
                                cancellation.clone(),
                                transport_error_value(request_id, &error),
                            )
                            .await
                            {
                                terminal_error = Some(error);
                                break;
                            }
                        }
                    }
                } else {
                    let direct_outputs = outputs.clone();
                    let direct_cancellation = cancellation.clone();
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let mut server = current_server;
                        let notification = message.is_notification();
                        let request_id = message.id().cloned();
                        let result = server.handle_with_output(message);
                        let dispatch_result = match result {
                            Ok(messages) => send_app_server_outputs(
                                &direct_outputs,
                                &direct_cancellation,
                                messages,
                            ),
                            Err(error) if !notification => send_output(
                                &direct_outputs,
                                OutputKind::Control,
                                &direct_cancellation,
                                transport_error_value(request_id, &error),
                            )
                            .map(|_| ()),
                            Err(_) => Ok(()),
                        };
                        (server, dispatch_result)
                    });
                    let (next_server, result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("request dispatch task failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    if let Err(error) = result {
                        terminal_error = Some(error);
                        break;
                    }
                }
                if server.as_ref().expect("stdio server owner").shutdown_requested() {
                    break;
                }
            }
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    let current_server = server.take().expect("stdio server owner");
    let stop_result = tokio::task::spawn_blocking(move || current_server.request_execution_stop())
        .await
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        .and_then(|result| {
            result.map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        });
    let mut worker_error = None;
    while active_workers > 0 {
        let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) else {
            worker_error = Some(format!(
                "timed out waiting for {active_workers} request worker(s) during shutdown"
            ));
            workers.abort_all();
            break;
        };
        match tokio::time::timeout(remaining, workers.join_next()).await {
            Ok(Some(Ok(Ok(())))) => active_workers = active_workers.saturating_sub(1),
            Ok(Some(Ok(Err(error)))) => {
                active_workers = active_workers.saturating_sub(1);
                if worker_error.is_none() {
                    worker_error = Some(error);
                }
            }
            Ok(Some(Err(error))) => {
                active_workers = active_workers.saturating_sub(1);
                if worker_error.is_none() {
                    worker_error = Some(format!("request worker task failed: {error}"));
                }
            }
            Ok(None) => active_workers = 0,
            Err(_) => {
                worker_error = Some(format!(
                    "timed out waiting for {active_workers} request worker(s) during shutdown"
                ));
                workers.abort_all();
                break;
            }
        }
    }

    let gap_result =
        if let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) {
            let gap_outputs = outputs.clone();
            let gap_cancellation = cancellation.clone();
            let mut gap_task = tokio::task::spawn_blocking(move || {
                flush_pending_event_gap(&gap_outputs, &gap_cancellation)
            });
            match tokio::time::timeout(remaining, &mut gap_task).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(format!("event gap flush task failed: {error}")),
                Err(_) => {
                    gap_task.abort();
                    Err("timed out flushing pending event gap during shutdown".to_string())
                }
            }
        } else {
            Err("timed out before flushing pending event gap during shutdown".to_string())
        };
    drop(outputs);

    if !writer_done {
        if let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, &mut writer).await {
                Ok(result) => {
                    writer_result = Some(result);
                }
                Err(_) => {
                    writer.abort();
                    writer_timeout = true;
                }
            }
        } else {
            writer.abort();
            writer_timeout = true;
        }
    }

    let mut errors = Vec::new();
    if let Err(error) = stop_result {
        errors.push(error);
    }
    if let Some(error) = worker_error {
        errors.push(error);
    }
    if let Some(error) = terminal_error {
        errors.push(error);
    }
    if let Err(error) = gap_result {
        errors.push(error);
    }
    if writer_timeout {
        errors.push("timed out waiting for stdout writer during shutdown".to_string());
    }
    if let Some(Err(error)) = writer_result {
        errors.push(format!("stdout writer task failed: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn initialize_app_server(
    configured_db_path: &str,
    runtime_handle: tokio::runtime::Handle,
) -> Result<AppServer, String> {
    let (db_path, capability_cache_path) = prepare_app_server_state_paths(configured_db_path)?;
    let store = SessionStore::open(&db_path)
        .map_err(|error| format!("failed to open app-server store {db_path}: {error}"))?;
    validate_database_file(Path::new(&db_path), false)?;
    store
        .recover_unowned_workspace_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture(
        |name| std::env::var(name).ok(),
        Some(runtime_handle),
        Some(capability_cache_path),
    );
    Ok(AppServer::new(store, provider_snapshot))
}

async fn send_output_async(
    outputs: OutputChannels,
    cancellation: AppServerCancellationHandle,
    message: Value,
) -> Result<OutputSendStatus, String> {
    tokio::task::spawn_blocking(move || {
        send_output(&outputs, OutputKind::Control, &cancellation, message)
    })
    .await
    .map_err(|error| format!("output dispatch task failed: {error}"))?
}

fn is_request_worker_method(message: &JsonRpcMessage) -> bool {
    !message.is_notification() && is_long_worker_method(message)
}

/// Methods whose execution can outlive a normal JSON-RPC dispatch turn.
///
/// Batch dispatch has no streaming worker path, so these methods must be
/// admitted only as single requests. Notifications are included here because
/// their lack of an id does not make the underlying execution short-lived.
fn is_long_worker_method(message: &JsonRpcMessage) -> bool {
    matches!(
        message.method_name(),
        Some(method)
            if method == Method::TurnStart.as_str()
                || method == Method::TurnResume.as_str()
                || method == Method::ApprovalDecision.as_str()
    )
}

/// 按输入顺序串行分发 batch；副作用项不并行，notification 项不产生控制响应。
fn dispatch_batch(
    server: &mut AppServer,
    payload: JsonRpcPayload,
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    let items = match payload {
        JsonRpcPayload::EmptyBatch => {
            return send_output(
                outputs,
                OutputKind::Control,
                cancellation,
                JsonRpcMessage::invalid_request(None).to_wire_value(),
            )
            .map(|_| ());
        }
        JsonRpcPayload::Batch(items) => items,
        JsonRpcPayload::Single(_) => {
            return Err("single JSON-RPC payload reached batch dispatcher".to_string());
        }
    };
    let mut notifications = Vec::<AppServerOutput>::new();
    let mut responses = Vec::new();
    for item in items {
        match item {
            JsonRpcBatchItem::Invalid { id } => {
                responses.push(JsonRpcMessage::invalid_request(id).to_wire_value());
            }
            JsonRpcBatchItem::Message(message) => {
                let notification = message.is_notification();
                let request_id = message.id().cloned();
                if is_long_worker_method(&message) {
                    if !notification {
                        responses.push(JsonRpcMessage::invalid_request(request_id).to_wire_value());
                    }
                    continue;
                }
                match server.handle_with_output(message) {
                    Ok(messages) => {
                        for output in messages {
                            match serde_json::from_value::<JsonRpcMessage>(output.message.clone()) {
                                Ok(JsonRpcMessage::Notification(_)) if !notification => {
                                    notifications.push(output);
                                }
                                Ok(JsonRpcMessage::Success(_) | JsonRpcMessage::Error(_))
                                    if !notification =>
                                {
                                    responses.push(output.message);
                                    server
                                        .output_order_coordinator()
                                        .complete(output.reservation.order);
                                }
                                Ok(_) if notification => {
                                    server
                                        .output_order_coordinator()
                                        .complete(output.reservation.order);
                                }
                                Ok(_) | Err(_) => {
                                    server
                                        .output_order_coordinator()
                                        .complete(output.reservation.order);
                                    responses.push(internal_error_value(
                                        request_id.clone(),
                                        "dispatcher produced an invalid response envelope",
                                    ));
                                }
                            }
                        }
                    }
                    Err(error) if !notification => {
                        responses.push(transport_error_value(request_id, &error));
                    }
                    Err(_) => {}
                }
            }
        }
    }
    send_app_server_outputs(outputs, cancellation, notifications)?;
    if responses.is_empty() {
        return Ok(());
    }
    send_output(
        outputs,
        OutputKind::Control,
        cancellation,
        Value::Array(responses),
    )
    .map(|_| ())
}

/// 分发一个由工作线程负责的请求，并将事件与最终控制响应分别排队。
fn run_request_worker(
    mut worker: AppServer,
    message: JsonRpcMessage,
    outputs: OutputChannels,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let request_id = message.id().cloned();
    let mut output_error = None;
    let result =
        if is_long_worker_method(&message) {
            let mut send_streaming_output = |output: AppServerOutput| {
                if output_error.is_none()
                    && let Err(error) = send_reserved_output(
                        &outputs,
                        &cancellation,
                        output.reservation.order,
                        output.message,
                        output.trace_binding,
                    )
                {
                    output_error = Some(error);
                } else if output_error.is_some() {
                    outputs.order_state.complete(output.reservation.order);
                }
            };
            match message.method_name() {
                Some(method) if method == Method::TurnStart.as_str() => worker
                    .handle_turn_start_streaming_with_output(message, &mut send_streaming_output),
                Some(method) if method == Method::TurnResume.as_str() => worker
                    .handle_turn_resume_streaming_with_output(message, &mut send_streaming_output),
                _ => worker.handle_approval_decision_streaming_with_output(
                    message,
                    &mut send_streaming_output,
                ),
            }
        } else {
            match worker.handle_with_output(message) {
                Ok(messages) => send_app_server_outputs(&outputs, &cancellation, messages)
                    .map_err(AppServerError::Workspace),
                Err(error) => Err(error),
            }
        };
    if let Some(error) = output_error {
        return Err(error);
    }
    if let Err(error) = result {
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            request_error_value(request_id, &error),
        )?;
    }
    Ok(())
}

fn classify_output(value: &Value) -> OutputKind {
    match serde_json::from_value::<JsonRpcMessage>(value.clone()) {
        Ok(JsonRpcMessage::Notification(notification)) => match notification.params.get("event") {
            Some(metadata) => match serde_json::from_value::<EventMetadata>(metadata.clone()) {
                Ok(metadata)
                    if metadata.class == EventClass::Progress
                        && metadata.delivery == EventDelivery::BestEffort =>
                {
                    OutputKind::Event
                }
                Ok(_) | Err(_) => OutputKind::ReliableEvent,
            },
            None => OutputKind::ReliableEvent,
        },
        Ok(JsonRpcMessage::Request(_))
        | Ok(JsonRpcMessage::Success(_))
        | Ok(JsonRpcMessage::Error(_))
        | Err(_) => OutputKind::Control,
    }
}

/// 按事件交付合同入队；控制与 state/gap 阻塞 backpressure，progress 可丢弃但记录 gap。
///
/// channel send 只承诺 frame 已进入有界队列；`ready` 由唯一 writer 在接收 frame 时提交，
/// 从而避免 sender 唤醒 writer 后才提交 ready 的竞态。
fn send_output(
    outputs: &OutputChannels,
    _kind: OutputKind,
    cancellation: &dyn ExecutionStop,
    message: Value,
) -> Result<OutputSendStatus, String> {
    let order = outputs
        .order_state
        .reserve(false)
        .map_err(|error| error.to_string())?
        .order;
    send_reserved_output(outputs, cancellation, order, message, None)
}

fn send_app_server_outputs(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
    messages: Vec<AppServerOutput>,
) -> Result<(), String> {
    for (index, output) in messages.iter().enumerate() {
        if let Err(error) = send_reserved_output(
            outputs,
            cancellation,
            output.reservation.order,
            output.message.clone(),
            output.trace_binding.clone(),
        ) {
            for remaining in &messages[index + 1..] {
                outputs.order_state.complete(remaining.reservation.order);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn send_reserved_output(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
    order: u64,
    message: Value,
    trace_binding: Option<TransportTraceBinding>,
) -> Result<OutputSendStatus, String> {
    let kind = classify_output(&message);
    if kind == OutputKind::Event {
        loop {
            let (pending_gap, send_result) = {
                let _send_guard = match outputs.send_lock.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        outputs.order_state.complete(order);
                        return Err("output ordering state poisoned".to_string());
                    }
                };
                let pending_gap = match prepare_pending_event_gap(outputs, cancellation) {
                    Ok(pending_gap) => pending_gap,
                    Err(error) => {
                        outputs.order_state.complete(order);
                        return Err(error);
                    }
                };
                let send_result = pending_gap.is_none().then(|| {
                    try_send_event(
                        outputs,
                        cancellation,
                        order,
                        message.clone(),
                        trace_binding.clone(),
                    )
                });
                (pending_gap, send_result)
            };
            if let Some(send_result) = send_result {
                let status = send_result?;
                if status == OutputSendStatus::EventDropped {
                    append_transport_metric(
                        outputs,
                        cancellation,
                        trace_binding.as_ref(),
                        format!("trace_transport_drop_{order}"),
                        "best-effort event queue drop",
                        TraceMetricSampleKind::EventQueueDrop,
                        true,
                    )?;
                }
                return Ok(status);
            }
            if let Some((pending, gap_message)) = pending_gap
                && let Err(error) =
                    enqueue_pending_event_gap(outputs, cancellation, pending, gap_message)
            {
                outputs.order_state.complete(order);
                return Err(error);
            }
        }
    }
    if let Err(error) = flush_pending_event_gap(outputs, cancellation) {
        outputs.order_state.complete(order);
        return Err(error);
    }
    match kind {
        OutputKind::Control => outputs
            .control
            .blocking_send(QueuedOutput {
                order,
                to_order: order,
                message,
                trace_binding,
            })
            .map(|()| OutputSendStatus::Enqueued)
            .map_err(|_| {
                outputs.order_state.complete(order);
                cancellation.request_execution_stop();
                "stdout transport unavailable".to_string()
            }),
        OutputKind::ReliableEvent => outputs
            .event
            .blocking_send(QueuedOutput {
                order,
                to_order: order,
                message,
                trace_binding,
            })
            .map(|()| OutputSendStatus::Enqueued)
            .map_err(|_| {
                outputs.order_state.complete(order);
                cancellation.request_execution_stop();
                "stdout transport unavailable".to_string()
            }),
        OutputKind::Event => unreachable!("progress events are sent under the ordering lock"),
    }
}

/// `try_send` 与 dropped-progress gap 的提交共享短排序临界区，但不执行阻塞 I/O。
fn try_send_event(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
    order: u64,
    message: Value,
    trace_binding: Option<TransportTraceBinding>,
) -> Result<OutputSendStatus, String> {
    match outputs.event.try_send(QueuedOutput {
        order,
        to_order: order,
        message,
        trace_binding,
    }) {
        Ok(()) => Ok(OutputSendStatus::Enqueued),
        Err(mpsc::error::TrySendError::Full(output)) => {
            record_event_gap(outputs, &output.message, order, output.trace_binding).map_or_else(
                |error| {
                    outputs.order_state.complete(order);
                    Err(error)
                },
                |()| Ok(OutputSendStatus::EventDropped),
            )
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            outputs.order_state.complete(order);
            cancellation.request_execution_stop();
            Err("stdout transport unavailable".to_string())
        }
    }
}

/// 仅在发送锁内取得 pending gap；实际 channel send 在锁外完成。
fn prepare_pending_event_gap(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
) -> Result<Option<(PendingEventGap, Value)>, String> {
    let mut pending_guard = outputs
        .pending_event_gap
        .lock()
        .map_err(|_| "event gap state poisoned".to_string())?;
    let Some(pending) = pending_guard.as_ref().cloned() else {
        return Ok(None);
    };
    let gap = EventGap {
        reason: EventGapReason::ProgressDropped,
        from_cursor: pending.from_cursor,
        to_cursor: pending.to_cursor,
    };
    let metadata = EventMetadata {
        sequence: pending.to_cursor,
        cursor: pending.to_cursor,
        class: EventClass::Gap,
        delivery: EventDelivery::Gap,
        recovery_query: None,
        gap: Some(gap.clone()),
    };
    let message = match JsonRpcMessage::notification(
        "event/gap",
        serde_json::json!({
            "gap": gap,
            "event": metadata,
        }),
    ) {
        Ok(message) => message.to_wire_value(),
        Err(_) => {
            pending_guard.take();
            drop(pending_guard);
            outputs
                .order_state
                .complete_range(pending.from_order, pending.to_order);
            cancellation.request_execution_stop();
            return Err("failed to serialize event gap".to_string());
        }
    };
    pending_guard.take();
    Ok(Some((pending, message)))
}

/// 将已取得的 gap 放入控制队列，并在 transport 失败时完成其 reservation 范围。
fn enqueue_pending_event_gap(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
    pending: PendingEventGap,
    message: Value,
) -> Result<(), String> {
    match outputs.control.blocking_send(QueuedOutput {
        order: pending.from_order,
        to_order: pending.to_order,
        message,
        trace_binding: pending.trace_binding.clone(),
    }) {
        Ok(()) => {}
        Err(_) => {
            outputs
                .order_state
                .complete_range(pending.from_order, pending.to_order);
            cancellation.request_execution_stop();
            return Err("stdout transport unavailable".to_string());
        }
    }
    append_transport_metric(
        outputs,
        cancellation,
        pending.trace_binding.as_ref(),
        format!(
            "trace_transport_gap_{}_{}",
            pending.from_order, pending.to_order
        ),
        "event gap enqueued",
        TraceMetricSampleKind::EventGap,
        true,
    )
}

/// 将待发送 gap 放入控制队列，保证它先于同一请求的匹配响应写出。
fn flush_pending_event_gap(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    let pending_gap = {
        let _send_guard = outputs
            .send_lock
            .lock()
            .map_err(|_| "output ordering state poisoned".to_string())?;
        prepare_pending_event_gap(outputs, cancellation)?
    };
    if let Some((pending, message)) = pending_gap {
        enqueue_pending_event_gap(outputs, cancellation, pending, message)?;
    }
    Ok(())
}

/// 合并连续丢弃的 progress cursor；state 事件不允许走此路径。
fn record_event_gap(
    outputs: &OutputChannels,
    message: &Value,
    order: u64,
    trace_binding: Option<TransportTraceBinding>,
) -> Result<(), String> {
    let metadata = message
        .get("params")
        .and_then(|params| params.get("event"))
        .cloned()
        .ok_or_else(|| "progress event is missing typed delivery metadata".to_string())
        .and_then(|value| {
            serde_json::from_value::<EventMetadata>(value)
                .map_err(|_| "progress event has invalid delivery metadata".to_string())
        })?;
    if metadata.sequence == 0
        || metadata.cursor != metadata.sequence
        || metadata.class != EventClass::Progress
        || metadata.delivery != EventDelivery::BestEffort
    {
        return Err("only typed best-effort progress events may be dropped".to_string());
    }
    let mut pending = outputs
        .pending_event_gap
        .lock()
        .map_err(|_| "event gap state poisoned".to_string())?;
    match pending.as_mut() {
        None => {
            *pending = Some(PendingEventGap {
                from_cursor: metadata.cursor,
                to_cursor: metadata.cursor,
                from_order: order,
                to_order: order,
                trace_binding: trace_binding.clone(),
            });
        }
        Some(pending)
            if metadata.cursor == pending.to_cursor.saturating_add(1)
                && order == pending.to_order.saturating_add(1)
                && pending.trace_binding == trace_binding =>
        {
            pending.to_cursor = metadata.cursor;
            pending.to_order = order;
        }
        Some(_) => return Err("non-contiguous dropped progress cursor".to_string()),
    }
    Ok(())
}

fn append_transport_metric(
    outputs: &OutputChannels,
    cancellation: &dyn ExecutionStop,
    binding: Option<&TransportTraceBinding>,
    event_id: String,
    summary: &str,
    kind: TraceMetricSampleKind,
    binding_required: bool,
) -> Result<(), String> {
    let Some(trace_sink) = outputs.trace_sink.as_ref() else {
        return Ok(());
    };
    let Some(binding) = binding else {
        if binding_required {
            cancellation.request_execution_stop();
            return Err("turn-bound transport observation is missing trace binding".to_string());
        }
        return Ok(());
    };
    trace_sink
        .append(binding, event_id, summary, kind)
        .inspect_err(|_| {
            cancellation.request_execution_stop();
        })
}

/// 将 writer 接收的 frame 与唯一顺序状态提交到同一个 owner 内的短临界步骤。
fn accept_output(
    order_state: &OutputOrderCoordinator,
    pending: &mut BTreeMap<u64, QueuedOutput>,
    message: QueuedOutput,
) {
    let order = message.order;
    let to_order = message.to_order;
    pending.insert(order, message);
    if order == to_order {
        order_state.enqueue(order);
    } else {
        order_state.enqueue_range(order, to_order);
    }
}

/// 按发送顺序从两个 Tokio 有界队列取出一条 frame，避免 progress 被后续 control 越过。
///
/// 接收和 ready 提交之间不发生 await；writer 随后才检查 readiness，因此唯一 frame 不会
/// 因 sender 尚未完成旧式的 post-send enqueue 而永久滞留。
async fn next_output(
    control_rx: &mut mpsc::Receiver<QueuedOutput>,
    event_rx: &mut mpsc::Receiver<QueuedOutput>,
    control_open: &mut bool,
    event_open: &mut bool,
    order_state: &OutputOrderCoordinator,
    pending: &mut BTreeMap<u64, QueuedOutput>,
) -> Option<QueuedOutput> {
    loop {
        if *control_open {
            loop {
                match control_rx.try_recv() {
                    Ok(message) => {
                        accept_output(order_state, pending, message);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        *control_open = false;
                        break;
                    }
                }
            }
        }
        if *event_open {
            loop {
                match event_rx.try_recv() {
                    Ok(message) => {
                        accept_output(order_state, pending, message);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        *event_open = false;
                        break;
                    }
                }
            }
        }
        if let Some(order) = pending.keys().next().copied() {
            if order_state.is_next_ready(order) {
                return pending.remove(&order);
            }
        } else if !*control_open && !*event_open {
            return None;
        }
        if !*control_open && !*event_open {
            return None;
        }
        tokio::select! {
            biased;
            message = control_rx.recv(), if *control_open => match message {
                Some(message) => accept_output(order_state, pending, message),
                None => *control_open = false,
            },
            message = event_rx.recv(), if *event_open => match message {
                Some(message) => accept_output(order_state, pending, message),
                None => *event_open = false,
            },
        }
    }
}

/// 串行写出控制与事件 frame；真实写入或 flush 失败才触发全局停止。
async fn write_output_queue<W: AsyncWrite + Unpin>(
    control_rx: &mut mpsc::Receiver<QueuedOutput>,
    event_rx: &mut mpsc::Receiver<QueuedOutput>,
    stdout: &mut W,
    cancellation: &dyn ExecutionStop,
    order_state: OutputOrderCoordinator,
    trace_sink: Option<TransportTraceSink>,
) -> Result<(), String> {
    let mut control_open = true;
    let mut event_open = true;
    let mut pending = BTreeMap::new();
    while let Some(output) = next_output(
        control_rx,
        event_rx,
        &mut control_open,
        &mut event_open,
        &order_state,
        &mut pending,
    )
    .await
    {
        let line = match serde_json::to_vec(&output.message) {
            Ok(line) => line,
            Err(error) => {
                cancellation.request_execution_stop();
                return Err(format!("failed to serialize response: {error}"));
            }
        };
        if let Err(error) = stdout.write_all(&line).await {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.write_all(b"\n").await {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.flush().await {
            cancellation.request_execution_stop();
            return Err(format!("failed to flush response: {error}"));
        }
        if let (Some(trace_sink), Some(binding)) =
            (trace_sink.clone(), output.trace_binding.clone())
        {
            let order = output.order;
            let persisted = match tokio::task::spawn_blocking(move || {
                trace_sink.append(
                    &binding,
                    format!("trace_transport_writer_{order}"),
                    "stdout frame visible",
                    TraceMetricSampleKind::WriterVisible,
                )
            })
            .await
            {
                Ok(persisted) => persisted,
                Err(error) => {
                    cancellation.request_execution_stop();
                    return Err(format!("transport trace task failed: {error}"));
                }
            };
            if let Err(error) = persisted {
                cancellation.request_execution_stop();
                return Err(error);
            }
        }
        order_state.acknowledge_written(output.order, output.to_order);
    }
    Ok(())
}

fn transport_error_value(id: Option<JsonRpcId>, _error: &AppServerError) -> Value {
    internal_error_value(id, "Internal error")
}

fn request_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    match error {
        AppServerError::InvalidParams(_) => {
            JsonRpcMessage::error(id, ErrorCode::invalid_params("Invalid params")).to_wire_value()
        }
        error => transport_error_value(id, error),
    }
}

fn request_capacity_error_value(id: Option<JsonRpcId>) -> Value {
    JsonRpcMessage::error(id, ErrorCode::request_capacity_exceeded()).to_wire_value()
}

fn internal_error_value(id: Option<JsonRpcId>, _diagnostic: impl Into<String>) -> Value {
    JsonRpcMessage::error(
        id,
        ErrorCode::new(JSON_RPC_INTERNAL_ERROR, "Internal error"),
    )
    .to_wire_value()
}

fn resolve_app_server_state_paths(configured_db_path: &str) -> Result<(String, PathBuf), String> {
    if is_unsupported_sqlite_database_path(configured_db_path) {
        return Err(FILE_BACKED_STORE_REQUIRED.to_string());
    }
    let db_path = configured_db_path.trim();
    let database_name = Path::new(db_path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    validate_database_name(database_name)?;
    let parent = Path::new(db_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let cache_path = parent.join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
    Ok((db_path.to_string(), cache_path))
}

fn is_unsupported_sqlite_database_path(configured_db_path: &str) -> bool {
    let trimmed = configured_db_path.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.eq_ignore_ascii_case(":memory:")
        || lower.starts_with("file:")
        || lower.starts_with("sqlite:")
}

fn prepare_app_server_state_paths(configured_db_path: &str) -> Result<(String, PathBuf), String> {
    let (raw_db_path, _) = resolve_app_server_state_paths(configured_db_path)?;
    let raw_db_path = Path::new(&raw_db_path);
    let raw_parent = raw_db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = prepare_state_directory(raw_parent)?;
    let database_name = raw_db_path
        .file_name()
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let database_path = canonical_parent.join(database_name);
    validate_database_file(&database_path, true)?;
    let cache_path = canonical_parent.join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
    Ok((
        database_path
            .to_str()
            .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?
            .to_string(),
        cache_path,
    ))
}

fn prepare_state_directory(parent: &Path) -> Result<PathBuf, String> {
    validate_existing_state_components(parent)?;
    std::fs::create_dir_all(parent).map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    validate_existing_state_components(parent)?;
    let canonical =
        std::fs::canonicalize(parent).map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let metadata = std::fs::symlink_metadata(&canonical)
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(canonical)
}

fn validate_existing_state_components(parent: &Path) -> Result<(), String> {
    let absolute = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?
            .join(parent)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata_is_reparse(&metadata) {
                    return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string()),
        }
    }
    Ok(())
}

fn validate_database_name(name: &str) -> Result<(), String> {
    let normalized = name
        .to_ascii_lowercase()
        .trim_end_matches([' ', '.'])
        .to_string();
    if normalized.is_empty()
        || normalized == PROVIDER_CAPABILITY_CACHE_FILE_NAME
        || normalized == "provider-capability-cache.lock"
        || normalized.starts_with(CACHE_TEMP_FILE_PREFIX)
        || normalized.starts_with(CACHE_KEY_LOCK_FILE_PREFIX)
    {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    #[cfg(windows)]
    if name.ends_with([' ', '.']) || name.contains('~') {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(())
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateFileIdentity {
    device: u64,
    inode: u64,
    links: u64,
}

fn state_file_identity(metadata: &cap_std::fs::Metadata) -> Result<StateFileIdentity, String> {
    let identity = StateFileIdentity {
        device: CapMetadataExt::dev(metadata),
        inode: CapMetadataExt::ino(metadata),
        links: CapMetadataExt::nlink(metadata),
    };
    (identity.links == 1)
        .then_some(identity)
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())
}

fn open_state_file(path: &Path) -> Result<(std::fs::File, StateFileIdentity), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let name = path
        .file_name()
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let directory = CapabilityDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = directory
        .open_with(name, &options)
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let identity = state_file_identity(
        &file
            .metadata()
            .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?,
    )?;
    Ok((file.into_std(), identity))
}

fn validate_database_file(path: &Path, allow_missing: bool) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(_) => return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    let (file, identity) = open_state_file(path)?;
    let opened = file
        .metadata()
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    if !opened.is_file() || metadata_is_reparse(&opened) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    let (_, reopened_identity) = open_state_file(path)?;
    if identity != reopened_identity {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Barrier};
    use std::task::{Context, Poll};
    use std::thread;
    use tokio::io::AsyncWrite;

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

    fn test_output_channels(
        control_capacity: usize,
        event_capacity: usize,
    ) -> (
        OutputChannels,
        mpsc::Receiver<QueuedOutput>,
        mpsc::Receiver<QueuedOutput>,
    ) {
        test_output_channels_with_trace_sink(control_capacity, event_capacity, None)
    }

    fn test_output_channels_with_trace_sink(
        control_capacity: usize,
        event_capacity: usize,
        trace_sink: Option<TransportTraceSink>,
    ) -> (
        OutputChannels,
        mpsc::Receiver<QueuedOutput>,
        mpsc::Receiver<QueuedOutput>,
    ) {
        let (control, control_rx) = mpsc::channel(control_capacity);
        let (event, event_rx) = mpsc::channel(event_capacity);
        (
            OutputChannels {
                control,
                event,
                pending_event_gap: Arc::new(Mutex::new(None)),
                send_lock: Arc::new(Mutex::new(())),
                order_state: OutputOrderCoordinator::new(),
                trace_sink,
            },
            control_rx,
            event_rx,
        )
    }

    fn transport_trace_fixture() -> (
        tempfile::TempDir,
        SessionStore,
        TransportTraceSink,
        TransportTraceBinding,
    ) {
        let directory = tempfile::tempdir().expect("transport trace directory");
        let store = SessionStore::open(directory.path().join("sessions.sqlite3"))
            .expect("transport trace store");
        let thread = store.create_thread(None, None).expect("trace thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("trace turn");
        let trace_sink = TransportTraceSink::new(
            store
                .trusted_reopen()
                .expect("transport trace store reopen"),
        );
        let binding = TransportTraceBinding::for_turn(thread.thread_id, turn.turn_id);
        (directory, store, trace_sink, binding)
    }

    fn transport_metric_count(
        store: &SessionStore,
        thread_id: &str,
        kind: TraceMetricSampleKind,
    ) -> u64 {
        store
            .list_trace(thread_id)
            .expect("transport trace list")
            .iter()
            .flat_map(|event| event.metric_samples.iter())
            .filter(|sample| sample.kind == kind)
            .map(|sample| sample.count)
            .sum()
    }

    fn progress_event(cursor: u64) -> Value {
        JsonRpcMessage::notification(
            "item/agentMessage/delta",
            serde_json::json!({
                "item": {"item_id": "item_progress"},
                "delta": "progress",
                "event": EventMetadata {
                    sequence: cursor,
                    cursor,
                    class: EventClass::Progress,
                    delivery: EventDelivery::BestEffort,
                    recovery_query: None,
                    gap: None,
                },
            }),
        )
        .expect("progress event")
        .to_wire_value()
    }

    fn reliable_state_event(cursor: u64) -> Value {
        JsonRpcMessage::notification(
            "turn/diff/updated",
            serde_json::json!({
                "turnId": "turn_state",
                "diff": {"files": []},
                "event": EventMetadata {
                    sequence: cursor,
                    cursor,
                    class: EventClass::State,
                    delivery: EventDelivery::Reliable,
                    recovery_query: None,
                    gap: None,
                },
            }),
        )
        .expect("state event")
        .to_wire_value()
    }

    fn reliable_item_event(method: &str, cursor: u64) -> Value {
        JsonRpcMessage::notification(
            method,
            serde_json::json!({
                "item": {"item_id": "item_realtime"},
                "event": EventMetadata {
                    sequence: cursor,
                    cursor,
                    class: EventClass::State,
                    delivery: EventDelivery::Reliable,
                    recovery_query: None,
                    gap: None,
                },
            }),
        )
        .expect("reliable item event")
        .to_wire_value()
    }

    #[test]
    fn state_path_rejects_sqlite_uri_before_cache_injection() {
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
    fn state_path_injects_cache_next_to_file_backed_database() {
        let (db_path, cache_path) =
            resolve_app_server_state_paths("state/rust-app-server.sqlite3").expect("state paths");
        assert_eq!(db_path, "state/rust-app-server.sqlite3");
        assert_eq!(
            cache_path,
            PathBuf::from("state").join(PROVIDER_CAPABILITY_CACHE_FILE_NAME)
        );
    }

    #[test]
    fn prepared_state_paths_use_the_canonical_directory() {
        let directory = tempfile::tempdir().expect("state directory");
        let configured = directory.path().join("nested").join("sessions.sqlite3");
        let (db_path, cache_path) =
            prepare_app_server_state_paths(configured.to_str().expect("configured path"))
                .expect("prepared state paths");
        let canonical_parent = std::fs::canonicalize(configured.parent().expect("parent"))
            .expect("canonical state directory");
        assert_eq!(
            Path::new(&db_path).parent(),
            Some(canonical_parent.as_path())
        );
        assert_eq!(cache_path.parent(), Some(canonical_parent.as_path()));
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
    fn state_path_rejects_cache_lock_and_temp_name_collisions() {
        for name in [
            PROVIDER_CAPABILITY_CACHE_FILE_NAME,
            "provider-capability-cache.lock",
            ".provider-capability-cache.key-lock-00.lock",
            ".provider-capability-cache.json.tmp-owned",
        ] {
            let error = resolve_app_server_state_paths(name)
                .expect_err("reserved cache state name rejected");
            assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
        }
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
    fn control_queue_uses_bounded_backpressure_without_stopping_execution() {
        let (outputs, mut control_rx, _event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            serde_json::json!({"first": true}),
        )
        .expect("first output fits");

        let (attempted_sender, attempted_receiver) = std_mpsc::channel();
        let sender_outputs = outputs.clone();
        let sender_cancellation = cancellation.clone();
        let sender = thread::spawn(move || {
            attempted_sender.send(()).expect("control send attempted");
            send_output(
                &sender_outputs,
                OutputKind::Control,
                &sender_cancellation,
                serde_json::json!({"second": true}),
            )
            .expect("bounded control send");
        });
        attempted_receiver.recv().expect("control send attempt");
        assert!(
            !sender.is_finished(),
            "full control queue must backpressure"
        );
        assert_eq!(cancellation.request_count(), 0);
        assert_eq!(
            control_rx
                .blocking_recv()
                .expect("first control output")
                .message["first"],
            true
        );
        sender.join().expect("control sender");
        assert_eq!(
            control_rx
                .blocking_recv()
                .expect("second control output")
                .message["second"],
            true
        );
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn saturated_event_output_does_not_stop_equivalent_active_turn_handles() {
        let (outputs, _control_rx, _event_rx) = test_output_channels(1, 1);
        let first_turn_cancellation = CancellationProbe::default();
        let second_turn_cancellation = first_turn_cancellation.clone();
        send_output(
            &outputs,
            OutputKind::Event,
            &first_turn_cancellation,
            progress_event(1),
        )
        .expect("first event fits");

        let result = send_output(
            &outputs,
            OutputKind::Event,
            &first_turn_cancellation,
            progress_event(2),
        )
        .expect("event pressure must not stop execution");

        assert_eq!(result, OutputSendStatus::EventDropped);
        assert_eq!(first_turn_cancellation.request_count(), 0);
        assert_eq!(second_turn_cancellation.request_count(), 0);
    }

    #[test]
    fn reliable_state_event_blocks_on_event_queue_without_global_stop() {
        let (outputs, _control_rx, mut event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Event,
            &cancellation,
            progress_event(1),
        )
        .expect("progress event fits");
        let (attempted_sender, attempted_receiver) = std_mpsc::channel();
        let sender_outputs = outputs.clone();
        let sender_cancellation = cancellation.clone();
        let sender = thread::spawn(move || {
            attempted_sender.send(()).expect("state send attempted");
            send_output(
                &sender_outputs,
                OutputKind::ReliableEvent,
                &sender_cancellation,
                reliable_state_event(2),
            )
            .expect("reliable state event");
        });
        attempted_receiver.recv().expect("state send attempt");
        assert!(
            !sender.is_finished(),
            "state event must apply bounded backpressure"
        );
        assert_eq!(cancellation.request_count(), 0);
        assert_eq!(event_rx.blocking_recv().expect("progress output").order, 0);
        sender.join().expect("state sender");
        assert_eq!(event_rx.blocking_recv().expect("state output").order, 1);
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn dropped_realtime_delta_records_gap_without_losing_completed_or_failed() {
        for terminal_method in ["item/completed", "item/failed"] {
            let (outputs, mut control_rx, mut event_rx) = test_output_channels(2, 1);
            let cancellation = CancellationProbe::default();
            assert_eq!(
                send_output(
                    &outputs,
                    OutputKind::Event,
                    &cancellation,
                    progress_event(1),
                )
                .expect("first delta"),
                OutputSendStatus::Enqueued
            );
            assert_eq!(
                send_output(
                    &outputs,
                    OutputKind::Event,
                    &cancellation,
                    progress_event(2),
                )
                .expect("second delta may drop"),
                OutputSendStatus::EventDropped
            );

            let sender_outputs = outputs.clone();
            let sender_cancellation = cancellation.clone();
            let expected_terminal_method = terminal_method;
            let terminal_method = terminal_method.to_string();
            let sender = thread::spawn(move || {
                send_output(
                    &sender_outputs,
                    OutputKind::ReliableEvent,
                    &sender_cancellation,
                    reliable_item_event(&terminal_method, 3),
                )
            });
            let first = event_rx.blocking_recv().expect("queued first delta");
            assert_eq!(first.message["method"], "item/agentMessage/delta");
            assert_eq!(
                sender
                    .join()
                    .expect("terminal sender")
                    .expect("terminal event"),
                OutputSendStatus::Enqueued
            );
            let terminal = event_rx.blocking_recv().expect("reliable terminal event");
            assert_eq!(terminal.message["method"], expected_terminal_method);
            let gap = control_rx.blocking_recv().expect("typed progress gap");
            assert_eq!(gap.message["method"], "event/gap");
            assert_eq!(gap.message["params"]["gap"]["reason"], "progress_dropped");
            assert_eq!(gap.message["params"]["gap"]["fromCursor"], 2);
            assert_eq!(gap.message["params"]["gap"]["toCursor"], 2);
        }
    }

    #[test]
    fn reverse_enqueue_race_keeps_event_cursor_and_stdout_order_contiguous() {
        let (outputs, mut control_rx, mut event_rx) = test_output_channels(2, 2);
        let cancellation = CancellationProbe::default();
        let first = outputs
            .order_state
            .reserve(true)
            .expect("first event reservation");
        let second = outputs
            .order_state
            .reserve(true)
            .expect("second event reservation");
        assert_eq!(first.order, 0);
        assert_eq!(first.event_cursor, Some(1));
        assert_eq!(second.order, 1);
        assert_eq!(second.event_cursor, Some(2));

        let send_gate = Arc::new(Barrier::new(2));
        let sender_outputs = outputs.clone();
        let sender_cancellation = cancellation.clone();
        let sender_gate = Arc::clone(&send_gate);
        let sender = thread::spawn(move || {
            send_reserved_output(
                &sender_outputs,
                &sender_cancellation,
                second.order,
                progress_event(2),
                None,
            )
            .expect("later event queues first");
            sender_gate.wait();
            send_reserved_output(
                &sender_outputs,
                &sender_cancellation,
                first.order,
                progress_event(1),
                None,
            )
            .expect("earlier event queues after the barrier");
        });
        // The channel/barrier makes the reverse enqueue order deterministic: cursor 2 is
        // already queued before cursor 1 is released, so the writer must use reservations.
        send_gate.wait();
        sender.join().expect("reverse enqueue sender");
        let order_state = outputs.order_state.clone();
        drop(outputs);

        let mut stdout = VecWriter::default();
        block_on(write_output_queue(
            &mut control_rx,
            &mut event_rx,
            &mut stdout,
            &cancellation,
            order_state,
            None,
        ))
        .expect("writer drains reserved events");
        let values = String::from_utf8(stdout.0)
            .expect("writer output is UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSONL frame"))
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["params"]["event"]["cursor"], 1);
        assert_eq!(values[1]["params"]["event"]["cursor"], 2);
    }

    #[test]
    fn gap_is_written_before_matching_control_output_when_progress_queue_is_full() {
        let (outputs, mut control_rx, mut event_rx) = test_output_channels(2, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Event,
            &cancellation,
            progress_event(1),
        )
        .expect("event fits");
        send_output(
            &outputs,
            OutputKind::Event,
            &cancellation,
            progress_event(2),
        )
        .expect("event pressure is a recoverable drop");
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            serde_json::json!({"kind": "control"}),
        )
        .expect("control remains available while events are full");
        let order_state = outputs.order_state.clone();
        drop(outputs);

        let mut stdout = VecWriter::default();
        block_on(write_output_queue(
            &mut control_rx,
            &mut event_rx,
            &mut stdout,
            &cancellation,
            order_state,
            None,
        ))
        .expect("writer drains both queues");
        let lines = String::from_utf8(stdout.0).expect("writer output is UTF-8");
        let values = lines
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSONL frame"))
            .collect::<Vec<_>>();
        assert_eq!(values[0]["params"]["event"]["sequence"], 1);
        assert_eq!(values[1]["method"], "event/gap");
        assert_eq!(values[1]["params"]["event"]["sequence"], 2);
        assert_eq!(values[1]["params"]["event"]["gap"]["fromCursor"], 2);
        assert_eq!(values[1]["params"]["event"]["gap"]["toCursor"], 2);
        assert_eq!(values[2]["kind"], "control");
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn transport_drop_gap_and_writer_visibility_use_the_bound_sqlite_trace() {
        let (_directory, store, trace_sink, binding) = transport_trace_fixture();
        let (outputs, mut control_rx, mut event_rx) =
            test_output_channels_with_trace_sink(2, 1, Some(trace_sink.clone()));
        let cancellation = CancellationProbe::default();
        let first = outputs
            .order_state
            .reserve(true)
            .expect("first bound event reservation");
        let second = outputs
            .order_state
            .reserve(true)
            .expect("second bound event reservation");
        assert_eq!(
            send_reserved_output(
                &outputs,
                &cancellation,
                first.order,
                progress_event(first.event_cursor.expect("first cursor")),
                Some(binding.clone()),
            )
            .expect("first bound progress event"),
            OutputSendStatus::Enqueued
        );
        assert_eq!(
            send_reserved_output(
                &outputs,
                &cancellation,
                second.order,
                progress_event(second.event_cursor.expect("second cursor")),
                Some(binding.clone()),
            )
            .expect("second bound progress event drops"),
            OutputSendStatus::EventDropped
        );
        assert_eq!(
            transport_metric_count(
                &store,
                &binding.thread_id,
                TraceMetricSampleKind::EventQueueDrop,
            ),
            1
        );

        let queued = event_rx.blocking_recv().expect("release progress queue");
        assert_eq!(queued.trace_binding.as_ref(), Some(&binding));
        outputs
            .event
            .blocking_send(queued)
            .expect("restore ordered progress output");
        flush_pending_event_gap(&outputs, &cancellation).expect("bound gap flush");
        let gap = control_rx.blocking_recv().expect("bound gap output");
        assert_eq!(gap.trace_binding.as_ref(), Some(&binding));
        outputs
            .control
            .blocking_send(gap)
            .expect("restore ordered gap output");
        assert_eq!(
            transport_metric_count(&store, &binding.thread_id, TraceMetricSampleKind::EventGap,),
            1
        );

        let writer_order_state = outputs.order_state.clone();
        let writer_trace_sink = outputs.trace_sink.clone();
        drop(outputs);
        let mut stdout = VecWriter::default();
        block_on(write_output_queue(
            &mut control_rx,
            &mut event_rx,
            &mut stdout,
            &cancellation,
            writer_order_state,
            writer_trace_sink,
        ))
        .expect("bound writer drains outputs");
        assert_eq!(
            transport_metric_count(
                &store,
                &binding.thread_id,
                TraceMetricSampleKind::WriterVisible,
            ),
            2
        );
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn transport_trace_persistence_failure_stops_execution() {
        let (_directory, _store, trace_sink, binding) = transport_trace_fixture();
        let invalid_binding =
            TransportTraceBinding::for_turn(binding.thread_id, "turn_missing_transport");
        let (outputs, _control_rx, _event_rx) =
            test_output_channels_with_trace_sink(1, 1, Some(trace_sink));
        let cancellation = CancellationProbe::default();
        let first = outputs
            .order_state
            .reserve(true)
            .expect("first event reservation");
        let second = outputs
            .order_state
            .reserve(true)
            .expect("second event reservation");
        send_reserved_output(
            &outputs,
            &cancellation,
            first.order,
            progress_event(first.event_cursor.expect("first cursor")),
            Some(invalid_binding.clone()),
        )
        .expect("first invalid-bound event only enters queue");
        let error = send_reserved_output(
            &outputs,
            &cancellation,
            second.order,
            progress_event(second.event_cursor.expect("second cursor")),
            Some(invalid_binding),
        )
        .expect_err("drop metric persistence must fail closed");
        assert!(error.starts_with("transport trace persistence failed:"));
        assert_eq!(cancellation.request_count(), 1);
    }

    #[test]
    fn transport_trace_ids_remain_unique_when_a_new_process_restarts_output_order() {
        let (_directory, store, first_sink, binding) = transport_trace_fixture();
        let second_sink = TransportTraceSink::new(
            store
                .trusted_reopen()
                .expect("second transport trace store reopen"),
        );

        for sink in [&first_sink, &second_sink] {
            sink.append(
                &binding,
                "trace_transport_writer_1".to_string(),
                "stdout frame visible",
                TraceMetricSampleKind::WriterVisible,
            )
            .expect("restarted output order must not collide");
        }

        assert_eq!(
            transport_metric_count(
                &store,
                &binding.thread_id,
                TraceMetricSampleKind::WriterVisible,
            ),
            2
        );
    }

    #[test]
    fn pending_gap_flush_does_not_hold_send_lock_across_control_backpressure() {
        let (outputs, mut control_rx, _event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            serde_json::json!({"kind": "occupied"}),
        )
        .expect("control queue is filled");
        send_output(
            &outputs,
            OutputKind::Event,
            &cancellation,
            progress_event(1),
        )
        .expect("first progress event fits");
        assert_eq!(
            send_output(
                &outputs,
                OutputKind::Event,
                &cancellation,
                progress_event(2),
            )
            .expect("second progress event is droppable"),
            OutputSendStatus::EventDropped
        );

        let (attempted_sender, attempted_receiver) = std_mpsc::channel();
        let sender_outputs = outputs.clone();
        let sender_cancellation = cancellation.clone();
        let sender = thread::spawn(move || {
            attempted_sender.send(()).expect("gap flush attempted");
            flush_pending_event_gap(&sender_outputs, &sender_cancellation)
        });
        attempted_receiver.recv().expect("gap flush started");

        let deadline = Instant::now() + Duration::from_millis(200);
        let mut lock_available = false;
        while Instant::now() < deadline {
            if let Ok(guard) = outputs.send_lock.try_lock() {
                drop(guard);
                lock_available = true;
                break;
            }
            thread::yield_now();
        }

        control_rx
            .blocking_recv()
            .expect("release occupied control output");
        sender
            .join()
            .expect("gap sender")
            .expect("gap flush succeeds after backpressure is released");
        assert!(
            lock_available,
            "gap flush must release send_lock before blocking on control queue"
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
        let (outputs, mut control_rx, mut event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            serde_json::json!({"kind": "control"}),
        )
        .expect("control fits");
        let order_state = outputs.order_state.clone();
        drop(outputs);

        let mut stdout = DisconnectedWriter;
        let error = block_on(write_output_queue(
            &mut control_rx,
            &mut event_rx,
            &mut stdout,
            &cancellation,
            order_state,
            None,
        ))
        .expect_err("writer disconnect is transport-fatal");

        assert!(error.starts_with("failed to write response:"));
        assert_eq!(cancellation.request_count(), 1);
    }

    #[test]
    fn writer_accepts_a_sole_frame_before_the_readiness_check() {
        let (outputs, mut control_rx, mut event_rx) = test_output_channels(1, 1);
        let reservation = outputs
            .order_state
            .reserve(false)
            .expect("single frame reservation");
        outputs
            .control
            .blocking_send(QueuedOutput {
                order: reservation.order,
                to_order: reservation.order,
                message: serde_json::json!({"only": true}),
                trace_binding: None,
            })
            .expect("single frame enters bounded queue");
        // Deliberately leave the reservation in-flight: this is the state observed when a
        // sender wakes the writer before its old post-send ready update.
        let order_state = outputs.order_state.clone();
        drop(outputs);

        let mut stdout = VecWriter::default();
        let cancellation = CancellationProbe::default();
        block_on(async {
            tokio::time::timeout(
                Duration::from_millis(100),
                write_output_queue(
                    &mut control_rx,
                    &mut event_rx,
                    &mut stdout,
                    &cancellation,
                    order_state,
                    None,
                ),
            )
            .await
        })
        .expect("sole frame must not wait for a second message")
        .expect("writer drains the sole frame");
        assert_eq!(
            String::from_utf8(stdout.0).expect("writer output is UTF-8"),
            "{\"only\":true}\n"
        );
    }

    #[test]
    fn mixed_batch_is_sequential_and_only_requests_produce_ordered_responses() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server =
            AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver, _event_receiver) = test_output_channels(8, 8);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}},
                {"jsonrpc":"2.0","method":"thread/read","params":{}},
                {"jsonrpc":"2.0","method":"unknown","params":{}},
                {"jsonrpc":"2.0","method":"unknown","id":3,"params":{}},
                {"jsonrpc":"2.0","method":"thread/read","id":4,"params":{}}
            ]"#,
        )
        .expect("batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation).expect("dispatch batch");

        let mut outputs = Vec::new();
        while let Ok(output) = receiver.try_recv() {
            outputs.push(output);
        }
        assert_eq!(outputs.len(), 1);
        let responses = outputs[0].message.as_array().expect("batch response array");
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[2]["id"], 3);
        assert_eq!(responses[2]["error"]["code"], -32601);
        assert_eq!(responses[3]["id"], 4);
        assert_eq!(responses[3]["error"]["code"], -32602);
        assert!(
            responses
                .iter()
                .all(|response| response["jsonrpc"] == "2.0")
        );
    }

    #[test]
    fn batch_rejects_long_worker_methods_and_continues_with_short_items() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver, _event_receiver) = test_output_channels(16, 8);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}},
                {"jsonrpc":"2.0","method":"turn/start","id":3,"params":{}},
                {"jsonrpc":"2.0","method":"turn/resume","id":4,"params":{}},
                {"jsonrpc":"2.0","method":"approval/decision","id":5,"params":{}},
                {"jsonrpc":"2.0","method":"turn/start","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":6,"params":{}}
            ]"#,
        )
        .expect("batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation).expect("dispatch batch");

        let response = receiver.try_recv().expect("batch response").message;
        let responses = response.as_array().expect("batch response array");
        assert_eq!(responses.len(), 6);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(
            responses[1]["result"]["transports"][0]["transport"],
            "stdio"
        );
        for (response, id) in responses.iter().skip(2).zip([3, 4, 5]) {
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], -32600);
            assert_eq!(response["error"]["message"], "Invalid Request");
        }
        assert_eq!(responses[5]["id"], 6);
        assert!(responses[5]["result"]["transports"].is_array());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn all_notification_batch_has_no_output_even_for_unknown_method_or_invalid_params() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server =
            AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut control_receiver, mut event_receiver) = test_output_channels(8, 8);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"thread/read","params":{}},
                {"jsonrpc":"2.0","method":"unknown","params":{}}
            ]"#,
        )
        .expect("notification batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation)
            .expect("dispatch notification batch");

        assert!(control_receiver.try_recv().is_err());
        assert!(event_receiver.try_recv().is_err());
    }

    #[test]
    fn notification_only_request_is_invalid_without_changing_batch_notification_contract() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server =
            AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut control_receiver, mut event_receiver) = test_output_channels(2, 2);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialized","id":1,"params":{}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"thread/read","params":{}}
            ]"#,
        )
        .expect("mixed notification batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation)
            .expect("dispatch mixed notification batch");

        let output = control_receiver
            .try_recv()
            .expect("invalid request response");
        let responses = output.message.as_array().expect("batch response array");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert!(event_receiver.try_recv().is_err());
    }

    #[test]
    fn development_evaluation_method_is_not_a_product_worker_method() {
        let message: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"eval/run","id":1,"params":{"manifest":"manifest.json","runId":"run"}}"#,
        )
        .expect("unknown request");

        assert!(!is_request_worker_method(&message));
        let notification: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"eval/run","params":{"manifest":"manifest.json","runId":"run"}}"#,
        )
        .expect("unknown notification");
        assert!(!is_request_worker_method(&notification));
    }

    #[test]
    fn turn_resume_requests_are_admitted_to_the_blocking_request_worker() {
        let request: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"turn/resume","id":1,"params":{"turnId":"turn"}}"#,
        )
        .expect("turn/resume request");
        assert!(is_request_worker_method(&request));

        let notification: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"turn/resume","params":{"turnId":"turn"}}"#,
        )
        .expect("turn/resume notification");
        assert!(!is_request_worker_method(&notification));
    }

    #[test]
    fn empty_batch_returns_standard_invalid_request() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server =
            AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver, _event_receiver) = test_output_channels(1, 1);

        dispatch_batch(
            &mut server,
            JsonRpcPayload::EmptyBatch,
            &outputs,
            &cancellation,
        )
        .expect("dispatch empty batch");

        let response = receiver
            .try_recv()
            .expect("invalid request response")
            .message;
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], -32600);
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
    fn request_capacity_exceeded_has_a_stable_typed_response() {
        let response = request_capacity_error_value(Some(JsonRpcId::String("request-7".into())));

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], "request-7");
        assert_eq!(response["error"]["code"], -32006);
        assert_eq!(response["error"]["message"], "Request capacity exceeded");
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

    #[test]
    fn failed_worker_does_not_drop_other_active_worker_handles() {
        block_on(async {
            let mut workers = JoinSet::<Result<(), String>>::new();
            workers.spawn(async { Err("worker failed".to_string()) });
            let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
            workers.spawn(async move {
                release_receiver.await.expect("release active worker");
                Ok(())
            });

            let error = workers
                .join_next()
                .await
                .expect("failed worker result")
                .expect("failed worker task")
                .expect_err("failed worker is reported");
            assert_eq!(error, "worker failed");
            release_sender.send(()).expect("release active worker");
            assert_eq!(
                workers
                    .join_next()
                    .await
                    .expect("active worker result")
                    .unwrap(),
                Ok(())
            );
        });
    }
}
