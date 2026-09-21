//! 固定工作台 RPC 版本的 adapter（版本号只有 protocol 的 PROTOCOL_VERSION
//! 一处来源）；参数形状和错误 envelope 在 transport 边界闭合。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use singularity_protocol::{
    PROTOCOL_VERSION, RpcCall, RpcError, RpcErrorCode, RpcMethod, RpcRequest, RpcResponse, calls,
};

use super::app_server::{AppServer, invalid_request};
use super::directory_picker;
use super::server::ServerState;

pub async fn handle(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.origin.validate_api_source(&headers, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(raw) => raw,
        Err(_) => return invalid_transport_response("", "请求不是有效 JSON。"),
    };
    // 响应只需要这个 ID；请求体（可能含用户文本、工具配置与密钥）不为响应再留副本。
    let request_id = raw
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let request: RpcRequest = match serde_json::from_value(raw) {
        Ok(request) => request,
        Err(error) => {
            return invalid_transport_response(&request_id, &format!("请求合同无效：{error}"));
        }
    };
    let result = if let Some(result) = dispatch_async(&state.app_server, &request).await {
        result
    } else {
        let app_server = Arc::clone(&state.app_server);
        tokio::task::spawn_blocking(move || dispatch(&app_server, &request))
            .await
            .unwrap_or_else(|error| {
                Err(RpcError::new(
                    RpcErrorCode::Internal,
                    format!("工作台操作未完成：{error}"),
                    "刷新状态后重试。",
                ))
            })
    };
    let response = match result {
        Ok(result) => RpcResponse {
            version: PROTOCOL_VERSION,
            request_id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => error_response(request_id, error),
    };
    (StatusCode::OK, axum::Json(response)).into_response()
}

async fn dispatch_async(
    app_server: &Arc<AppServer>,
    request: &RpcRequest,
) -> Option<Result<Value, RpcError>> {
    match request.method {
        RpcMethod::ModelDiscover => Some(
            async {
                let params = parse::<calls::ModelDiscover>(&request.params)?;
                value::<calls::ModelDiscover>(
                    app_server
                        .discover_models(
                            &params.provider_id,
                            &params.base_url,
                            params.api_key.as_deref(),
                        )
                        .await?,
                )
            }
            .await,
        ),
        RpcMethod::DirectoryPick => Some(
            async {
                parse::<calls::DirectoryPick>(&request.params)?;
                value::<calls::DirectoryPick>(directory_picker::pick_directory().await?)
            }
            .await,
        ),
        _ => None,
    }
}

fn dispatch(app_server: &Arc<AppServer>, request: &RpcRequest) -> Result<Value, RpcError> {
    match request.method {
        RpcMethod::AppBootstrap => {
            parse::<calls::AppBootstrap>(&request.params)?;
            value::<calls::AppBootstrap>(app_server.bootstrap()?)
        }
        // 两者由 handle 的异步分支处理，不会进入同步分发；留出口只为新增
        // 异步方法时立刻暴露分发漏接，而不是静默走错路径。
        RpcMethod::DirectoryPick | RpcMethod::ModelDiscover => {
            Err(invalid_request("该操作需要异步分发。"))
        }
        RpcMethod::SkillsList => {
            let params = parse::<calls::SkillsList>(&request.params)?;
            value::<calls::SkillsList>(
                app_server.skills(&params.workspace_id, params.session_id.as_deref())?,
            )
        }
        RpcMethod::FileSearch => {
            let params = parse::<calls::FileSearch>(&request.params)?;
            value::<calls::FileSearch>(app_server.file_search(
                &params.workspace_id,
                params.session_id.as_deref(),
                &params.query,
                params.limit,
            )?)
        }
        RpcMethod::WorkspaceAdd => {
            let params = parse::<calls::WorkspaceAdd>(&request.params)?;
            value::<calls::WorkspaceAdd>(app_server.add_workspace(&params.root)?)
        }
        RpcMethod::WorkspaceRename => {
            let params = parse::<calls::WorkspaceRename>(&request.params)?;
            value::<calls::WorkspaceRename>(
                app_server.rename_workspace(&params.workspace_id, &params.name)?,
            )
        }
        RpcMethod::WorkspaceRemove => {
            let params = parse::<calls::WorkspaceRemove>(&request.params)?;
            value::<calls::WorkspaceRemove>(app_server.remove_workspace(&params.workspace_id)?)
        }
        RpcMethod::ModelSaveProvider => {
            let params = parse::<calls::ModelSaveProvider>(&request.params)?;
            value::<calls::ModelSaveProvider>(
                app_server.save_provider(params.provider, params.api_key.as_deref())?,
            )
        }
        RpcMethod::ModelSetApiKey => {
            let params = parse::<calls::ModelSetApiKey>(&request.params)?;
            value::<calls::ModelSetApiKey>(
                app_server.set_api_key(&params.provider_id, &params.api_key)?,
            )
        }
        RpcMethod::ModelRemoveProvider => {
            let params = parse::<calls::ModelRemoveProvider>(&request.params)?;
            value::<calls::ModelRemoveProvider>(app_server.remove_provider(&params.provider_id)?)
        }
        RpcMethod::SessionCreate => {
            let params = parse::<calls::SessionCreate>(&request.params)?;
            value::<calls::SessionCreate>(app_server.create_session(
                &params.workspace_id,
                params.settings.and_then(|settings| settings.selector),
            )?)
        }
        RpcMethod::SessionRead => {
            let params = parse::<calls::SessionRead>(&request.params)?;
            value::<calls::SessionRead>(app_server.read_session(
                &params.workspace_id,
                &params.session_id,
                params.limit,
                params.before_turn.as_deref(),
            )?)
        }
        RpcMethod::SessionRename => {
            let params = parse::<calls::SessionRename>(&request.params)?;
            value::<calls::SessionRename>(app_server.rename_session(
                &params.workspace_id,
                &params.session_id,
                &params.name,
            )?)
        }
        RpcMethod::SessionArchive => {
            let params = parse::<calls::SessionArchive>(&request.params)?;
            value::<calls::SessionArchive>(
                app_server.archive_session(&params.workspace_id, &params.session_id)?,
            )
        }
        RpcMethod::SessionSubmit => {
            let params = parse::<calls::SessionSubmit>(&request.params)?;
            value::<calls::SessionSubmit>(app_server.submit(
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionSteer => {
            let params = parse::<calls::SessionSteer>(&request.params)?;
            value::<calls::SessionSteer>(app_server.steer(
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionFollowUp => {
            let params = parse::<calls::SessionFollowUp>(&request.params)?;
            value::<calls::SessionFollowUp>(app_server.follow_up(
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueWithdraw => {
            let params = parse::<calls::SessionQueueWithdraw>(&request.params)?;
            value::<calls::SessionQueueWithdraw>(app_server.queue_withdraw(
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
            )?)
        }
        RpcMethod::SessionQueueReplace => {
            let params = parse::<calls::SessionQueueReplace>(&request.params)?;
            value::<calls::SessionQueueReplace>(app_server.queue_replace(
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueSendNow => {
            let params = parse::<calls::SessionQueueSendNow>(&request.params)?;
            value::<calls::SessionQueueSendNow>(app_server.queue_send_now(
                &params.workspace_id,
                &params.session_id,
                params.control_id.as_deref(),
            )?)
        }
        RpcMethod::SessionAbort => {
            let params = parse::<calls::SessionAbort>(&request.params)?;
            value::<calls::SessionAbort>(
                app_server.abort(&params.workspace_id, &params.session_id)?,
            )
        }
        RpcMethod::SessionCompact => {
            let params = parse::<calls::SessionCompact>(&request.params)?;
            value::<calls::SessionCompact>(
                app_server.compact(&params.workspace_id, &params.session_id)?,
            )
        }
        RpcMethod::SessionUpdateSettings => {
            let params = parse::<calls::SessionUpdateSettings>(&request.params)?;
            value::<calls::SessionUpdateSettings>(app_server.update_settings(
                &params.workspace_id,
                &params.session_id,
                &params.selector,
            )?)
        }
    }
}

/// 以借用的 params JSON 作为 Deserializer 直接得到参数对象，不复制整棵中间树。
fn parse<C: RpcCall>(value: &Value) -> Result<C::Params, RpcError> {
    serde::Deserialize::deserialize(value)
        .map_err(|error| invalid_request(format!("参数无效：{error}")))
}

fn value<C: RpcCall>(value: C::Output) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| {
        RpcError::new(
            RpcErrorCode::Internal,
            format!("响应无法序列化：{error}"),
            "刷新工作台后重试。",
        )
    })
}

fn error_response(request_id: String, error: RpcError) -> RpcResponse {
    RpcResponse {
        version: PROTOCOL_VERSION,
        request_id,
        ok: false,
        result: None,
        error: Some(error),
    }
}

fn invalid_transport_response(request_id: &str, message: &str) -> Response {
    let response = error_response(
        request_id.to_string(),
        RpcError::new(RpcErrorCode::InvalidRequest, message, "刷新页面后重试。"),
    );
    (StatusCode::BAD_REQUEST, axum::Json(response)).into_response()
}
