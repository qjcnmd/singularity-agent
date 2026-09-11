//! 固定版本 1 RPC adapter；参数形状和错误 envelope 在 transport 边界闭合。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use singularity_protocol::{
    RpcCall, RpcError, RpcErrorCode, RpcMethod, RpcRequest, RpcResponse,
    WORKBENCH_PROTOCOL_VERSION, calls,
};

use super::host::HostState;
use super::workbench::{Workbench, invalid_request};
use super::workspace_files;

pub async fn handle(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.origin.validate_api_source(&headers, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(raw) => raw,
        Err(_) => return invalid_transport_response(&state.workbench, "", "请求不是有效 JSON。"),
    };
    let request_id = raw
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let request: RpcRequest = match serde_json::from_value(raw) {
        Ok(request) => request,
        Err(error) => {
            return invalid_transport_response(
                &state.workbench,
                &request_id,
                &format!("请求合同无效：{error}"),
            );
        }
    };
    let result = if request.method == RpcMethod::ModelDiscover {
        match parse::<calls::ModelDiscover>(&request.params) {
            Ok(params) => state
                .workbench
                .discover_models(
                    &params.provider_id,
                    &params.base_url,
                    params.api_key.as_deref(),
                )
                .await
                .and_then(value::<calls::ModelDiscover>),
            Err(error) => Err(error),
        }
    } else if request.method == RpcMethod::DirectoryPick {
        match parse::<calls::DirectoryPick>(&request.params) {
            Ok(_) => workspace_files::pick_directory()
                .await
                .and_then(value::<calls::DirectoryPick>),
            Err(error) => Err(error),
        }
    } else {
        let workbench = Arc::clone(&state.workbench);
        let dispatch_request = request.clone();
        tokio::task::spawn_blocking(move || dispatch(&workbench, &dispatch_request))
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
            version: WORKBENCH_PROTOCOL_VERSION,
            request_id: request.request_id,
            ok: true,
            generation: state.workbench.generation().to_string(),
            revision: state.workbench.revision(),
            result: Some(result),
            error: None,
        },
        Err(error) => error_response(&state.workbench, request.request_id, error),
    };
    (StatusCode::OK, axum::Json(response)).into_response()
}

