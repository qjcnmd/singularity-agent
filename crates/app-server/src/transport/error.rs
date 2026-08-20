//! Transport-to-JSON-RPC error projection.
//!
//! Sensitive diagnostics are redacted at this single output seam.

use serde_json::Value;
use singularity_app_server::AppServerError;
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR, contains_sensitive_text};
use singularity_protocol::{JsonRpcId, JsonRpcMessage};
pub(crate) fn transport_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    let diagnostic = match error {
        AppServerError::TurnExecution { original, .. }
        | AppServerError::TurnTerminalization { original, .. } => {
            original.clone().unwrap_or_else(|| error.to_string())
        }
        other => other.to_string(),
    };
    // 透出真实错误文本供诊断（DB/锁/provider 等）；若文本疑似含密钥则回退脱敏。
    let diagnostic = if contains_sensitive_text(&diagnostic) {
        "Internal error".to_string()
    } else {
        diagnostic
    };
    internal_error_value(id, diagnostic)
}

pub(crate) fn request_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    match error {
        AppServerError::InvalidParams(_) => {
            JsonRpcMessage::error(id, ErrorCode::invalid_params("Invalid params")).to_wire_value()
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
