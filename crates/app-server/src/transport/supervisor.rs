//! Stdio supervisor and worker lifecycle.
//!
//! The supervisor owns frame classification, ordinary/control dispatch workers, turn workers,
//! and bounded shutdown/join.
use super::*;
use serde_json::Value;
use singularity_app_server::{
    AppServer, AppServerCancellationHandle, AppServerControlHandle, AppServerError, AppServerOutput,
};
use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::{JsonRpcInbound, JsonRpcMessage, Method, parse_json_rpc_payload};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::sync::mpsc;

/// 在单一 Tokio runtime 内运行 stdio app-server 控制面。
pub(crate) async fn run(runtime_handle: tokio::runtime::Handle) -> Result<(), String> {
    run_with_io(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        runtime_handle,
    )
    .await
}

/// 在 stdio 上运行 JSON-Lines 控制面；所有同步 AppServer 工作跨 blocking 边界执行。
async fn run_with_io<R, W>(
    reader: R,
    writer: W,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = tokio::task::spawn_blocking(move || initialize_app_server(runtime_handle))
        .await
        .map_err(|error| format!("app-server startup task failed: {error}"))??;
    run_server_with_io(server, reader, writer).await
}

/// 在给定 AppServer 实例和 IO 流上运行 JSON-Lines 控制面。
pub(crate) async fn run_server_with_io<R, W>(
    server: AppServer,
    reader: R,
    writer: W,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let cancellation = server.cancellation_handle();
    // Ordinary requests retain the single AppServer owner. The control
    // lane receives only cloneable active-turn/inbox handles and therefore
    // cannot create a second mutable state owner.
    let control_handle = server.control_handle();
    let turn_factory = server
        .turn_worker()
        .map_err(|error| format!("app-server turn factory setup failed: {error}"))?;
    let (output_tx, mut output_rx) = mpsc::channel::<Value>(OUTPUT_QUEUE_CAPACITY);
    let writer_cancellation = cancellation.clone();
    let mut output = writer;
    let mut writer = tokio::spawn(async move {
        write_output_queue(&mut output_rx, &mut output, &writer_cancellation).await
    });
    let mut writer_done = false;
    let mut writer_result = None;
    let mut writer_timeout = false;
    let ready_for_turn = Arc::new(AtomicBool::new(false));
    let ready_notify = Arc::new(tokio::sync::Notify::new());
    let (ordinary_tx, ordinary_rx) = mpsc::channel::<JsonRpcMessage>(64);
    let (control_tx, control_rx) = mpsc::channel::<JsonRpcMessage>(64);
    let (turn_tx, turn_rx) = mpsc::channel::<JsonRpcMessage>(64);
    let mut ordinary_task = tokio::spawn(run_ordinary_dispatch(
        server,
        ordinary_rx,
        output_tx.clone(),
        cancellation.clone(),
        Arc::clone(&ready_for_turn),
        Arc::clone(&ready_notify),
    ));
    let mut control_task = tokio::spawn(run_control_dispatch(
        control_handle,
        control_rx,
        output_tx.clone(),
        cancellation.clone(),
    ));
    // turn dispatcher 独立消费 turn/start：claim 与 worker 启动不阻塞 stdin
    // 读取，控制 lane（interrupt/steer/followUp）在 claim 期间保持可路由。
    let mut turn_dispatcher_task = tokio::spawn(run_turn_dispatcher(
        turn_factory,
        turn_rx,
        Arc::clone(&ready_for_turn),
        Arc::clone(&ready_notify),
        output_tx.clone(),
        cancellation.clone(),
    ));
    let mut ordinary_done = false;
    let mut control_done = false;
    let mut turn_dispatcher_done = false;
    let mut reader = reader;
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
            result = &mut turn_dispatcher_task, if !turn_dispatcher_done => {
                turn_dispatcher_done = true;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => terminal_error = Some(error),
                    Err(error) => terminal_error =
                        Some(format!("turn dispatcher task failed: {error}")),
                }
                break;
            }
            result = &mut ordinary_task, if !ordinary_done => {
                ordinary_done = true;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => terminal_error = Some(error),
                    Err(error) => terminal_error = Some(format!("ordinary dispatch task failed: {error}")),
                }
                break;
            }
            result = &mut control_task, if !control_done => {
                control_done = true;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => terminal_error = Some(error),
                    Err(error) => terminal_error = Some(format!("control dispatch task failed: {error}")),
                }
                break;
            }
            line = read_bounded_line(&mut reader) => {
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
                let inbound = match parse_json_rpc_payload(&line) {
                    Ok(inbound) => inbound,
                    Err(_) => {
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
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
                let message = match inbound {
                    JsonRpcInbound::Message(message) => message,
                    JsonRpcInbound::Invalid { id } => {
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
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
                if is_turn_request(&message) {
                    // 转交 turn dispatcher（未就绪的消息由 dispatcher 暂存等待）；
                    // claim 与 worker 启动不阻塞 stdin 读取。
                    turn_tx
                        .send(message)
                        .await
                        .map_err(|_| "turn dispatcher channel closed".to_string())?;
                    continue;
                } else if is_turn_control(&message) && ready_for_turn.load(Ordering::SeqCst) {
                    if let Err(error) = control_tx.try_send(message) {
                        let Some(id) = error.into_inner().id().cloned() else {
                            continue;
                        };
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
                            cancellation.clone(),
                            internal_error_value(Some(id), "control request queue is full"),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                    }
                } else {
                    if let Err(error) = ordinary_tx.try_send(message) {
                        let Some(id) = error.into_inner().id().cloned() else {
                            continue;
                        };
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
                            cancellation.clone(),
                            internal_error_value(Some(id), "ordinary request queue is full"),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    drop(ordinary_tx);
    drop(control_tx);
    drop(turn_tx);
    let stop_result = cancellation
        .request_execution_stop()
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"));
    let mut worker_error = None;
    if !turn_dispatcher_done
        && let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now())
    {
        // dispatcher 在 turn_rx 关闭且 worker 全部收敛后退出；宽限到点 abort。
        match tokio::time::timeout(remaining, &mut turn_dispatcher_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                worker_error.get_or_insert(error);
            }
            Ok(Err(error)) => {
                worker_error.get_or_insert(format!("turn dispatcher task failed: {error}"));
            }
            Err(_) => {
                worker_error.get_or_insert(
                    "timed out waiting for turn dispatcher during shutdown".to_string(),
                );
                turn_dispatcher_task.abort();
            }
        }
    }
    if !ordinary_done
        && let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now())
    {
        match tokio::time::timeout(remaining, &mut ordinary_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                worker_error.get_or_insert(error);
            }
            Ok(Err(error)) => {
                worker_error.get_or_insert(format!("ordinary dispatch task failed: {error}"));
            }
            Err(_) => {
                worker_error.get_or_insert(
                    "timed out waiting for ordinary dispatch during shutdown".to_string(),
                );
            }
        }
    }
    if !control_done
        && let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now())
    {
        match tokio::time::timeout(remaining, &mut control_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                worker_error.get_or_insert(error);
            }
            Ok(Err(error)) => {
                worker_error.get_or_insert(format!("control dispatch task failed: {error}"));
            }
            Err(_) => {
                worker_error.get_or_insert(
                    "timed out waiting for control dispatch during shutdown".to_string(),
                );
            }
        }
    }
    drop(output_tx);

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

/// 独立消费 turn/start：claim 与 worker 启动不阻塞 stdin 读取，控制 lane
/// 在 claim 期间保持可路由。未就绪时消息暂存等待就绪标志；worker 失败
/// 经任务返回值传播给 supervisor。
async fn run_turn_dispatcher(
    mut turn_factory: AppServer,
    mut turn_rx: mpsc::Receiver<JsonRpcMessage>,
    ready_for_turn: Arc<AtomicBool>,
    ready_notify: Arc<tokio::sync::Notify>,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let mut turn_tasks: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let mut pending: std::collections::VecDeque<JsonRpcMessage> = std::collections::VecDeque::new();
    loop {
        // 就绪后按到达顺序处理暂存的 turn/start。
        if ready_for_turn.load(Ordering::SeqCst) {
            while let Some(message) = pending.pop_front() {
                handle_streaming_turn_start(
                    message,
                    &mut turn_factory,
                    &outputs,
                    &cancellation,
                    &mut turn_tasks,
                )
                .await?;
            }
        }
        let ready_fut = ready_notify.notified();
        tokio::select! {
            biased;
            // 暂存非空且就绪标志刚置位：回到循环顶部按序处理。
            _ = ready_fut, if !pending.is_empty() => {}
            Some(message) = turn_rx.recv() => {
                if ready_for_turn.load(Ordering::SeqCst) {
                    handle_streaming_turn_start(
                        message,
                        &mut turn_factory,
                        &outputs,
                        &cancellation,
                        &mut turn_tasks,
                    )
                    .await?;
                } else {
                    pending.push_back(message);
                }
            }
            Some(join_result) = turn_tasks.join_next(), if !turn_tasks.is_empty() => {
                match join_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(format!("turn worker task failed: {error}")),
                }
            }
            else => break,
        }
    }
    Ok(())
}

