//! Ordered, bounded stdout output.
//!
//! Output queue ownership, backpressure, and writer lifecycle are kept together.
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

/// 同步上下文（spawn_blocking worker、turn worker）的入队入口：队列满时
/// 以 `blocking_send` 阻塞当前线程形成背压，channel 关闭才触发全局停止。
///
/// 只能在 Tokio 运行时线程之外调用；运行时线程一律走 [`send_output_async`]。
pub(crate) fn send_output(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    message: Value,
) -> Result<(), String> {
    match outputs.blocking_send(message) {
        Ok(()) => Ok(()),
        Err(_) => {
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
