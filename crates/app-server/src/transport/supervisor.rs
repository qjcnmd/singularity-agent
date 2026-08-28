//! Stdio supervisor 与 worker 生命周期：supervisor 拥有帧分类、ordinary/control
//! 分发 worker、turn worker 与有界关闭/join。
use super::*;
use crate::{
    AppServer, AppServerCancellationHandle, AppServerControlHandle, AppServerError, AppServerOutput,
};
use serde_json::Value;
use singularity_protocol::{JsonRpcInbound, JsonRpcMessage, Method, parse_json_rpc_payload};
use singularity_runtime::ProviderConfigSnapshot;
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
    // 普通请求保留单一 AppServer owner；控制 lane 只接收可克隆的
    // 活动 turn/inbox 句柄，因此不可能产生第二个可变状态 owner。
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
                terminal_error = Some(match result {
                    Ok(Ok(())) => "stdout writer task failed: writer stopped unexpectedly".to_string(),
                    Ok(Err(error)) => format!("stdout writer task failed: {error}"),
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
                if let Err(error) = route_inbound_line(
                    line,
                    &turn_tx,
                    &control_tx,
                    &ordinary_tx,
                    &output_tx,
                    &cancellation,
                    &ready_for_turn,
                )
                .await
                {
                    terminal_error = Some(error);
                    break;
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
    if !turn_dispatcher_done {
        // dispatcher 在 turn_rx 关闭且 worker 全部收敛后退出；宽限到点 abort。
        if let Some(error) = wait_task_graceful(
            &mut turn_dispatcher_task,
            shutdown_deadline,
            "turn dispatcher",
        )
        .await
        {
            worker_error.get_or_insert(error);
        }
    }
    if !ordinary_done
        && let Some(error) =
            wait_task_graceful(&mut ordinary_task, shutdown_deadline, "ordinary dispatch").await
    {
        worker_error.get_or_insert(error);
    }
    if !control_done
        && let Some(error) =
            wait_task_graceful(&mut control_task, shutdown_deadline, "control dispatch").await
    {
        worker_error.get_or_insert(error);
    }
    drop(output_tx);

    if !writer_done
        && let Some(error) =
            wait_task_graceful(&mut writer, shutdown_deadline, "stdout writer").await
    {
        // 保留既有的 writer 错误措辞（内部错误带来源前缀，超时文本原样）。
        let error = if error.starts_with("timed out waiting for") {
            error
        } else {
            format!("stdout writer task failed: {error}")
        };
        worker_error.get_or_insert(error);
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
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 解析一帧 JSON-Lines 输入并转交到唯一匹配的请求 lane。
///
/// 解析错误和队列满错误都通过输出队列回复；只有输出队列或 turn lane
/// 本身关闭时才把错误交回 supervisor，由 supervisor 统一进入关闭流程。
async fn route_inbound_line(
    line: String,
    turn_tx: &mpsc::Sender<JsonRpcMessage>,
    control_tx: &mpsc::Sender<JsonRpcMessage>,
    ordinary_tx: &mpsc::Sender<JsonRpcMessage>,
    output_tx: &mpsc::Sender<Value>,
    cancellation: &AppServerCancellationHandle,
    ready_for_turn: &AtomicBool,
) -> Result<(), String> {
    if line.trim().is_empty() {
        return Ok(());
    }
    let inbound = match parse_json_rpc_payload(&line) {
        Ok(inbound) => inbound,
        Err(_) => {
            send_output_async(
                output_tx.clone(),
                cancellation.clone(),
                JsonRpcMessage::parse_error().to_wire_value(),
            )
            .await?;
            return Ok(());
        }
    };
    let message = match inbound {
        JsonRpcInbound::Message(message) => message,
        JsonRpcInbound::Invalid { id } => {
            send_output_async(
                output_tx.clone(),
                cancellation.clone(),
                JsonRpcMessage::invalid_request(id).to_wire_value(),
            )
            .await?;
            return Ok(());
        }
    };
    if is_turn_request(&message) {
        turn_tx
            .send(message)
            .await
            .map_err(|_| "turn dispatcher channel closed".to_string())?;
    } else if is_turn_control(&message) && ready_for_turn.load(Ordering::SeqCst) {
        enqueue_request(
            control_tx,
            message,
            output_tx,
            cancellation,
            "control request queue is full",
        )
        .await?;
    } else {
        enqueue_request(
            ordinary_tx,
            message,
            output_tx,
            cancellation,
            "ordinary request queue is full",
        )
        .await?;
    }
    Ok(())
}

/// 非阻塞入队；队列满时仅向对应请求回复内部错误，不阻塞 stdin lane。
async fn enqueue_request(
    queue: &mpsc::Sender<JsonRpcMessage>,
    message: JsonRpcMessage,
    output_tx: &mpsc::Sender<Value>,
    cancellation: &AppServerCancellationHandle,
    full_message: &str,
) -> Result<(), String> {
    if let Err(error) = queue.try_send(message) {
        let Some(id) = error.into_inner().id().cloned() else {
            return Ok(());
        };
        send_output_async(
            output_tx.clone(),
            cancellation.clone(),
            internal_error_value(Some(id), full_message),
        )
        .await?;
    }
    Ok(())
}

/// 在宽限期内等待后台任务收敛，超时一律 abort。残留的 dispatch 任务持有
/// output_tx 克隆，不 abort 会推迟 writer 侧的通道关闭——writer 被杀在
/// write_all 半截帧的窗口由此消除；writer 自身超时仍是最后的强制手段。
async fn wait_task_graceful(
    task: &mut tokio::task::JoinHandle<Result<(), String>>,
    deadline: Instant,
    label: &str,
) -> Option<String> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        // 宽限期已耗尽：强制 abort 并报告。
        task.abort();
        return Some(format!("timed out waiting for {label} during shutdown"));
    };
    match tokio::time::timeout(remaining, &mut *task).await {
        Ok(Ok(Ok(()))) => None,
        // 任务自身返回的错误（worker 内部错误文本）原样透出。
        Ok(Ok(Err(error))) => Some(error),
        // join 层面失败（任务 panic）带上任务来源。
        Ok(Err(error)) => Some(format!("{label} task failed: {error}")),
        Err(_) => {
            task.abort();
            Some(format!("timed out waiting for {label} during shutdown"))
        }
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
        Ok(crate::TurnClaim::Responded(response)) => {
            send_output_async(output_tx.clone(), cancellation.clone(), response).await
        }
        Ok(crate::TurnClaim::Accepted(claim)) => {
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
        let home = singularity_runtime::user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for sessions".to_string())?;
        let cwd = std::env::current_dir()
            .map_err(|error| format!("failed to read app-server cwd: {error}"))?;
        singularity_runtime::ensure_singularity_home_outside_workspace(&home, &cwd)?;
    }
    let paths = crate::paths::AppPaths::resolve()?;
    paths.prepare()?;
    let provider_snapshot =
        ProviderConfigSnapshot::capture(|name| std::env::var(name).ok(), runtime_handle);
    Ok(AppServer::new(provider_snapshot, paths.sessions_dir))
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
/// 不读取会话 JSONL，也不等待 ordinary owner。
///
/// 控制 lane 的就绪点 = initialize 请求处理完成（`ready_for_turn`），
/// 不等待 `initialized` 通知；与 turn lane 共用同一就绪合同。ordinary
/// 方法仍以 `initialized` 通知为门禁（dispatch 层）。
fn is_turn_control(message: &JsonRpcMessage) -> bool {
    matches!(
        message.method_name(),
        Some(method)
            if Method::parse(method).is_some_and(|method| matches!(
                method,
                Method::TurnInterrupt | Method::TurnSteer | Method::TurnFollowUp
            ))
    )
}

/// 唯一 ordinary AppServer owner。输入 reader 只把普通请求排入有界队列；
/// 此任务按到达顺序处理队列并持有该 owner 的协调状态。
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
    claim: crate::TurnStartClaim,
) -> Result<(), String> {
    let request_id = claim.request_id;
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
