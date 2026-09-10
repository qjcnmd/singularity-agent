use std::future::Future;

use reqwest::Response;
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError};
use crate::{
    HTTP_STATUS_CONFLICT, HTTP_STATUS_FORBIDDEN, HTTP_STATUS_INTERNAL_SERVER_ERROR,
    HTTP_STATUS_NOT_FOUND, HTTP_STATUS_RATE_LIMITED, HTTP_STATUS_REQUEST_TIMEOUT,
    HTTP_STATUS_UNAUTHORIZED, MAX_PROVIDER_RESPONSE_BODY_BYTES,
};

pub(crate) fn provider_error_from_http_status(status: u16) -> ProviderError {
    let kind = match status {
        HTTP_STATUS_UNAUTHORIZED | HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        HTTP_STATUS_REQUEST_TIMEOUT => ModelErrorKind::Timeout,
        HTTP_STATUS_CONFLICT | HTTP_STATUS_RATE_LIMITED => ModelErrorKind::RateLimited,
        HTTP_STATUS_NOT_FOUND => ModelErrorKind::InvalidRequest,
        status if status >= HTTP_STATUS_INTERNAL_SERVER_ERROR => ModelErrorKind::ProviderOverloaded,
        status if (400..=499).contains(&status) => ModelErrorKind::InvalidRequest,
        _ => ModelErrorKind::UnknownProviderError,
    };
    let message = format!("Provider returned HTTP {status}.");
    ProviderError::new(kind, message).with_code("provider_http_status")
}

/// Provider 错误响应体中精确表示上下文超限的 wire 错误码；匹配必须是全等，不做模糊推断。
const PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE: &str = "context_length_exceeded";
/// 限流类 wire 码：保持可重试分型（与状态码分型同归 RateLimited）。
const PROVIDER_RATE_LIMIT_EXCEEDED_CODE: &str = "rate_limit_exceeded";
/// 配额耗尽 wire 码：重试无意义，归入认证/账务类不可重试分型。
const PROVIDER_INSUFFICIENT_QUOTA_CODE: &str = "insufficient_quota";
/// 附加到非 2xx 错误的 provider 诊断文本上界（字符数）。
const MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS: usize = 256;

/// 非 2xx 响应体解析出的结构化错误字段。
#[derive(Default)]
pub(crate) struct ProviderErrorBodyFields {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// 从 provider 的 error 对象（{"code": "...", "message": "..."}）提取结构化
/// 字段；非对象或字段类型不符时一律视为未提供。流内事件、200 载荷内嵌错误
/// 与非 2xx 响应体共用这一个提取点。
pub(crate) fn provider_error_fields(error: &Value) -> ProviderErrorBodyFields {
    ProviderErrorBodyFields {
        code: error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// 解析非 2xx 响应体的 {"error": {"code": "...", "message": "..."}} 形状。
/// 顶层缺失或 error 非对象时一律视为未提供。
pub(crate) fn parse_provider_error_body(body: &[u8]) -> ProviderErrorBodyFields {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("error").map(provider_error_fields))
        .unwrap_or_default()
}

/// wire 错误码到类型化 kind 的精确映射（全等匹配，不做文本推断）；
/// 未命中返回 None，由调用方决定兜底分型。
pub(crate) fn provider_error_kind_for_code(code: Option<&str>) -> Option<ModelErrorKind> {
    match code {
        Some(PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE) => Some(ModelErrorKind::ContextLengthExceeded),
        Some(PROVIDER_RATE_LIMIT_EXCEEDED_CODE) => Some(ModelErrorKind::RateLimited),
        Some(PROVIDER_INSUFFICIENT_QUOTA_CODE) => Some(ModelErrorKind::AuthError),
        _ => None,
    }
}

/// 有界单行 provider 诊断：控制字符与空白合并为单个空格后截断到上限。
pub(crate) fn bounded_provider_error_diagnostic(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let collapsed = flattened
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    collapsed
        .chars()
        .take(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS)
        .collect()
}

/// 内嵌 provider 错误（流内事件或 200 载荷）的类型化构造：已知 wire 码
/// 映射到对应 kind（上下文溢出触发强制压缩、限流保持可重试、配额归入不可重试的认证类），未知码保持
/// UnknownProviderError（可重试）但携带 provider 原文与码，绝不静默丢弃。
pub(crate) fn provider_embedded_error(
    fields: &ProviderErrorBodyFields,
    fallback_message: &str,
    diagnostic_code: &'static str,
) -> ProviderError {
    let kind = provider_error_kind_for_code(fields.code.as_deref())
        .unwrap_or(ModelErrorKind::UnknownProviderError);
    let message = fields
        .message
        .as_deref()
        .map(bounded_provider_error_diagnostic)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback_message.to_string());
    let details = fields
        .code
        .as_deref()
        .map(|code| {
            format!(
                "provider_error_code={}",
                bounded_provider_error_diagnostic(code)
            )
        })
        .into_iter()
        .collect();
    ProviderError::diagnostic(kind, message, diagnostic_code, details)
}

pub(super) fn provider_transport_error(error: reqwest::Error, code: &'static str) -> ProviderError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    };
    let message = format!("provider transport failed: {}", error.without_url());
    ProviderError::new(kind, message).with_code(code)
}

pub(super) fn provider_client_initialization_error(error: reqwest::Error) -> ProviderError {
    provider_transport_error(error, "provider_client_initialization_failed")
}

pub(super) fn provider_cancelled_error() -> ProviderError {
    ProviderError::new(ModelErrorKind::Cancelled, "provider request cancelled")
        .with_code("provider_request_cancelled")
}

pub(crate) fn provider_reasoning_history_error(message: &'static str) -> ProviderError {
    ProviderError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_code("provider_reasoning_history_invalid")
}

pub(crate) fn block_on_provider_future<C, F, T>(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    error_code: &'static str,
    create_future: C,
) -> Result<T, ProviderError>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let _runtime_context = runtime.enter();
    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }
    let mut future = Box::pin(create_future());
    runtime.block_on(async {
        tokio::select! {
            _ = cancellation.cancelled_notified() => Err(provider_cancelled_error()),
            result = &mut future => result
                .map_err(|error| provider_transport_error(error, error_code)),
        }
    })
}

