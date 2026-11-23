use std::sync::Arc;

use protocol::{RpcError, RpcRequest, StreamEvent};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
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

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RpcResponse {
    Success { result: Box<RawValue> },
    Error { error: RpcError },
}

const STDOUT_WRITER_CAPACITY: usize = 512;
const STDOUT_WRITE_BATCH_BYTES: usize = 64 * 1024;

pub(crate) async fn run(
    app: Arc<AppServer>,
    mut events: mpsc::Receiver<protocol::StreamEnvelope>,
) -> Result<(), String> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(STDOUT_WRITER_CAPACITY);
    let writer = tokio::spawn(async move {
        let mut output = tokio::io::stdout();
        while let Some(mut bytes) = writer_rx.recv().await {
            while bytes.len() < STDOUT_WRITE_BATCH_BYTES
                && let Ok(more) = writer_rx.try_recv()
            {
                bytes.extend_from_slice(&more);
            }
            output.write_all(&bytes).await.map_err(|e| format!("write desktop pipe: {e}"))?;
            output.flush().await.map_err(|e| format!("flush desktop pipe: {e}"))?;
        }
        Ok::<(), String>(())
    });
    let mut requests = JoinSet::new();
    let shutdown = CancellationToken::new();
    let result = async {
        let ready = encode_line(&protocol::StreamEnvelope {
            revision: app.revision(),
            event: StreamEvent::Ready,
        })?;
        writer_tx.send(ready).await.map_err(|_| "desktop pipe writer closed".to_string())?;
        let mut pending_write: Option<Vec<u8>> = None;
        let mut next_request_seq = 0u64;
        loop {
            tokio::select! {
                biased;
                line = input.next_line() => {
                    let Some(line) = line.map_err(|e| format!("read desktop pipe: {e}"))? else { break };
                    let call: Request = serde_json::from_str(&line)
                        .map_err(|e| format!("invalid desktop pipe envelope: {e}"))?;
                    next_request_seq += 1;
                    let request_seq = next_request_seq;
                    let app = Arc::clone(&app);
                    let shutdown = shutdown.clone();
                    requests.spawn(async move {
                        let response = match serde_json::from_str::<RpcRequest>(call.request.get()) {
                            Ok(request) => rpc::handle(&app, request, request_seq, &shutdown).await,
                            Err(error) => Err(RpcError::new(error)),
                        };
                        let response = match response {
                            Ok(result) => RpcResponse::Success { result },
                            Err(error) => RpcResponse::Error { error },
                        };
                        encode_line(&Response { id: call.id, response })
                    });
                }
                permit = writer_tx.reserve(), if pending_write.is_some() => {
                    let permit = permit.map_err(|_| "desktop pipe writer closed".to_string())?;
                    permit.send(pending_write.take().expect("pending_write is guarded by is_some"));
                }
                result = requests.join_next(), if !requests.is_empty() && pending_write.is_none() => {
                    pending_write = Some(result.expect("nonempty RPC task set")
                        .expect("RPC task completes while the runtime is running")?);
                }
                event = events.recv(), if pending_write.is_none() => {
                    let Some(frame) = event else { break };
                    pending_write = Some(encode_line(&frame)?);
                }
            }
        }
        if let Some(bytes) = pending_write {
            let _ = writer_tx.try_send(bytes);
        }
        Ok(())
    }
    .await;
    shutdown.cancel();
    while let Some(joined) = requests.join_next().await {
        let _ = joined.expect("accepted RPC task completes before shutdown");
    }
    app.shutdown().await;
    if result.is_ok() {
        while let Ok(frame) = events.try_recv() {
            if let Ok(bytes) = encode_line(&frame)
                && writer_tx.try_send(bytes).is_err()
            {
                break;
            }
        }
    }
    drop(writer_tx);
    let write_result = writer.await.expect("desktop pipe writer task completes");
    write_result.and(result)
}

fn encode_line(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}
