//! 固定版本 1 RPC adapter；参数形状和错误 envelope 在 transport 边界闭合。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_protocol::{
    ProviderConfigurationInput, RpcError, RpcErrorCode, RpcMethod, RpcRequest, RpcResponse,
    WORKBENCH_PROTOCOL_VERSION,
};

use super::host::HostState;
use super::workbench::{Workbench, WorkbenchError};
use super::workspace_files;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectoryListParams {
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileSearchParams {
    workspace_id: String,
    query: String,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceAddParams {
    root: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceParams {
    workspace_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderSaveParams {
    provider: ProviderConfigurationInput,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiKeyParams {
    provider_id: String,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionSettingsInput {
    selector: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionCreateParams {
    workspace_id: String,
    settings: Option<SessionSettingsInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionReadParams {
    workspace_id: String,
    session_id: String,
    before_turn: Option<String>,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionParams {
    workspace_id: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionTextParams {
    workspace_id: String,
    session_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionRenameParams {
    workspace_id: String,
    session_id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueControlParams {
    workspace_id: String,
    session_id: String,
    control_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueReplaceParams {
    workspace_id: String,
    session_id: String,
    control_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateSettingsParams {
    workspace_id: String,
    session_id: String,
    selector: String,
}

pub async fn handle(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.auth.validate_api_source(&headers, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.auth.has_valid_cookie(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
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
    let result = dispatch(&state.workbench, &request);
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

fn dispatch(workbench: &Arc<Workbench>, request: &RpcRequest) -> Result<Value, WorkbenchError> {
    match request.method {
        RpcMethod::WorkbenchBootstrap => {
            parse::<EmptyParams>(&request.params)?;
            value(workbench.bootstrap()?)
        }
        RpcMethod::DirectoryList => {
            let params = parse::<DirectoryListParams>(&request.params)?;
            value(workspace_files::list_directory(params.path.as_deref()).map_err(invalid_request)?)
        }
        RpcMethod::FileSearch => {
            let params = parse::<FileSearchParams>(&request.params)?;
            if !(1..=100).contains(&params.limit) {
                return Err(invalid_request("limit must be between 1 and 100"));
            }
            let workspace = workbench.workspace(&params.workspace_id)?;
            value(
                workspace_files::search_files(&workspace, &params.query, params.limit)
                    .map_err(invalid_request)?,
            )
        }
        RpcMethod::WorkspaceAdd => {
            let params = parse::<WorkspaceAddParams>(&request.params)?;
            value(workbench.add_workspace(&params.root)?)
        }
        RpcMethod::WorkspaceRemove => {
            let params = parse::<WorkspaceParams>(&request.params)?;
            workbench.remove_workspace(&params.workspace_id)
        }
        RpcMethod::ModelSaveProvider => {
            let params = parse::<ProviderSaveParams>(&request.params)?;
            value(workbench.save_provider(params.provider)?)
        }
        RpcMethod::ModelSetApiKey => {
            let params = parse::<ApiKeyParams>(&request.params)?;
            value(workbench.set_api_key(&params.provider_id, &params.api_key)?)
        }
        RpcMethod::SessionCreate => {
            let params = parse::<SessionCreateParams>(&request.params)?;
            value(workbench.create_session(
                &params.workspace_id,
                params.settings.and_then(|settings| settings.selector),
            )?)
        }
        RpcMethod::SessionRead => {
            let params = parse::<SessionReadParams>(&request.params)?;
            value(workbench.read_session(
                &params.workspace_id,
                &params.session_id,
                params.limit,
                params.before_turn.as_deref(),
            )?)
        }
        RpcMethod::SessionRename => {
            let params = parse::<SessionRenameParams>(&request.params)?;
            value(workbench.rename_session(
                &params.workspace_id,
                &params.session_id,
                &params.name,
            )?)
        }
        RpcMethod::SessionArchive => {
            let params = parse::<SessionParams>(&request.params)?;
            workbench.archive_session(&params.workspace_id, &params.session_id)
        }
        RpcMethod::SessionSubmit => {
            let params = parse::<SessionTextParams>(&request.params)?;
            value(workbench.submit(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionSteer => {
            let params = parse::<SessionTextParams>(&request.params)?;
            value(workbench.steer(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionFollowUp => {
            let params = parse::<SessionTextParams>(&request.params)?;
            value(workbench.follow_up(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueWithdraw => {
            let params = parse::<QueueControlParams>(&request.params)?;
            value(workbench.queue_withdraw(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
            )?)
        }
        RpcMethod::SessionQueueReplace => {
            let params = parse::<QueueReplaceParams>(&request.params)?;
            value(workbench.queue_replace(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
                params.text,
            )?)
        }
        RpcMethod::SessionQueueSendNow => {
            let params = parse::<QueueControlParams>(&request.params)?;
            value(workbench.queue_send_now(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
                &params.control_id,
            )?)
        }
        RpcMethod::SessionAbort => {
            let params = parse::<SessionParams>(&request.params)?;
            value(workbench.abort(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
            )?)
        }
        RpcMethod::SessionCompact => {
            let params = parse::<SessionParams>(&request.params)?;
            value(workbench.compact(
                &request.request_id,
                &params.workspace_id,
                &params.session_id,
            )?)
        }
        RpcMethod::SessionUpdateSettings => {
            let params = parse::<UpdateSettingsParams>(&request.params)?;
            workbench.update_settings(&params.workspace_id, &params.session_id, &params.selector)
        }
    }
}

fn parse<T: DeserializeOwned>(value: &Value) -> Result<T, WorkbenchError> {
    serde_json::from_value(value.clone())
        .map_err(|error| invalid_request(format!("参数无效：{error}")))
}

fn value(value: impl serde::Serialize) -> Result<Value, WorkbenchError> {
    serde_json::to_value(value).map_err(|error| {
        WorkbenchError::new(
            RpcErrorCode::Internal,
            format!("响应无法序列化：{error}"),
            "刷新工作台后重试。",
        )
    })
}

fn invalid_request(message: impl Into<String>) -> WorkbenchError {
    WorkbenchError::new(
        RpcErrorCode::InvalidRequest,
        message,
        "检查请求参数后重试。",
    )
}

fn error_response(workbench: &Workbench, request_id: String, error: WorkbenchError) -> RpcResponse {
    RpcResponse {
        version: WORKBENCH_PROTOCOL_VERSION,
        request_id,
        ok: false,
        generation: workbench.generation().to_string(),
        revision: workbench.revision(),
        result: None,
        error: Some(RpcError {
            code: error.code,
            message: error.message,
            recovery: error.recovery,
            preserved_input: error.preserved_input,
        }),
    }
}

fn invalid_transport_response(workbench: &Workbench, request_id: &str, message: &str) -> Response {
    let response = error_response(
        workbench,
        request_id.to_string(),
        WorkbenchError::new(RpcErrorCode::InvalidRequest, message, "刷新页面后重试。"),
    );
    (StatusCode::BAD_REQUEST, axum::Json(response)).into_response()
}
