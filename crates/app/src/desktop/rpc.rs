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
        if let RpcRequest::ModelDiscover(params) = request {
            let models = tokio::select! {
                result = app.discover_models(
                    &params.provider_id, &params.base_url, params.api_key.as_deref(), &params.api_protocol,
                ) => result?,
                () = shutdown.cancelled() => return Err(RpcError::new(
                    RpcErrorCode::Internal, "工作台正在退出。", "重新启动桌面应用。",
                )),
            };
            return value(models);
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

fn dispatch(app: &Arc<AppServer>, request: RpcRequest) -> Result<Value, RpcError> {
    match request {
        RpcRequest::AppBootstrap(_) => value(app.bootstrap()?),
        // 目录选择由 Electron 接收；模型发现由 handle 异步执行。
        RpcRequest::DirectoryPick(_) | RpcRequest::ModelDiscover(_) => {
            Err(invalid_request("该方法不由同步 AppServer 分发。"))
        }
        RpcRequest::SkillsList(params) => value(app.skills(&params.workspace_id)?),
        RpcRequest::FileSearch(params) => {
            value(app.file_search(&params.workspace_id, &params.query, params.limit)?)
        }
        RpcRequest::WorkspaceAdd(params) => value(app.add_workspace(&params.root)?),
        RpcRequest::WorkspaceRename(params) => {
            value(app.rename_workspace(&params.workspace_id, &params.name)?)
        }
        RpcRequest::WorkspaceRemove(params) => value(app.remove_workspace(&params.workspace_id)?),
        RpcRequest::ModelSaveProvider(params) => {
            value(app.save_provider(params.provider, params.api_key.as_deref())?)
        }
        RpcRequest::ModelSetApiKey(params) => {
            value(app.set_api_key(&params.provider_id, &params.api_key)?)
        }
        RpcRequest::ModelRemoveProvider(params) => value(app.remove_provider(&params.provider_id)?),
        RpcRequest::SessionCreate(params) => value(app.create_session(&params.workspace_id)?),
        RpcRequest::SessionRead(params) => value(app.read_session(
            &params.session_id,
            params.limit,
            params.before_turn.as_deref(),
        )?),
        RpcRequest::SessionRename(params) => {
            value(app.rename_session(&params.session_id, &params.name)?)
        }
        RpcRequest::SessionArchive(params) => value(app.archive_session(&params.session_id)?),
        RpcRequest::SessionSubmit(params) => value(app.submit(&params.session_id, params.text)?),
        RpcRequest::SessionSteer(params) => value(app.steer(&params.session_id, params.text)?),
        RpcRequest::SessionFollowUp(params) => {
            value(app.follow_up(&params.session_id, params.text)?)
        }
        RpcRequest::SessionQueueWithdraw(params) => {
            value(app.queue_withdraw(&params.session_id, &params.control_id)?)
        }
        RpcRequest::SessionQueueReplace(params) => {
            value(app.queue_replace(&params.session_id, &params.control_id, params.text)?)
        }
        RpcRequest::SessionQueueSendNow(params) => {
            value(app.queue_send_now(&params.session_id, params.control_id.as_deref())?)
        }
        RpcRequest::SessionAbort(params) => value(app.abort(&params.session_id)?),
        RpcRequest::SessionCompact(params) => value(app.compact(&params.session_id)?),
        RpcRequest::SessionUpdateSettings(params) => {
            value(app.update_settings(&params.session_id, &params.selector)?)
        }
    }
}

fn value(output: impl Serialize) -> Result<Value, RpcError> {
    Ok(serde_json::to_value(output).expect("RPC output contains JSON-compatible protocol values"))
}
