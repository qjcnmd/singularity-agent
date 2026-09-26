//! Private newline-delimited pipe transport. Only correlation IDs wrap the existing protocol.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_protocol::{EmptyParams, RpcRequest, RpcResponse, StreamEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{app_server::AppServer, rpc};
use crate::session_options::DesktopSetup;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: u64,
    request: Value,
}

#[derive(Serialize)]
struct Response {
    id: u64,
    response: RpcResponse,
}

pub async fn run(setup: DesktopSetup) -> Result<(), String> {
    let app = AppServer::new(
        setup.runner,
        tokio::runtime::Handle::current(),
        setup.catalog,
        setup.workspaces,
        setup.models,
        setup.home,
    );
    let mut events = app.subscribe();
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut requests = JoinSet::new();
    let shutdown = CancellationToken::new();
    let result = async {
        write(
            &mut output,
            &app.frame(StreamEvent::Ready {
                payload: EmptyParams {},
            }),
        )
        .await?;
        loop {
            tokio::select! {
                line = input.next_line() => {
                    let Some(line) = line.map_err(|e| format!("read desktop pipe: {e}"))? else { break };
                    let call: Request = serde_json::from_str(&line)
                        .map_err(|e| format!("invalid desktop pipe envelope: {e}"))?;
                    let app = Arc::clone(&app);
                    let shutdown = shutdown.clone();
                    requests.spawn(async move {
                        let response = match serde_json::from_value::<RpcRequest>(call.request) {
                            Ok(request) => rpc::handle(&app, request, &shutdown).await,
                            Err(error) => rpc::error_response(super::app_server::invalid_request(error.to_string())),
                        };
                        Response { id: call.id, response }
                    });
                }
                result = requests.join_next(), if !requests.is_empty() => {
                    let response = result.ok_or("RPC task disappeared")?
                        .map_err(|e| format!("desktop RPC task failed: {e}"))?;
                    write(&mut output, &response).await?;
                }
                event = events.recv() => {
                    let frame = match event {
                        Ok(frame) => frame,
                        Err(broadcast::error::RecvError::Lagged(_)) => app.frame(StreamEvent::ResyncRequired { payload: EmptyParams {} }),
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    write(&mut output, &frame).await?;
                }
            }
        }
        Ok(())
    }.await;
    // EOF 或管道故障都结束接收；先取消目录查询、收齐已接受的修改，再停止会话。
    shutdown.cancel();
    let mut result = result;
    while let Some(joined) = requests.join_next().await {
        if let Err(error) = joined {
            // 继续等待其他已接受的操作，不能因一个 worker 失败而跳过会话结算。
            eprintln!("desktop RPC task failed during shutdown: {error}");
            if result.is_ok() {
                result = Err(format!("desktop RPC task failed during shutdown: {error}"));
            }
        }
    }
    app.shutdown().await;
    result
}

async fn write(output: &mut tokio::io::Stdout, value: &impl Serialize) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    output
        .write_all(&bytes)
        .await
        .map_err(|e| format!("write desktop pipe: {e}"))?;
    output
        .flush()
        .await
        .map_err(|e| format!("flush desktop pipe: {e}"))
}