fn dispatch(workbench: &Arc<Workbench>, request: &RpcRequest) -> Result<Value, RpcError> {
    match request.method {
        RpcMethod::WorkbenchBootstrap => {
            parse::<calls::WorkbenchBootstrap>(&request.params)?;
            value::<calls::WorkbenchBootstrap>(workbench.bootstrap()?)
        }
        RpcMethod::DirectoryPick => Err(invalid_request("directory.pick requires async dispatch")),
        RpcMethod::DirectoryList => {
            let params = parse::<calls::DirectoryList>(&request.params)?;
            value::<calls::DirectoryList>(
                workspace_files::list_directory(params.path.as_deref()).map_err(invalid_request)?,
            )
        }
        RpcMethod::SkillsList => {
            let params = parse::<calls::SkillsList>(&request.params)?;
            value::<calls::SkillsList>(
                workbench.skills(&params.workspace_id, params.session_id.as_deref())?,
            )
        }
        RpcMethod::FileSearch => {
            let params = parse::<calls::FileSearch>(&request.params)?;
            if !(1..=100).contains(&params.limit) {
                return Err(invalid_request("limit must be between 1 and 100"));
            }
            let root = match params.session_id {
                Some(id) => workbench.session_directory(&params.workspace_id, &id)?,
                None => workbench.workspace(&params.workspace_id)?.root,
            };
            value::<calls::FileSearch>(
                workspace_files::search_files(&root, &params.query, params.limit)
                    .map_err(invalid_request)?,
            )
        }
        RpcMethod::WorkspaceAdd => {
            let params = parse::<calls::WorkspaceAdd>(&request.params)?;
            value::<calls::WorkspaceAdd>(workbench.add_workspace(&params.root)?)
        }
        RpcMethod::WorkspaceRename => {
            let params = parse::<calls::WorkspaceRename>(&request.params)?;
            value::<calls::WorkspaceRename>(
                workbench.rename_workspace(&params.workspace_id, &params.name)?,
            )
        }
        RpcMethod::WorkspaceRemove => {
            let params = parse::<calls::WorkspaceRemove>(&request.params)?;
            value::<calls::WorkspaceRemove>(workbench.remove_workspace(&params.workspace_id)?)
        }
        RpcMethod::ModelSaveProvider => {
            let params = parse::<calls::ModelSaveProvider>(&request.params)?;
            value::<calls::ModelSaveProvider>(workbench.save_provider(params.provider)?)
        }
        RpcMethod::ModelSetApiKey => {
            let params = parse::<calls::ModelSetApiKey>(&request.params)?;
            value::<calls::ModelSetApiKey>(
                workbench.set_api_key(&params.provider_id, &params.api_key)?,
            )
        }
        RpcMethod::ModelDiscover => Err(invalid_request("模型查询需要异步请求。")),
        RpcMethod::ModelRemoveProvider => {
            let params = parse::<calls::ModelRemoveProvider>(&request.params)?;
            value::<calls::ModelRemoveProvider>(workbench.remove_provider(&params.provider_id)?)
        }
        RpcMethod::SessionCreate => {
            let params = parse::<calls::SessionCreate>(&request.params)?;
            value::<calls::SessionCreate>(workbench.create_session(
                &params.workspace_id,
                params.settings.and_then(|settings| settings.selector),
            )?)
        }
        RpcMethod::SessionRead => {
            let params = parse::<calls::SessionRead>(&request.params)?;
            value::<calls::SessionRead>(workbench.read_session(
                &params.workspace_id,
                &params.session_id,
                params.limit,
                params.before_turn.as_deref(),
            )?)
        }
        RpcMethod::SessionRequest => {
            let params = parse::<calls::SessionRequest>(&request.params)?;
            value::<calls::SessionRequest>(workbench.request_details(
                &params.workspace_id,
                &params.session_id,
                &params.request_id,
            )?)
        }
        RpcMethod::SessionRename => {
            let params = parse::<calls::SessionRename>(&request.params)?;
            value::<calls::SessionRename>(workbench.rename_session(
                &params.workspace_id,
                &params.session_id,
                &params.name,
            )?)
        }
        RpcMethod::SessionArchive => {
            let params = parse::<calls::SessionArchive>(&request.params)?;
            value::<calls::SessionArchive>(
                workbench.archive_session(&params.workspace_id, &params.session_id)?,
            )
        }
        RpcMethod::SessionSubmit => {
            let params = parse::<calls::SessionSubmit>(&request.params)?;
            value::<calls::SessionSubmit>(workbench.submit(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionSteer => {
            let params = parse::<calls::SessionSteer>(&request.params)?;
            value::<calls::SessionSteer>(workbench.steer(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionFollowUp => {
            let params = parse::<calls::SessionFollowUp>(&request.params)?;
            value::<calls::SessionFollowUp>(workbench.follow_up(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueWithdraw => {
            let params = parse::<calls::SessionQueueWithdraw>(&request.params)?;
            value::<calls::SessionQueueWithdraw>(workbench.queue_withdraw(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
            )?)
        }
        RpcMethod::SessionQueueReplace => {
            let params = parse::<calls::SessionQueueReplace>(&request.params)?;
            value::<calls::SessionQueueReplace>(workbench.queue_replace(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueSendNow => {
            let params = parse::<calls::SessionQueueSendNow>(&request.params)?;
            value::<calls::SessionQueueSendNow>(workbench.queue_send_now(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
            )?)
        }
        RpcMethod::SessionAbort => {
            let params = parse::<calls::SessionAbort>(&request.params)?;
            value::<calls::SessionAbort>(workbench.abort(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
            )?)
        }
        RpcMethod::SessionCompact => {
            let params = parse::<calls::SessionCompact>(&request.params)?;
            value::<calls::SessionCompact>(workbench.compact(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
            )?)
        }
        RpcMethod::SessionUpdateSettings => {
            let params = parse::<calls::SessionUpdateSettings>(&request.params)?;
            value::<calls::SessionUpdateSettings>(workbench.update_settings(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.selector,
            )?)
        }
    }
}

fn parse<C: RpcCall>(value: &Value) -> Result<C::Params, RpcError> {
    serde_json::from_value(value.clone())
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

fn error_response(workbench: &Workbench, request_id: String, error: RpcError) -> RpcResponse {
    RpcResponse {
        version: WORKBENCH_PROTOCOL_VERSION,
        request_id,
        ok: false,
        generation: workbench.generation().to_string(),
        revision: workbench.revision(),
        result: None,
        error: Some(error),
    }
}

fn invalid_transport_response(workbench: &Workbench, request_id: &str, message: &str) -> Response {
    let response = error_response(
        workbench,
        request_id.to_string(),
        RpcError::new(RpcErrorCode::InvalidRequest, message, "刷新页面后重试。"),
    );
    (StatusCode::BAD_REQUEST, axum::Json(response)).into_response()
}
