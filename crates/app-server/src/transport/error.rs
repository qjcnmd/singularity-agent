//! Transport 到 JSON-RPC 的错误投影：单一函数、单一事实源。
//!
//! 错误类到码的映射：客户端参数错误 -32602、
//! 状态冲突标准 invalid-request -32600、资源不存在 -32004，其余
//! 保持 JSON-RPC internal 语义 -32603。

use crate::AppServerError;
use serde_json::Value;
use singularity_protocol::{JSON_RPC_INTERNAL_ERROR, JsonRpcError, JsonRpcId, JsonRpcMessage};

pub(crate) fn error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    let error = match error {
        AppServerError::InvalidParams(message) => JsonRpcError::invalid_params(message.clone()),
        AppServerError::InvalidState(message) => JsonRpcError::invalid_request(message.clone()),
        AppServerError::NotFound(message) => JsonRpcError::not_found(message.clone()),
        AppServerError::TurnExecution { original, .. }
        | AppServerError::TurnTerminalization { original, .. } => JsonRpcError::new(
            JSON_RPC_INTERNAL_ERROR,
            original.clone().unwrap_or_else(|| error.to_string()),
        ),
        other => JsonRpcError::new(JSON_RPC_INTERNAL_ERROR, other.to_string()),
    };
    JsonRpcMessage::error(id, error).to_wire_value()
}
