//! Ordered, bounded stdout output.
//!
//! Output queue ownership, backpressure, and writer lifecycle are kept together.
use super::ExecutionStop;
use serde_json::Value;
use singularity_app_server::{AppServerCancellationHandle, AppServerOutput};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

pub(crate) async fn send_output_async(
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
    message: Value,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || send_output(&outputs, &cancellation, message))
        .await
        .map_err(|error| format!("output dispatch task failed: {error}"))?
}

/// 将消息放入唯一输出队列；队列满时阻塞（背压），真实发送失败才触发全局停止。
pub(crate) fn send_output(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    mut message: Value,
) -> Result<(), String> {
    loop {
        match outputs.try_send(message) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Full(next)) => {
                if cancellation.execution_stop_requested() {
                    return Err("stdout transport stopping".to_string());
                }
                message = next;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                cancellation.request_execution_stop();
                return Err("stdout transport unavailable".to_string());
            }
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

/// 串行写出所有输出 frame；真实写入或 flush 失败才触发全局停止。
pub(crate) async fn write_output_queue<W: AsyncWrite + Unpin>(
    output_rx: &mut mpsc::Receiver<Value>,
    stdout: &mut W,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    while let Some(message) = output_rx.recv().await {
        let line = match serde_json::to_vec(&message) {
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
    }
    Ok(())
}
