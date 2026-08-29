//! Stdio supervisor 与 worker 生命周期：单一分发 owner（对齐 codex 的
//! MessageProcessor 形状）+ 有界关闭/join。stdin reader 只入队，dispatch
//! 任务按到达顺序以快路径 handler 处理全部请求；Accepted 的 turn 交给
//! 独立 worker 线程，输出经唯一 writer 顺序写出。
use super::*;
use crate::{
    AppServer, AppServerCancellationHandle, AppServerError, AppServerOutput, TurnStartClaim,
};
use serde_json::Value;
use singularity_protocol::{JsonRpcInbound, JsonRpcMessage, Method, parse_json_rpc_payload};
use singularity_runtime::ProviderConfigSnapshot;
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
///
/// 全部请求流经单一分发队列：dispatch 任务是唯一的可变状态 owner，快路径
/// handler（含 `turn/start` 的同步 claim）不阻塞 stdin 读取；claim 接受后
/// 由独立 turn worker 线程执行整条链，控制请求因此始终可按到达顺序处理。
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
    let (output_tx, mut output_rx) = mpsc::channel::<Value>(OUTPUT_QUEUE_CAPACITY);
    let (request_tx, request_rx) = mpsc::channel::<JsonRpcMessage>(REQUEST_QUEUE_CAPACITY);
    let writer_cancellation = cancellation.clone();
    let mut output = writer;
    let mut writer = tokio::spawn(async move {
        write_output_queue(&mut output_rx, &mut output, &writer_cancellation).await
    });
    let mut writer_done = false;
    let mut dispatch_task = tokio::spawn(run_dispatch(
        server,
        request_rx,
        output_tx.clone(),
        cancellation.clone(),
    ));
    let mut dispatch_done = false;
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
            result = &mut dispatch_task, if !dispatch_done => {
                dispatch_done = true;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => terminal_error = Some(error),
                    Err(error) => terminal_error =
                        Some(format!("dispatch task failed: {error}")),
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
                if let Err(error) =
                    route_inbound_line(line, &request_tx, &output_tx, &cancellation).await
                {
                    terminal_error = Some(error);
                    break;
                }
            }
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    drop(request_tx);
    let stop_result = cancellation
        .request_execution_stop()
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"));
    let mut worker_error = None;
    if !dispatch_done {
        // dispatch 在请求队列关闭且在飞 turn worker 全部收敛后退出；宽限
        // 到点 abort。残留的 dispatch 任务持有 output_tx 克隆，不 abort 会
        // 推迟 writer 侧的通道关闭。
        if let Some(error) =
            wait_task_graceful(&mut dispatch_task, shutdown_deadline, "dispatch").await
        {
            worker_error.get_or_insert(error);
        }
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

/// 解析一帧 JSON-Lines 输入并排入唯一分发队列。解析错误和队列满错误都
/// 通过输出队列回复；只有输出队列或分发队列本身关闭时才把错误交回
/// supervisor，由 supervisor 统一进入关闭流程。
async fn route_inbound_line(
    line: String,
    requests: &mpsc::Sender<JsonRpcMessage>,
    output_tx: &mpsc::Sender<Value>,
    cancellation: &AppServerCancellationHandle,
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
    enqueue_request(
        requests,
        message,
        output_tx,
        cancellation,
        "request queue is full",
    )
    .await
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

/// 单个请求在 dispatch 处理任务内的归宿：响应已在任务内发出，或 turn/start
/// claim 被接受、待 dispatcher 启动 turn worker。
enum Handled {
    Done,
    TurnAccepted(TurnStartClaim),
}

/// 唯一分发 owner：按到达顺序处理全部请求（快路径 handler，含 `turn/start`
/// 的同步 claim），Accepted 的 turn 交由独立 worker 线程执行整条链。请求
/// 队列关闭后继续等待在飞 turn worker 收敛，不丢失已完成轮次的输出。
async fn run_dispatch(
    mut server: AppServer,
    mut requests: mpsc::Receiver<JsonRpcMessage>,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let mut turn_tasks: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let mut requests_open = true;
    loop {
        tokio::select! {
            biased;
            Some(join_result) = turn_tasks.join_next(), if !turn_tasks.is_empty() => {
                match join_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(format!("turn worker task failed: {error}")),
                }
            }
            message = requests.recv(), if requests_open => {
                let Some(message) = message else {
                    requests_open = false;
                    continue;
                };
                let handled = tokio::task::spawn_blocking({
                    let outputs = outputs.clone();
                    let cancellation = cancellation.clone();
                    move || {
                        let request_id = message.id().cloned();
                        let handled = if is_turn_request(&message) {
                            match server.claim_turn(message) {
                                Ok(crate::TurnClaim::Accepted(claim)) => {
                                    Ok(Handled::TurnAccepted(claim))
                                }
                                Ok(crate::TurnClaim::Responded(response)) => send_output(
                                    &outputs,
                                    &cancellation,
                                    response,
                                )
                                .map(|_| Handled::Done),
                                Err(error) => send_output(
                                    &outputs,
                                    &cancellation,
                                    transport_error_value(request_id, &error),
                                )
                                .map(|_| Handled::Done),
                            }
                        } else {
                            let notification = message.is_notification();
                            let result = match server.handle_with_output(message) {
                                Ok(messages) => {
                                    send_app_server_outputs(&outputs, &cancellation, messages)
                                }
                                Err(error) if !notification => send_output(
                                    &outputs,
                                    &cancellation,
                                    transport_error_value(request_id, &error),
                                )
                                .map(|_| ()),
                                Err(_) => Ok(()),
                            };
                            result.map(|_| Handled::Done)
                        };
                        (server, handled)
                    }
                })
                .await
                .map_err(|error| format!("request dispatch task failed: {error}"))?;
                let (next_server, handled) = handled;
                server = next_server;
                match handled? {
                    Handled::Done => {}
                    Handled::TurnAccepted(claim) => {
                        // turn_worker 仅克隆共享 Arc，无需 spawn_blocking 线程池跳转。
                        match server.turn_worker() {
                            Ok(worker) => {
                                let worker_outputs = outputs.clone();
                                let worker_cancellation = cancellation.clone();
                                turn_tasks.spawn_blocking(move || {
                                    run_turn_request(
                                        worker,
                                        worker_outputs,
                                        worker_cancellation,
                                        claim,
                                    )
                                });
                            }
                            Err(error) => {
                                send_output_async(
                                    outputs.clone(),
                                    cancellation.clone(),
                                    transport_error_value(Some(claim.request_id), &error),
                                )
                                .await?;
                            }
                        }
                    }
                }
                if server.shutdown_requested() {
                    requests_open = false;
                }
            }
            else => break,
        }
    }
    Ok(())
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
    let provider_snapshot = ProviderConfigSnapshot::capture(runtime_handle);
    Ok(AppServer::new(provider_snapshot, paths.sessions_dir))
}

/// 判断单请求是否需要 turn claim 与独立 turn worker。
fn is_turn_request(message: &JsonRpcMessage) -> bool {
    !message.is_notification()
        && matches!(
            message.method_name(),
            Some(method) if method == Method::TurnStart.as_str()
        )
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
