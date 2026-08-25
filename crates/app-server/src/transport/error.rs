//! Transport-to-JSON-RPC error projection.

use serde_json::Value;
use singularity_app_server::AppServerError;
use singularity_protocol::{ErrorCode, JSON_RPC_INTERNAL_ERROR, JsonRpcId, JsonRpcMessage};
pub(crate) fn transport_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    let diagnostic = match error {
        AppServerError::TurnExecution { original, .. }
        | AppServerError::TurnTerminalization { original, .. } => {
            original.clone().unwrap_or_else(|| error.to_string())
        }
        other => other.to_string(),
    };
    internal_error_value(id, diagnostic)
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
