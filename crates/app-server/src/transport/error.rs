//! Transport 到 JSON-RPC 的错误投影。

use crate::AppServerError;
use serde_json::Value;
use singularity_protocol::{ErrorCode, JSON_RPC_INTERNAL_ERROR, JsonRpcId, JsonRpcMessage};
pub(crate) fn transport_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    // not_found 是本边界唯一的非 internal 错误类：thread 缺失是可寻址的
    // 客户端错误，其余保持 JSON-RPC internal 语义。
    let message = match error {
        AppServerError::NotFound(message) => {
            return JsonRpcMessage::error(id, ErrorCode::not_found(message.clone()))
                .to_wire_value();
        }
        AppServerError::TurnExecution { original, .. }
        | AppServerError::TurnTerminalization { original, .. } => {
            original.clone().unwrap_or_else(|| error.to_string())
        }
        other => other.to_string(),
    };
    internal_error_value(id, message)
}

pub(crate) fn request_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    match error {
        AppServerError::InvalidParams(message) => {
            JsonRpcMessage::error(id, ErrorCode::invalid_params(message.clone())).to_wire_value()
        }
        error => transport_error_value(id, error),
    }
}

pub(crate) fn internal_error_value(id: Option<JsonRpcId>, diagnostic: impl Into<String>) -> Value {
    JsonRpcMessage::error(
        id,
        ErrorCode::new(JSON_RPC_INTERNAL_ERROR, diagnostic.into()),
    )
    .to_wire_value()
}