/// 在流式 lane 处理 turn/start：claim 同步裁定（先到先得、后到立即
/// invalid-state），Accepted 后启动独立 turn worker 执行整条链。
async fn handle_streaming_turn_start(
    message: JsonRpcMessage,
    turn_factory: &mut AppServer,
    output_tx: &mpsc::Sender<Value>,
    cancellation: &AppServerCancellationHandle,
    turn_tasks: &mut tokio::task::JoinSet<Result<(), String>>,
) -> Result<(), String> {
    let request_id = message.id().cloned();
    let claim_result = tokio::task::spawn_blocking({
        let factory = turn_factory.clone();
        move || factory.claim_turn(message)
    })
    .await
    .map_err(|error| format!("turn claim task failed: {error}"))?;
    match claim_result {
        Err(error) => {
            send_output_async(
                output_tx.clone(),
                cancellation.clone(),
                transport_error_value(request_id, &error),
            )
            .await
        }
        Ok(singularity_app_server::TurnClaim::Responded(response)) => {
            send_output_async(output_tx.clone(), cancellation.clone(), response).await
        }
        Ok(singularity_app_server::TurnClaim::Accepted(claim)) => {
            // turn_worker 仅克隆共享 Arc，无需 spawn_blocking 线程池跳转。
            match turn_factory.turn_worker() {
                Ok(worker) => {
                    let worker_outputs = output_tx.clone();
                    let worker_cancellation = cancellation.clone();
                    turn_tasks.spawn_blocking(move || {
                        run_turn_request(worker, worker_outputs, worker_cancellation, claim)
                    });
                    Ok(())
                }
                Err(error) => {
                    send_output_async(
                        output_tx.clone(),
                        cancellation.clone(),
                        transport_error_value(request_id, &error),
                    )
                    .await
                }
            }
        }
    }
}

