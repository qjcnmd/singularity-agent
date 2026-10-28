//! 工作台 RPC 的适配层：每个方法只分发一次，阻塞操作进入工作线程。
use super::app_server::{AppServer, invalid_request};
use super::file_view::read_text_file;
use serde::Serialize;
use serde_json::Value;
use singularity_protocol::{RpcError, RpcErrorCode, RpcRequest, RpcResponse};
use std::sync::Arc;

pub async fn handle(
    app: &Arc<AppServer>,
    request: RpcRequest,
    shutdown: &tokio_util::sync::CancellationToken,
) -> RpcResponse {
    match dispatch(Arc::clone(app), request, shutdown).await {
        Ok(result) => RpcResponse::Success { result },
        Err(error) => RpcResponse::Error { error },
    }
}

async fn dispatch(
    app: Arc<AppServer>,
    request: RpcRequest,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Value, RpcError> {
    match request {
        RpcRequest::McpReconnect(params) => {
            let cwd = app.mcp_directory(params.workspace_root.as_deref())?;
            let inspect = async { app.mcp.reconnect(&params.server_id, &cwd).await.map_err(invalid_request) };
            value(unless_shutdown(shutdown, inspect).await?)
        }
        RpcRequest::ModelDiscover(params) => {
            let discover =
                app.discover_models(&params.provider_id, &params.base_url, params.api_key.as_deref());
            value(unless_shutdown(shutdown, discover).await?)
        }
        RpcRequest::BackgroundTaskStop(params) => {
            value(app.stop_background_task(&params.session_id, &params.task_id).await?)
        }
        RpcRequest::WorkspaceRemove(params) => value(app.remove_workspace(&params.workspace_root).await?),
        RpcRequest::SessionDelete(params) => value(app.delete_session(&params.session_id).await?),
        RpcRequest::SessionContext(params) => value(app.inspect_context(&params.session_id, shutdown).await?),
        RpcRequest::DirectoryPick(_) => Err(invalid_request("目录选择由 Electron 处理。")),
        RpcRequest::AppBootstrap(_) => blocking(move || app.bootstrap()).await,
        RpcRequest::SkillsList(params) => blocking(move || app.skills(&params.workspace_root)).await,
        RpcRequest::InstructionsRead(params) => {
            blocking(move || app.read_instructions(params.workspace_root.as_deref())).await
        }
        RpcRequest::InstructionsSave(params) => {
            blocking(move || app.save_instructions(params.workspace_root.as_deref(), &params.content)).await
        }
        RpcRequest::FileRead(params) => blocking(move || read_text_file(&params.cwd, &params.path)).await,
        RpcRequest::WorkspaceAdd(params) => blocking(move || app.add_workspace(&params.root)).await,
        RpcRequest::ModelRead(_) => blocking(move || Ok(app.read_models())).await,
        RpcRequest::ModelSaveProvider(params) => {
            blocking(move || app.save_provider(params.provider, params.api_key.as_deref())).await
        }
        RpcRequest::ModelRemoveProvider(params) => {
            blocking(move || app.remove_provider(&params.provider_id)).await
        }
        RpcRequest::McpList(params) => {
            blocking(move || {
                let cwd = app.mcp_directory(params.workspace_root.as_deref())?;
                app.mcp.list(&cwd).map_err(invalid_request)
            })
            .await
        }
        RpcRequest::McpSave(params) => {
            blocking(move || {
                let cwd = app.mcp_directory(params.workspace_root.as_deref())?;
                app.mcp.save(params.server, &cwd).map_err(invalid_request)
            })
            .await
        }
        RpcRequest::McpRemove(params) => {
            blocking(move || {
                let cwd = app.mcp_directory(params.workspace_root.as_deref())?;
                app.mcp.remove(&params.server_id, &cwd).map_err(invalid_request)
            })
            .await
        }
        RpcRequest::ContextPreview(params) => {
            blocking(move || app.context_preview(&params.workspace_root)).await
        }
        RpcRequest::SessionStart(params) => blocking(move || app.start_session(params)).await,
        RpcRequest::SessionRead(params) => blocking(move || app.read_session(&params.session_id)).await,
        RpcRequest::SessionImageRead(params) => {
            blocking(move || app.image_data(&params.session_id, &params.image_id)).await
        }
        RpcRequest::SessionRename(params) => {
            blocking(move || app.rename_session(&params.session_id, &params.name)).await
        }
        RpcRequest::SessionSubmit(params) => blocking(move || app.submit(params)).await,
        RpcRequest::SessionAnswerQuestion(params) => {
            blocking(move || app.answer_question(&params.session_id, &params.item_id, params.answers)).await
        }
        RpcRequest::SessionAbort(params) => blocking(move || app.abort(&params.session_id)).await,
        RpcRequest::SessionCompact(params) => blocking(move || app.compact(&params.session_id)).await,
        RpcRequest::SessionUpdateSettings(params) => {
            blocking(move || app.update_settings(&params.session_id, &params.selection)).await
        }
    }
}

async fn blocking<T: Serialize>(
    operation: impl FnOnce() -> Result<T, RpcError> + Send + 'static,
) -> Result<Value, RpcError> {
    tokio::task::spawn_blocking(move || value(operation()?))
        .await
        .expect("RPC worker completes while the backend is running")
}

/// 网络查询在退出时立即结束，不拖住关闭流程。
async fn unless_shutdown<T>(
    shutdown: &tokio_util::sync::CancellationToken,
    work: impl Future<Output = Result<T, RpcError>>,
) -> Result<T, RpcError> {
    tokio::select! {
        result = work => result,
        () = shutdown.cancelled() => Err(RpcError::new(RpcErrorCode::Internal, "工作台正在退出。")),
    }
}

fn value(output: impl Serialize) -> Result<Value, RpcError> {
    Ok(serde_json::to_value(output).expect("RPC output contains JSON-compatible protocol values"))
}