pub(crate) fn read_bounded_provider_response_body(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    mut response: Response,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_body_too_large_error());
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            || response.chunk(),
        )?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_body_too_large_error());
        }
        body.extend_from_slice(&chunk);
    }
}

pub(super) fn provider_response_body_too_large_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response body exceeded the fixed safety limit",
    )
    .with_code("provider_response_body_too_large")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_status_codes_map_to_retryable_kinds() {
        // 仅 408/409/429 与 ≥500 可重试。
        for status in [408, 409, 429, 500, 503, 599] {
            let error = provider_error_from_http_status(status);
            assert!(
                error.is_retryable(),
                "status {status} mapped to {:?} should be retryable",
                error.kind
            );
        }
    }

    #[test]
    fn client_error_status_codes_are_not_retryable() {
        for status in [400, 401, 403, 404, 413, 422, 499] {
            let error = provider_error_from_http_status(status);
            assert!(
                !error.is_retryable(),
                "status {status} mapped to {:?} should not be retryable",
                error.kind
            );
        }
    }

    #[test]
    fn wire_error_codes_map_to_typed_kinds() {
        assert_eq!(
            provider_error_kind_for_code(Some(PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE)),
            Some(ModelErrorKind::ContextLengthExceeded)
        );
        assert_eq!(
            provider_error_kind_for_code(Some(PROVIDER_RATE_LIMIT_EXCEEDED_CODE)),
            Some(ModelErrorKind::RateLimited)
        );
        assert_eq!(
            provider_error_kind_for_code(Some(PROVIDER_INSUFFICIENT_QUOTA_CODE)),
            Some(ModelErrorKind::AuthError)
        );
        assert_eq!(
            provider_error_kind_for_code(Some("context_length_exceededx")),
            None
        );
        assert_eq!(provider_error_kind_for_code(None), None);
    }

    #[test]
    #[allow(clippy::expect_used)] // 测试断言惯例
    fn embedded_error_preserves_provider_message_and_code() {
        let payload = serde_json::json!({
            "error": {"code": "context_length_exceeded", "message": "input is too long"}
        });
        let fields = provider_error_fields(payload.get("error").expect("error"));
        let error = provider_embedded_error(&fields, "fallback text", "chat_stream_error");
        assert_eq!(error.kind, ModelErrorKind::ContextLengthExceeded);
        assert!(error.to_string().contains("input is too long"));
        assert!(error.is_context_overflow());
        assert!(!error.is_retryable());
        assert!(
            error
                .to_string()
                .contains("provider_error_code=context_length_exceeded")
        );
    }
}