fn initialize_app_server(runtime_handle: tokio::runtime::Handle) -> Result<AppServer, String> {
    // 显式 SINGULARITY_HOME 时，先于任何目录创建校验其不在当前仓库内
    // （model 层配置校验的启动期第一道防线；违规 fail closed）。
    if std::env::var_os("SINGULARITY_HOME").is_some() {
        let home = singularity_core::user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for session index".to_string())?;
        ensure_home_outside_current_repo(&home)?;
    }
    let paths = singularity_app_server::paths::AppPaths::resolve()?;
    paths.prepare()?;
    // 目录投影缓存过期时后台刷新（拉取 models.dev，失败 fail-soft 不阻塞启动）。
    if singularity_model::catalog_cache_is_stale() {
        let refresh_handle = runtime_handle.clone();
        let _ = runtime_handle
            .spawn(async move { singularity_model::refresh_catalog_cache(&refresh_handle) });
    }
    // 进程内会话索引：启动时从 sessions 目录的 JSONL rollout 重建（JSONL 是
    // 唯一持久事实源，索引不落盘）。
    let session_index =
        singularity_app_server::SessionIndex::from_sessions_dir(&paths.sessions_dir)
            .map_err(|error| format!("failed to scan app-server session index: {error}"))?;
    let provider_snapshot =
        ProviderConfigSnapshot::capture(|name| std::env::var(name).ok(), runtime_handle);
    Ok(AppServer::new(session_index, provider_snapshot).with_sessions_dir(paths.sessions_dir))
}

/// 判断单请求是否需要后台 turn worker。
fn is_turn_request(message: &JsonRpcMessage) -> bool {
    !message.is_notification()
        && matches!(
            message.method_name(),
            Some(method) if method == Method::TurnStart.as_str()
        )
}

