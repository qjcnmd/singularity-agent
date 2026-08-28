//! 有序、有界 stdout 输出：输出队列所有权、背压与 writer 生命周期集中管理。
use super::ExecutionStop;
use crate::{AppServerCancellationHandle, AppServerOutput};
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Async 上下文的入队入口：队列满时在 `send().await` 上背压等待，
/// 真实发送失败（writer 已消失）才触发全局停止。
pub(crate) async fn send_output_async(
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
    message: Value,
) -> Result<(), String> {
    match outputs.send(message).await {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = cancellation.request_execution_stop();
            Err("stdout transport unavailable".to_string())
        }
    }
}

/// 同步上下文（spawn_blocking worker、turn worker、stream 回调）的入队入口：
/// 优先尝试无阻塞直接入队；队列满时以背压阻塞（若在 Tokio runtime 线程内则使用 `block_in_place`），
/// channel 关闭才触发全局停止。
pub(crate) fn send_output(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    message: Value,
) -> Result<(), String> {
    match outputs.try_send(message) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(message)) => {
            let send_result = if let Ok(handle) = tokio::runtime::Handle::try_current() {
                match handle.runtime_flavor() {
                    tokio::runtime::RuntimeFlavor::MultiThread => {
                        tokio::task::block_in_place(|| outputs.blocking_send(message))
                    }
                    _ => {
                        let mut msg = message;
                        loop {
                            match outputs.try_send(msg) {
                                Ok(()) => break Ok(()),
                                Err(mpsc::error::TrySendError::Full(m)) => {
                                    msg = m;
                                    std::thread::yield_now();
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    break Err(tokio::sync::mpsc::error::SendError(Value::Null));
                                }
                            }
                        }
                    }
                }
            } else {
                outputs.blocking_send(message)
            };
            match send_result {
                Ok(()) => Ok(()),
                Err(_) => {
                    cancellation.request_execution_stop();
                    Err("stdout transport unavailable".to_string())
                }
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            cancellation.request_execution_stop();
            Err("stdout transport unavailable".to_string())
        }
    }
}

pub(crate) fn send_app_server_outputs(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    messages: Vec<AppServerOutput>,
) -> Result<(), String> {
    for message in messages {
        send_output(outputs, cancellation, message)?;
    }
    Ok(())
}

/// 串行写出所有输出 frame；每个 frame 连同换行一次写出，真实写入或 flush
/// 失败才触发全局停止。
pub(crate) async fn write_output_queue<W: AsyncWrite + Unpin>(
    output_rx: &mut mpsc::Receiver<Value>,
    stdout: &mut W,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    while let Some(message) = output_rx.recv().await {
        let mut frame = match serde_json::to_vec(&message) {
            Ok(frame) => frame,
            Err(error) => {
                cancellation.request_execution_stop();
                return Err(format!("failed to serialize response: {error}"));
            }
        };
        frame.push(b'\n');
        if let Err(error) = stdout.write_all(&frame).await {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.flush().await {
            cancellation.request_execution_stop();
            return Err(format!("failed to flush response: {error}"));
        }
    }
    Ok(())
}
