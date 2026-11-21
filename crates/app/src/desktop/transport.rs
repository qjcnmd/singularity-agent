//! Electron 与后端之间的按行 JSON 管道传输，通过 `id` 关联请求与响应。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use singularity_protocol::{RpcError, RpcRequest, RpcResponse, StreamEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{app_server::AppServer, rpc};

#[derive(Deserialize)]
struct Request {
    id: u64,
    request: Box<RawValue>,
}

#[derive(Serialize)]
struct Response {
    id: u64,
    response: RpcResponse,
}

const STDOUT_WRITER_CAPACITY: usize = 512;

pub async fn run(
    app: Arc<AppServer>,
    mut events: mpsc::Receiver<singularity_protocol::StreamEnvelope>,
) -> Result<(), String> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(STDOUT_WRITER_CAPACITY);
    // 独立有界写任务隔离 stdout 背压，确保大输出期间 abort/RPC 仍可及时进入且内存有上界。
    let writer = tokio::spawn(async move {
        let mut output = tokio::io::stdout();
        while let Some(bytes) = writer_rx.recv().await {
            output.write_all(&bytes).await.map_err(|e| format!("write desktop pipe: {e}"))?;
            while let Ok(more) = writer_rx.try_recv() {
                output.write_all(&more).await.map_err(|e| format!("write desktop pipe: {e}"))?;
            }
            output.flush().await.map_err(|e| format!("flush desktop pipe: {e}"))?;
        }
        Ok::<(), String>(())
    });
    let mut requests = JoinSet::new();
    let shutdown = CancellationToken::new();
    let result = async {
        send_line(
            &writer_tx,
            &singularity_protocol::StreamEnvelope {
                revision: app.revision(),
                event: StreamEvent::Ready,
            },
        )
        .await?;
        let mut pending_write: Option<Vec<u8>> = None;
        loop {
            tokio::select! {
                biased;
                line = input.next_line() => {
                    let Some(line) = line.map_err(|e| format!("read desktop pipe: {e}"))? else { break };
                    let call: Request = serde_json::from_str(&line)
                        .map_err(|e| format!("invalid desktop pipe envelope: {e}"))?;
                    let app = Arc::clone(&app);
                    let shutdown = shutdown.clone();
                    requests.spawn(async move {
                        let response = match serde_json::from_str::<RpcRequest>(call.request.get()) {
                            Ok(request) => rpc::handle(&app, request, &shutdown).await,
                            Err(error) => RpcResponse::Error { error: RpcError::new(error) },
                        };
                        Response { id: call.id, response }
                    });
                }
                permit = writer_tx.reserve(), if pending_write.is_some() => {
                    let permit = permit.map_err(|_| "desktop pipe writer closed".to_string())?;
                    permit.send(pending_write.take().expect("pending_write is guarded by is_some"));
                }
                result = requests.join_next(), if !requests.is_empty() && pending_write.is_none() => {
                    let response = result.expect("nonempty RPC task set")
                        .expect("RPC task completes while the runtime is running");
                    pending_write = Some(encode_line(&response)?);
                }
                event = events.recv(), if pending_write.is_none() => {
                    let Some(frame) = event else { break };
                    pending_write = Some(encode_line(&frame)?);
                }
            }
        }
        if let Some(bytes) = pending_write {
            let _ = writer_tx.send(bytes).await;
        }
        Ok(())
    }
    .await;
    // EOF 或管道故障都结束接收；先取消目录查询、收齐已接受的修改，再停止会话。
    shutdown.cancel();
    while let Some(joined) = requests.join_next().await {
        joined.expect("accepted RPC task completes before shutdown");
    }
    app.shutdown().await;
    while let Ok(frame) = events.try_recv() {
        let _ = send_line(&writer_tx, &frame).await;
    }
    drop(writer_tx);
    let write_result = writer.await.expect("desktop pipe writer task completes");
    result.and(write_result)
}

fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

async fn send_line(writer_tx: &mpsc::Sender<Vec<u8>>, value: &impl Serialize) -> Result<(), String> {
    let bytes = encode_line(value)?;
    writer_tx.send(bytes).await.map_err(|_| "desktop pipe writer closed".to_string())
}