/// 三个活动 turn 控制请求走独立 lane；它们只触碰 active-turn maps，
/// 不读取会话索引，也不等待 ordinary owner。
///
/// 控制 lane 的就绪点 = initialize 请求处理完成（`ready_for_turn`），
/// 不等待 `initialized` 通知；与 turn lane 共用同一就绪合同。ordinary
/// 方法仍以 `initialized` 通知为门禁（dispatch 层）。
fn is_turn_control(message: &JsonRpcMessage) -> bool {
    matches!(
        message.method_name(),
        Some(method)
            if matches!(
                method,
                "turn/interrupt" | "turn/steer" | "turn/followUp"
            )
    )
}

/// 唯一 ordinary AppServer owner。输入 reader 只把普通请求排入有界队列；
/// 此任务按到达顺序处理队列并持有该 owner 的会话索引。
async fn run_ordinary_dispatch(
    mut server: AppServer,
    mut requests: mpsc::Receiver<JsonRpcMessage>,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
    ready_for_turn: Arc<AtomicBool>,
    ready_notify: Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    while let Some(message) = requests.recv().await {
        let direct_outputs = outputs.clone();
        let direct_cancellation = cancellation.clone();
        let ready_for_turn = Arc::clone(&ready_for_turn);
        let ready_notify = Arc::clone(&ready_notify);
        let task = tokio::task::spawn_blocking(move || {
            let notification = message.is_notification();
            let request_id = message.id().cloned();
            let result = server.handle_with_output(message);
            let dispatch_result = match result {
                Ok(messages) => {
                    send_app_server_outputs(&direct_outputs, &direct_cancellation, messages)
                }
                Err(error) if !notification => send_output(
                    &direct_outputs,
                    &direct_cancellation,
                    transport_error_value(request_id, &error),
                )
                .map(|_| ()),
                Err(_) => Ok(()),
            };
            // 就绪点 = initialize 请求处理完成（回执已发出）：与响应写出在同一
            // 处理任务内先后发生，消除就绪标志晚于回执的窗口。`initialized`
            // 通知仍把守 ordinary 门禁，不再延迟 turn lane 的就绪。
            if server.ready_for_turn_worker() {
                ready_for_turn.store(true, Ordering::SeqCst);
                ready_notify.notify_waiters();
            }
            let shutdown = server.shutdown_requested();
            (server, dispatch_result, shutdown)
        });
        let (next_server, result, shutdown) = task
            .await
            .map_err(|error| format!("request dispatch task failed: {error}"))?;
        server = next_server;
        result?;
        if shutdown {
            break;
        }
    }
    Ok(())
}

/// 独立 control owner。它使用 AppServer 的共享活动-turn句柄，因而控制
/// 请求不会排在 thread/read、thread/list 等 ordinary state request 后面。
async fn run_control_dispatch(
    control: AppServerControlHandle,
    mut requests: mpsc::Receiver<JsonRpcMessage>,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    while let Some(message) = requests.recv().await {
        let direct_outputs = outputs.clone();
        let direct_cancellation = cancellation.clone();
        let task = tokio::task::spawn_blocking({
            let control = control.clone();
            move || {
                let request_id = message.id().cloned();
                let result = control.handle(message);
                match result {
                    Ok(messages) => {
                        send_app_server_outputs(&direct_outputs, &direct_cancellation, messages)
                    }
                    Err(error) => send_output(
                        &direct_outputs,
                        &direct_cancellation,
                        transport_error_value(request_id, &error),
                    )
                    .map(|_| ()),
                }
            }
        });
        let result = task
            .await
            .map_err(|error| format!("control dispatch task failed: {error}"))?;
        result?;
    }
    Ok(())
}

/// 在单一 turn 工作线程内执行已预订的 turn/start，事件与最终响应顺序入队。
pub(crate) fn run_turn_request(
    mut worker: AppServer,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
    claim: singularity_app_server::TurnStartClaim,
) -> Result<(), String> {
    let request_id = claim.request_id.clone();
    let mut output_error = None;
    let mut emit = |output: AppServerOutput| {
        if output_error.is_none()
            && let Err(error) = send_output(&outputs, &cancellation, output)
        {
            output_error = Some(error);
        }
    };
    let result = worker.run_turn_started(claim, &mut emit);
    if let Some(error) = output_error {
        return Err(error);
    }
    if let Err(error) = result {
        match &error {
            AppServerError::TurnTerminalization { .. } => {
                return Err(format!("fatal turn worker error: {error}"));
            }
            _ => {
                send_output(
                    &outputs,
                    &cancellation,
                    request_error_value(Some(request_id), &error),
                )?;
            }
        }
    }
    Ok(())
}
