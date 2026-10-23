//! 工作台 RPC 的适配层：具体参数在入口解析，操作结果在这里编码。

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use singularity_protocol::{RpcError, RpcErrorCode, RpcRequest, RpcResponse};

use super::app_server::{AppServer, invalid_request};

pub async fn handle(
    app: &Arc<AppServer>,
    request: RpcRequest,
    shutdown: &tokio_util::sync::CancellationToken,
) -> RpcResponse {
    let result = async {
        if let RpcRequest::McpReconnect(params) = request {
            let cwd = app.mcp_directory(params.workspace_id.as_deref())?;
            let inspect = async { app.mcp.reconnect(&params.server_id, &cwd).await.map_err(invalid_request) };
            return value(unless_shutdown(shutdown, inspect).await?);
        }
        if let RpcRequest::ModelDiscover(params) = request {
            let discover =
                app.discover_models(&params.provider_id, &params.base_url, params.api_key.as_deref());
            return value(unless_shutdown(shutdown, discover).await?);
        }
        if let RpcRequest::BackgroundTaskStop(params) = request {
            return value(app.stop_background_task(&params.session_id, &params.task_id).await?);
        }
        if let RpcRequest::WorkspaceRemove(params) = request {
            return value(app.remove_workspace(&params.workspace_id).await?);
        }
        if let RpcRequest::SessionDelete(params) = request {
            return value(app.delete_session(&params.session_id).await?);
        }
        let app = Arc::clone(app);
        tokio::task::spawn_blocking(move || dispatch(&app, request))
            .await
            .expect("RPC worker completes while the backend is running")
    }
    .await;
    match result {
        Ok(result) => RpcResponse::Success { result },
        Err(error) => RpcResponse::Error { error },
    }
}

/// 网络查询在退出时立即结束，不拖住关闭流程。
async fn unless_shutdown<T>(
    shutdown: &tokio_util::sync::CancellationToken,
    work: impl Future<Output = Result<T, RpcError>>,
) -> Result<T, RpcError> {
    tokio::select! {
        result = work => result,
        () = shutdown.cancelled() => {
            Err(RpcError::new(RpcErrorCode::Internal, "工作台正在退出。", "重新启动桌面应用。"))
        }
    }
}

fn dispatch(app: &Arc<AppServer>, request: RpcRequest) -> Result<Value, RpcError> {
    match request {
        RpcRequest::AppBootstrap(_) => value(app.bootstrap()?),
        // 目录选择由 Electron 接收；网络查询和删除等待由 handle 异步执行。
        RpcRequest::DirectoryPick(_)
        | RpcRequest::ModelDiscover(_)
        | RpcRequest::McpReconnect(_)
        | RpcRequest::BackgroundTaskStop(_)
        | RpcRequest::WorkspaceRemove(_)
        | RpcRequest::SessionDelete(_) => Err(invalid_request("该方法不由同步 AppServer 分发。")),
        RpcRequest::SkillsList(params) => value(app.skills(&params.workspace_id)?),
        RpcRequest::FileSearch(params) => value(app.file_search(&params.workspace_id, &params.query)?),
        RpcRequest::WorkspaceAdd(params) => value(app.add_workspace(&params.root)?),
        RpcRequest::ModelSaveProvider(params) => {
            value(app.save_provider(params.provider, params.api_key.as_deref())?)
        }
        RpcRequest::ModelRemoveProvider(params) => value(app.remove_provider(&params.provider_id)?),
        RpcRequest::McpList(params) => {
            let cwd = app.mcp_directory(params.workspace_id.as_deref())?;
            value(app.mcp.list(&cwd).map_err(invalid_request)?)
        }
        RpcRequest::McpSave(params) => {
            let cwd = app.mcp_directory(params.workspace_id.as_deref())?;
            value(app.mcp.save(params.server, &cwd).map_err(invalid_request)?)
        }
        RpcRequest::McpRemove(params) => {
            let cwd = app.mcp_directory(params.workspace_id.as_deref())?;
            value(app.mcp.remove(&params.server_id, &cwd).map_err(invalid_request)?)
        }
        RpcRequest::McpToggle(params) => {
            let cwd = app.mcp_directory(params.workspace_id.as_deref())?;
            value(app.mcp.toggle(&params.server_id, params.enabled, &cwd).map_err(invalid_request)?)
        }
        RpcRequest::SessionStart(params) => value(app.start_session(
            &params.workspace_id,
            params.selection,
            input(params.text, params.images)?,
        )?),
        RpcRequest::SessionRead(params) => value(app.read_session(&params.session_id)?),
        RpcRequest::SessionImageRead(params) => value(app.image_data(&params.session_id, &params.image_id)?),
        RpcRequest::SessionRename(params) => value(app.rename_session(&params.session_id, &params.name)?),
        RpcRequest::SessionSubmit(params) => {
            value(app.submit(&params.session_id, input(params.text, params.images)?)?)
        }
        RpcRequest::SessionAnswerQuestion(params) => {
            value(app.answer_question(&params.session_id, &params.item_id, params.answers)?)
        }
        RpcRequest::SessionAbort(params) => value(app.abort(&params.session_id)?),
        RpcRequest::SessionCompact(params) => value(app.compact(&params.session_id)?),
        RpcRequest::SessionUpdateSettings(params) => {
            value(app.update_settings(&params.session_id, &params.selection)?)
        }
    }
}

fn input(
    text: String,
    images: Vec<singularity_protocol::ImageUpload>,
) -> Result<singularity_agent::agent::UserInput, RpcError> {
    singularity_agent::agent::UserInput::from_uploads(text, images)
        .map_err(|message| invalid_request(&message))
}

fn value(output: impl Serialize) -> Result<Value, RpcError> {
    Ok(serde_json::to_value(output).expect("RPC output contains JSON-compatible protocol values"))
}
