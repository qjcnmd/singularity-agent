//! `AppServer` 的标准输入输出（stdio）传输层。
//!
//! 输入独立读取；请求工作线程准入队列和传输队列均有界，由单一写入方串行化 JSON 行输出，
//! state/gap 事件在事件队列中可靠阻塞；progress 事件共用该有界队列但可丢弃，控制响应保持独立。

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cap_fs_ext::{FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde_json::Value;
use singularity_app_server::{
    AppServer, AppServerCancellationHandle, AppServerError, AppServerOutput, OutputOrderCoordinator,
};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_model::{PROVIDER_CAPABILITY_CACHE_FILE_NAME, ProviderConfigSnapshot};
use singularity_protocol::{
    EventClass, EventDelivery, EventGap, EventGapReason, EventMetadata, JsonRpcBatchItem,
    JsonRpcId, JsonRpcMessage, JsonRpcPayload, Method, parse_json_rpc_payload,
};
use singularity_store::SessionStore;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const MAX_REQUEST_WORKERS: usize = 16;
const INPUT_QUEUE_CAPACITY: usize = 64;
const CONTROL_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 256;
const REQUEST_CAPACITY_EXCEEDED: &str = "AppServer request capacity exceeded";
const FILE_BACKED_STORE_REQUIRED: &str =
    "app-server requires a file-backed SINGULARITY_APP_SERVER_DB";
const SAFE_FILE_BACKED_STATE_REQUIRED: &str =
    "app-server requires a canonical regular file-backed state database";
const CACHE_TEMP_FILE_PREFIX: &str = ".provider-capability-cache.json.tmp-";
const CACHE_KEY_LOCK_FILE_PREFIX: &str = ".provider-capability-cache.key-lock-";

#[derive(Clone)]
struct OutputChannels {
    control: SyncSender<QueuedOutput>,
    event: SyncSender<QueuedOutput>,
    pending_event_gap: Arc<Mutex<Option<PendingEventGap>>>,
    send_lock: Arc<Mutex<()>>,
    order_state: OutputOrderCoordinator,
}

#[derive(Debug)]
struct QueuedOutput {
    order: u64,
    to_order: u64,
    message: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingEventGap {
    from_cursor: u64,
    to_cursor: u64,
    from_order: u64,
    to_order: u64,
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

trait ExecutionStop {
    fn request_execution_stop(&self);
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }
}

/// 启动标准输入输出服务；传输或生命周期关闭失败时以非零状态退出。
/// 负责 `stdin` 读取、请求工作线程准入、`stdout` 串行化和优雅关闭。
pub(super) fn run() -> Result<(), String> {
    let configured_db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    let (db_path, capability_cache_path) = prepare_app_server_state_paths(&configured_db_path)?;
    let store = SessionStore::open(&db_path)
        .map_err(|error| format!("failed to open app-server store {db_path}: {error}"))?;
    validate_database_file(Path::new(&db_path), false)?;
    store
        .recover_unowned_workspace_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture_with_cache_path(
        |name| std::env::var(name).ok(),
        Some(capability_cache_path),
    );
    let mut server = AppServer::new(store, provider_snapshot);
    let cancellation = server.cancellation_handle();
    let output_order = server.output_order_coordinator();
    let (control_tx, control_rx) = mpsc::sync_channel::<QueuedOutput>(CONTROL_QUEUE_CAPACITY);
    let (event_tx, event_rx) = mpsc::sync_channel::<QueuedOutput>(EVENT_QUEUE_CAPACITY);
    let outputs = OutputChannels {
        control: control_tx,
        event: event_tx,
        pending_event_gap: Arc::new(Mutex::new(None)),
        send_lock: Arc::new(Mutex::new(())),
        order_state: output_order,
    };
    let (writer_error_tx, writer_error_rx) = mpsc::channel::<String>();
    let writer_cancellation = cancellation.clone();
    let writer_order_state = outputs.order_state.clone();
    let writer = thread::spawn(move || -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        let result = write_output_queue(
            control_rx,
            event_rx,
            &mut stdout,
            &writer_cancellation,
            writer_order_state,
        );
        if let Err(error) = &result {
            let _ = writer_error_tx.send(error.clone());
        }
        result
    });
    let (input_tx, input_rx) =
        mpsc::sync_channel::<Result<Option<String>, String>>(INPUT_QUEUE_CAPACITY);
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if input_tx
                .send(
                    line.map(Some)
                        .map_err(|error| format!("failed to read stdin: {error}")),
                )
                .is_err()
            {
                return;
            }
        }
        let _ = input_tx.send(Ok(None));
    });
    let mut request_workers = Vec::new();
    let mut terminal_error = None;

    loop {
        if let Err(error) = reap_finished_workers(&mut request_workers) {
            terminal_error = Some(error);
            break;
        }
        match writer_error_rx.try_recv() {
            Ok(error) => {
                terminal_error = Some(error);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                terminal_error = Some("stdout writer stopped unexpectedly".to_string());
                break;
            }
        }
        let line = match input_rx.recv_timeout(INPUT_POLL_INTERVAL) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                terminal_error = Some(error);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminal_error = Some("stdin reader stopped unexpectedly".to_string());
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let payload = match parse_json_rpc_payload(&line) {
            Ok(payload) => payload,
            Err(_) => {
                if let Err(error) = send_output(
                    &outputs,
                    OutputKind::Control,
                    &cancellation,
                    JsonRpcMessage::parse_error().to_wire_value(),
                ) {
                    terminal_error = Some(error);
                    break;
                }
                continue;
            }
        };
        if !matches!(payload, JsonRpcPayload::Single(_)) {
            if let Err(error) = dispatch_batch(&mut server, payload, &outputs, &cancellation) {
                terminal_error = Some(error);
                break;
            }
            if server.shutdown_requested() {
                break;
            }
            continue;
        }
        let JsonRpcPayload::Single(item) = payload else {
            unreachable!("non-single payload handled above")
        };
        let message = match item {
            JsonRpcBatchItem::Message(message) => message,
            JsonRpcBatchItem::Invalid { id } => {
                if let Err(error) = send_output(
                    &outputs,
                    OutputKind::Control,
                    &cancellation,
                    JsonRpcMessage::invalid_request(id).to_wire_value(),
                ) {
                    terminal_error = Some(error);
                    break;
                }
                continue;
            }
        };
        let request_id = message.id().cloned();
        if is_request_worker_method(&message) && server.ready_for_turn_worker() {
            if request_workers.len() >= MAX_REQUEST_WORKERS {
                if let Err(error) = send_output(
                    &outputs,
                    OutputKind::Control,
                    &cancellation,
                    internal_error_value(request_id, REQUEST_CAPACITY_EXCEEDED),
                ) {
                    terminal_error = Some(error);
                    break;
                }
                continue;
            }
            match server.turn_worker() {
                Ok(worker) => {
                    let worker_outputs = outputs.clone();
                    let worker_cancellation = cancellation.clone();
                    match thread::Builder::new()
                        .name("singularity-request".to_string())
                        .spawn(move || {
                            run_request_worker(worker, message, worker_outputs, worker_cancellation)
                        }) {
                        Ok(worker) => request_workers.push(worker),
                        Err(error) => {
                            if let Err(error) = send_output(
                                &outputs,
                                OutputKind::Control,
                                &cancellation,
                                internal_error_value(
                                    request_id,
                                    format!("failed to start request worker: {error}"),
                                ),
                            ) {
                                terminal_error = Some(error);
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Err(error) = send_output(
                        &outputs,
                        OutputKind::Control,
                        &cancellation,
                        transport_error_value(request_id, &error),
                    ) {
                        terminal_error = Some(error);
                        break;
                    }
                }
            }
        } else {
            let notification = message.is_notification();
            match server.handle_with_output(message) {
                Ok(messages) => {
                    if let Err(error) = send_app_server_outputs(&outputs, &cancellation, messages) {
                        terminal_error = Some(error);
                    }
                }
                Err(error) if !notification => {
                    if let Err(error) = send_output(
                        &outputs,
                        OutputKind::Control,
                        &cancellation,
                        transport_error_value(request_id, &error),
                    ) {
                        terminal_error = Some(error);
                    }
                }
                Err(_) => {}
            }
            if terminal_error.is_some() {
                break;
            }
        }
        if server.shutdown_requested() {
            break;
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    let request_worker_result = server
        .request_execution_stop()
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        .and_then(|()| {
            join_request_workers_during_shutdown(&mut request_workers, shutdown_deadline)
        });
    let gap_result = flush_pending_event_gap(&outputs, &cancellation);
    drop(outputs);
    let writer_result = join_writer_during_shutdown(writer, shutdown_deadline);
    request_worker_result?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    gap_result?;
    writer_result?;
    Ok(())
}

fn is_request_worker_method(message: &JsonRpcMessage) -> bool {
    if message.method_name() == Some(Method::EvalRun.as_str()) {
        return true;
    }
    if message.is_notification() {
        return false;
    }
    matches!(
        message.method_name(),
        Some(method)
            if method == Method::TurnStart.as_str()
                || method == Method::ApprovalDecision.as_str()
                || method == Method::EvalRun.as_str()
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
    let result = if message.method_name() == Some(Method::TurnStart.as_str()) {
        worker.handle_turn_start_streaming_with_output(message, |output| {
            if output_error.is_none()
                && let Err(error) = send_reserved_output(
                    &outputs,
                    &cancellation,
                    output.reservation.order,
                    output.message,
                )
            {
                output_error = Some(error);
            } else if output_error.is_some() {
                outputs.order_state.complete(output.reservation.order);
            }
        })
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

fn reap_finished_workers(workers: &mut Vec<JoinHandle<Result<(), String>>>) -> Result<(), String> {
    let mut active = Vec::with_capacity(workers.len());
    let mut first_error = None;
    for worker in workers.drain(..) {
        if worker.is_finished() {
            if let Err(error) = join_request_worker(worker)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        } else {
            active.push(worker);
        }
    }
    *workers = active;
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn join_request_workers_during_shutdown(
    workers: &mut Vec<JoinHandle<Result<(), String>>>,
    deadline: Instant,
) -> Result<(), String> {
    let mut first_error = None;
    while !workers.is_empty() {
        if let Err(error) = reap_finished_workers(workers)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if workers.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            let timeout = format!(
                "timed out waiting for {} request worker(s) during shutdown",
                workers.len()
            );
            return Err(match first_error {
                Some(error) => format!("{error}; {timeout}"),
                None => timeout,
            });
        }
        thread::sleep(INPUT_POLL_INTERVAL);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn join_writer_during_shutdown(
    writer: JoinHandle<Result<(), String>>,
    deadline: Instant,
) -> Result<(), String> {
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            return Err("timed out waiting for stdout writer during shutdown".to_string());
        }
        thread::sleep(INPUT_POLL_INTERVAL);
    }
    writer
        .join()
        .map_err(|_| "stdout writer panicked".to_string())?
}

fn join_request_worker(worker: JoinHandle<Result<(), String>>) -> Result<(), String> {
    worker
        .join()
        .map_err(|_| "request worker panicked".to_string())?
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
    send_reserved_output(outputs, cancellation, order, message)
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
) -> Result<OutputSendStatus, String> {
    let kind = classify_output(&message);
    if kind == OutputKind::Event {
        loop {
            let pending_gap = {
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
                if pending_gap.is_none() {
                    return try_send_event(outputs, cancellation, order, message);
                }
                pending_gap
            };
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
            .send(QueuedOutput {
                order,
                to_order: order,
                message,
            })
            .map(|()| {
                outputs.order_state.enqueue(order);
                OutputSendStatus::Enqueued
            })
            .map_err(|_| {
                outputs.order_state.complete(order);
                cancellation.request_execution_stop();
                "stdout transport unavailable".to_string()
            }),
        OutputKind::ReliableEvent => outputs
            .event
            .send(QueuedOutput {
                order,
                to_order: order,
                message,
            })
            .map(|()| {
                outputs.order_state.enqueue(order);
                OutputSendStatus::Enqueued
            })
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
) -> Result<OutputSendStatus, String> {
    match outputs.event.try_send(QueuedOutput {
        order,
        to_order: order,
        message,
    }) {
        Ok(()) => {
            outputs.order_state.enqueue(order);
            Ok(OutputSendStatus::Enqueued)
        }
        Err(TrySendError::Full(output)) => record_event_gap(outputs, &output.message, order)
            .map_or_else(
                |error| {
                    outputs.order_state.complete(order);
                    Err(error)
                },
                |()| Ok(OutputSendStatus::EventDropped),
            ),
        Err(TrySendError::Disconnected(_)) => {
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
    let Some(pending) = pending_guard.as_ref().copied() else {
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
    match outputs.control.send(QueuedOutput {
        order: pending.from_order,
        to_order: pending.to_order,
        message,
    }) {
        Ok(()) => outputs
            .order_state
            .enqueue_range(pending.from_order, pending.to_order),
        Err(_) => {
            outputs
                .order_state
                .complete_range(pending.from_order, pending.to_order);
            cancellation.request_execution_stop();
            return Err("stdout transport unavailable".to_string());
        }
    }
    Ok(())
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
fn record_event_gap(outputs: &OutputChannels, message: &Value, order: u64) -> Result<(), String> {
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
            });
        }
        Some(pending)
            if metadata.cursor == pending.to_cursor.saturating_add(1)
                && order == pending.to_order.saturating_add(1) =>
        {
            pending.to_cursor = metadata.cursor;
            pending.to_order = order;
        }
        Some(_) => return Err("non-contiguous dropped progress cursor".to_string()),
    }
    Ok(())
}

/// 按发送顺序从两个有界队列取出一条 frame，避免 progress 被后续 control 越过。
fn next_output(
    control_rx: &Receiver<QueuedOutput>,
    event_rx: &Receiver<QueuedOutput>,
    control_open: &mut bool,
    event_open: &mut bool,
    order_state: &OutputOrderCoordinator,
    pending: &mut BTreeMap<u64, QueuedOutput>,
) -> Option<QueuedOutput> {
    loop {
        if *control_open {
            match control_rx.try_recv() {
                Ok(message) => {
                    pending.insert(message.order, message);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    *control_open = false;
                }
            }
        }
        if *event_open {
            match event_rx.try_recv() {
                Ok(message) => {
                    pending.insert(message.order, message);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    *event_open = false;
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
        let mut received = false;
        if *control_open {
            match control_rx.recv_timeout(INPUT_POLL_INTERVAL) {
                Ok(message) => {
                    pending.insert(message.order, message);
                    received = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => *control_open = false,
            }
        }
        if *event_open {
            match event_rx.recv_timeout(INPUT_POLL_INTERVAL) {
                Ok(message) => {
                    pending.insert(message.order, message);
                    received = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => *event_open = false,
            }
        }
        if !received && !*control_open && !*event_open {
            return None;
        }
    }
}

/// 串行写出控制与事件 frame；真实写入或 flush 失败才触发全局停止。
fn write_output_queue(
    control_rx: Receiver<QueuedOutput>,
    event_rx: Receiver<QueuedOutput>,
    stdout: &mut impl Write,
    cancellation: &dyn ExecutionStop,
    order_state: OutputOrderCoordinator,
) -> Result<(), String> {
    let mut control_open = true;
    let mut event_open = true;
    let mut pending = BTreeMap::new();
    while let Some(output) = next_output(
        &control_rx,
        &event_rx,
        &mut control_open,
        &mut event_open,
        &order_state,
        &mut pending,
    ) {
        if let Err(error) = write_json_line(stdout, &output.message) {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.flush() {
            cancellation.request_execution_stop();
            return Err(format!("failed to flush response: {error}"));
        }
        order_state.acknowledge_written(output.order, output.to_order);
    }
    Ok(())
}

/// 将一个 JSON-RPC 值严格串行化为一条以换行分隔的 `stdout` 记录。
fn write_json_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

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
        Receiver<QueuedOutput>,
        Receiver<QueuedOutput>,
    ) {
        let (control, control_rx) = mpsc::sync_channel(control_capacity);
        let (event, event_rx) = mpsc::sync_channel(event_capacity);
        (
            OutputChannels {
                control,
                event,
                pending_event_gap: Arc::new(Mutex::new(None)),
                send_lock: Arc::new(Mutex::new(())),
                order_state: OutputOrderCoordinator::new(),
            },
            control_rx,
            event_rx,
        )
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
        let (outputs, control_rx, _event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Control,
            &cancellation,
            serde_json::json!({"first": true}),
        )
        .expect("first output fits");

        let (attempted_sender, attempted_receiver) = mpsc::channel();
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
            control_rx.recv().expect("first control output").message["first"],
            true
        );
        sender.join().expect("control sender");
        assert_eq!(
            control_rx.recv().expect("second control output").message["second"],
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
        let (outputs, _control_rx, event_rx) = test_output_channels(1, 1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            OutputKind::Event,
            &cancellation,
            progress_event(1),
        )
        .expect("progress event fits");
        let (attempted_sender, attempted_receiver) = mpsc::channel();
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
        assert_eq!(event_rx.recv().expect("progress output").order, 0);
        sender.join().expect("state sender");
        assert_eq!(event_rx.recv().expect("state output").order, 1);
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn reverse_enqueue_race_keeps_event_cursor_and_stdout_order_contiguous() {
        let (outputs, control_rx, event_rx) = test_output_channels(2, 2);
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
            )
            .expect("later event queues first");
            sender_gate.wait();
            send_reserved_output(
                &sender_outputs,
                &sender_cancellation,
                first.order,
                progress_event(1),
            )
            .expect("earlier event queues after the barrier");
        });
        // The channel/barrier makes the reverse enqueue order deterministic: cursor 2 is
        // already queued before cursor 1 is released, so the writer must use reservations.
        send_gate.wait();
        sender.join().expect("reverse enqueue sender");
        let order_state = outputs.order_state.clone();
        drop(outputs);

        let mut stdout = Vec::new();
        write_output_queue(
            control_rx,
            event_rx,
            &mut stdout,
            &cancellation,
            order_state,
        )
        .expect("writer drains reserved events");
        let values = String::from_utf8(stdout)
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
        let (outputs, control_rx, event_rx) = test_output_channels(2, 1);
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

        let mut stdout = Vec::new();
        write_output_queue(
            control_rx,
            event_rx,
            &mut stdout,
            &cancellation,
            order_state,
        )
        .expect("writer drains both queues");
        let lines = String::from_utf8(stdout).expect("writer output is UTF-8");
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
    fn pending_gap_flush_does_not_hold_send_lock_across_control_backpressure() {
        let (outputs, control_rx, _event_rx) = test_output_channels(1, 1);
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

        let (attempted_sender, attempted_receiver) = mpsc::channel();
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

        control_rx.recv().expect("release occupied control output");
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

    impl Write for DisconnectedWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stdout_writer_disconnect_stops_all_execution() {
        let (outputs, control_rx, event_rx) = test_output_channels(1, 1);
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
        let error = write_output_queue(
            control_rx,
            event_rx,
            &mut stdout,
            &cancellation,
            order_state,
        )
        .expect_err("writer disconnect is transport-fatal");

        assert!(error.starts_with("failed to write response:"));
        assert_eq!(cancellation.request_count(), 1);
    }

    #[test]
    fn mixed_batch_is_sequential_and_only_requests_produce_ordered_responses() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None));
        let cancellation = server.cancellation_handle();
        let (outputs, receiver, _event_receiver) = test_output_channels(8, 8);
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

        let outputs = receiver.try_iter().collect::<Vec<_>>();
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
    fn all_notification_batch_has_no_output_even_for_unknown_method_or_invalid_params() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None));
        let cancellation = server.cancellation_handle();
        let (outputs, control_receiver, event_receiver) = test_output_channels(8, 8);
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
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None));
        let cancellation = server.cancellation_handle();
        let (outputs, control_receiver, event_receiver) = test_output_channels(2, 2);
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
    fn evaluation_requests_are_admitted_to_the_blocking_request_worker() {
        let message: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"eval/run","id":1,"params":{"manifest":"manifest.json","runId":"run"}}"#,
        )
        .expect("evaluation request");

        assert!(is_request_worker_method(&message));
        let notification: JsonRpcMessage = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"eval/run","params":{"manifest":"manifest.json","runId":"run"}}"#,
        )
        .expect("evaluation notification");
        assert!(is_request_worker_method(&notification));
    }

    #[test]
    fn empty_batch_returns_standard_invalid_request() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None));
        let cancellation = server.cancellation_handle();
        let (outputs, receiver, _event_receiver) = test_output_channels(1, 1);

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
    fn stdout_writer_join_obeys_the_shutdown_deadline() {
        let (release_sender, release_receiver) = mpsc::channel();
        let writer = thread::spawn(move || {
            release_receiver.recv().expect("release writer");
            Ok(())
        });

        let error = join_writer_during_shutdown(writer, Instant::now() + Duration::from_millis(20))
            .expect_err("stalled writer must not outlive the deadline");
        release_sender.send(()).expect("release detached writer");

        assert_eq!(error, "timed out waiting for stdout writer during shutdown");
    }

    #[test]
    fn failed_worker_does_not_drop_other_active_worker_handles() {
        let failed = thread::spawn(|| Err("worker failed".to_string()));
        while !failed.is_finished() {
            thread::yield_now();
        }
        let (release_sender, release_receiver) = mpsc::channel();
        let active = thread::spawn(move || {
            release_receiver.recv().expect("release active worker");
            Ok(())
        });
        let mut workers = vec![failed, active];

        let error = reap_finished_workers(&mut workers).expect_err("failed worker is reported");

        assert_eq!(error, "worker failed");
        assert_eq!(workers.len(), 1);
        release_sender.send(()).expect("release active worker");
        join_request_workers_during_shutdown(&mut workers, Instant::now() + Duration::from_secs(1))
            .expect("remaining worker is still tracked");
    }
}
