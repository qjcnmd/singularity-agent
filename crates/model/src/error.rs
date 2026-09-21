use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// 从模型提供方边界保留下来的具体失败类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Cancelled,
    NetworkError,
    Timeout,
    RateLimited,
    ProviderOverloaded,
    AuthError,
    InvalidRequest,
    ContextLengthExceeded,
    JsonSchemaViolation,
    ContentFilter,
    UnknownProviderError,
}

/// 供调用方决定状态和恢复行为的较粗错误类别。请求观测与持久
/// provider_attempt 的错误词形共用同一 Display 投影（serde snake_case 单源）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCategory {
    Cancelled,
    Authentication,
    Network,
    ModelConfiguration,
    InvalidRequest,
    ContextLengthExceeded,
    JsonSchema,
    ContentFilter,
    ProviderUnavailable,
    UnknownProviderError,
}

impl std::fmt::Display for ModelErrorCategory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&singularity_protocol::wire_word(self))
    }
}

/// 提供方配置已落盘、API 密钥写入失败：配置可用但凭据缺失，可重试保存。
pub const CREDENTIAL_SAVE_FAILED_CODE: &str = "provider_credential_save_failed";
/// 提供方配置已删除、API 密钥删除失败：选择已失效但凭据文件仍留旧值，可重试删除。
pub const CREDENTIAL_DELETE_FAILED_CODE: &str = "provider_credential_delete_failed";

/// 提供方配置未通过校验。
pub(crate) const PROVIDER_CONFIGURATION_INVALID_CODE: &str = "provider_configuration_invalid";

/// 模型提供方失败，包含分类、可显示诊断和自动重试约束。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub code: Option<String>,
    /// provider 定向的自动重试前最小延迟。
    pub retry_after: Option<Duration>,
    /// 调用方是否可自动重发同一逻辑请求。
    pub automatic_retry_allowed: bool,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// 创建带稳定 kind 的模型提供方错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            retry_after: None,
            automatic_retry_allowed: true,
        }
    }

    /// 附加供事件与诊断使用的稳定错误码。
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// 构造分类诊断，将具体原因纳入各入口实际显示的错误文本。
    pub(crate) fn diagnostic(
        kind: ModelErrorKind,
        message: impl Into<String>,
        code: impl Into<String>,
        details: Vec<String>,
    ) -> Self {
        let mut message = message.into();
        if !details.is_empty() {
            message.push_str(": ");
            message.push_str(&details.join(", "));
        }
        Self::new(kind, message).with_code(code)
    }

    /// 归类为公共模型错误类别。
    pub fn category(&self) -> ModelErrorCategory {
        match self.kind {
            ModelErrorKind::Cancelled => ModelErrorCategory::Cancelled,
            ModelErrorKind::AuthError => ModelErrorCategory::Authentication,
            ModelErrorKind::NetworkError | ModelErrorKind::Timeout => ModelErrorCategory::Network,
            ModelErrorKind::InvalidRequest
                if matches!(
                    self.code.as_deref(),
                    Some("provider_configuration_missing" | PROVIDER_CONFIGURATION_INVALID_CODE)
                ) =>
            {
                ModelErrorCategory::ModelConfiguration
            }
            ModelErrorKind::InvalidRequest => ModelErrorCategory::InvalidRequest,
            ModelErrorKind::ContextLengthExceeded => ModelErrorCategory::ContextLengthExceeded,
            ModelErrorKind::JsonSchemaViolation => ModelErrorCategory::JsonSchema,
            ModelErrorKind::ContentFilter => ModelErrorCategory::ContentFilter,
            ModelErrorKind::RateLimited | ModelErrorKind::ProviderOverloaded => {
                ModelErrorCategory::ProviderUnavailable
            }
            ModelErrorKind::UnknownProviderError => ModelErrorCategory::UnknownProviderError,
        }
    }

    /// provider 是否明确拒绝请求的上下文规模（触发强制压缩路径）。
    pub fn is_context_overflow(&self) -> bool {
        self.kind == ModelErrorKind::ContextLengthExceeded
    }

    /// 判断是否允许自动重发同一请求。
    pub fn is_retryable(&self) -> bool {
        use ModelErrorKind::*;
        self.automatic_retry_allowed
            && matches!(
                self.kind,
                RateLimited | NetworkError | Timeout | ProviderOverloaded | UnknownProviderError
            )
    }

    /// 为所属重试策略保留 provider 定向延迟。
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// 标记该失败不可自动重放。
    pub fn without_automatic_retry(mut self) -> Self {
        self.automatic_retry_allowed = false;
        self
    }
}

/// Provider 错误响应体中精确表示上下文超限的 wire 错误码；匹配必须是全等，不做模糊推断。
const PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE: &str = "context_length_exceeded";
/// 限流类 wire 码：保持可重试分型（与状态码分型同归 RateLimited）。
const PROVIDER_RATE_LIMIT_EXCEEDED_CODE: &str = "rate_limit_exceeded";
/// 配额耗尽 wire 码：重试无意义，归入认证/账务类不可重试分型。
const PROVIDER_INSUFFICIENT_QUOTA_CODE: &str = "insufficient_quota";
/// 附加到非 2xx 错误的 provider 诊断文本上界（字符数）。
pub(crate) const MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS: usize = 256;

/// 非 2xx 响应体解析出的结构化错误字段。
#[derive(Default)]
pub(crate) struct ProviderErrorBodyFields {
    pub(crate) code: Option<String>,
    /// wire 的 error.type：与 code 一样是服务端原始事实，分类不据此猜测，但保留。
    pub(crate) wire_type: Option<String>,
    pub(crate) message: Option<String>,
}

/// 从 provider 的 error 对象（{"code": "...", "type": "...", "message": "..."}）
/// 提取结构化字段；非对象或字段类型不符时一律视为未提供。流内事件、200 载荷
/// 内嵌错误与非 2xx 响应体共用这一个提取点。
pub(crate) fn provider_error_fields(error: &Value) -> ProviderErrorBodyFields {
    let text = |field: &str| error.get(field).and_then(Value::as_str).map(str::to_string);
    ProviderErrorBodyFields {
        code: text("code"),
        wire_type: text("type"),
        message: text("message"),
    }
}

/// 服务端原始协议事实的有界条目：HTTP 状态与 wire code/type 都保留，分类结果
/// （kind/code）不因它们改变；HTTP 与 SSE 两条入口共用同一保留规则。空字段不
/// 产生条目，正文只取有界诊断，绝不保存原始响应体或凭据。
pub(crate) fn provider_wire_facts(
    status: Option<u16>,
    fields: &ProviderErrorBodyFields,
) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(status) = status {
        facts.push(format!("HTTP {status}"));
    }
    if let Some(code) = fields.code.as_deref().filter(|code| !code.is_empty()) {
        facts.push(format!(
            "provider_error_code={}",
            bounded_provider_error_diagnostic(code)
        ));
    }
    if let Some(wire_type) = fields
        .wire_type
        .as_deref()
        .filter(|wire_type| !wire_type.is_empty())
    {
        facts.push(format!(
            "provider_error_type={}",
            bounded_provider_error_diagnostic(wire_type)
        ));
    }
    facts
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

pub(crate) fn provider_error_kind_for_transport(error: &reqwest::Error) -> ModelErrorKind {
    if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    }
}

/// HTTP 状态到错误类别的共同定义：401/403 认证、408 超时、429 限流、其余
/// 4xx 输入错误、5xx 提供方过载，其他未知。调用方只对自己确实不同的状态码
/// 在本地显式处理，不改写共同部分。
pub(crate) fn provider_error_kind_for_http_status(status: u16) -> ModelErrorKind {
    match status {
        crate::HTTP_STATUS_UNAUTHORIZED | crate::HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        crate::HTTP_STATUS_REQUEST_TIMEOUT => ModelErrorKind::Timeout,
        crate::HTTP_STATUS_RATE_LIMITED => ModelErrorKind::RateLimited,
        status if (400..=499).contains(&status) => ModelErrorKind::InvalidRequest,
        status if (500..=599).contains(&status) => ModelErrorKind::ProviderOverloaded,
        _ => ModelErrorKind::UnknownProviderError,
    }
}

/// 有界单行 provider 诊断：控制字符与空白合并为单个空格后截断到上限。
/// 只生成实际返回的文本，不构造全文与词数组副本。
pub(crate) fn bounded_provider_error_diagnostic(text: &str) -> String {
    let mut diagnostic = String::new();
    let mut remaining = MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS;
    let mut separator_pending = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            // 首个词之前的空白丢弃；其后的空白只作为下一次写入前的单个分隔符。
            separator_pending = !diagnostic.is_empty();
            continue;
        }
        if separator_pending {
            if remaining == 0 {
                break;
            }
            diagnostic.push(' ');
            remaining -= 1;
            separator_pending = false;
        }
        if remaining == 0 {
            break;
        }
        diagnostic.push(character);
        remaining -= 1;
    }
    diagnostic
}

/// wire message 的有界非空诊断；兜底由协议调用方决定。
pub(crate) fn bounded_wire_detail(fields: &ProviderErrorBodyFields) -> Option<String> {
    fields
        .message
        .as_deref()
        .map(bounded_provider_error_diagnostic)
        .filter(|text| !text.is_empty())
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
    let message = bounded_wire_detail(fields).unwrap_or_else(|| fallback_message.to_string());
    let details = provider_wire_facts(None, fields);
    ProviderError::diagnostic(kind, message, diagnostic_code, details)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // 测试断言惯例
    use super::*;

    #[test]
    fn shared_http_status_classification_covers_the_common_cases() {
        use ModelErrorKind::*;
        for (status, expected) in [
            (400, InvalidRequest),
            (401, AuthError),
            (403, AuthError),
            (404, InvalidRequest),
            (408, Timeout),
            (409, InvalidRequest),
            (429, RateLimited),
            (499, InvalidRequest),
            (500, ProviderOverloaded),
            (599, ProviderOverloaded),
            (600, UnknownProviderError),
            (302, UnknownProviderError),
        ] {
            assert_eq!(
                provider_error_kind_for_http_status(status),
                expected,
                "status {status}"
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

    /// 有界诊断的对外承诺：控制字符与空白归并为单个空格、去掉首尾空白，
    /// 分隔符与正文一起截断到上限。
    #[test]
    fn bounded_diagnostic_collapses_whitespace_and_stops_at_the_limit() {
        assert_eq!(bounded_provider_error_diagnostic(""), "");
        assert_eq!(bounded_provider_error_diagnostic("  \t\n\u{a0} "), "");
        assert_eq!(
            bounded_provider_error_diagnostic("a\u{0}b\u{7f}c\nd"),
            "a b c d"
        );
        assert_eq!(
            bounded_provider_error_diagnostic("\u{4e2d}\u{6587} \u{3000}\u{7a7a}\u{767d}"),
            "\u{4e2d}\u{6587} \u{7a7a}\u{767d}"
        );
        assert_eq!(
            bounded_provider_error_diagnostic(&"x".repeat(4 * MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS)),
            "x".repeat(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS)
        );
        // 分隔符占用最后一个位置时写入空格后停止；正文满格时不再写入分隔符。
        let long = |head: usize| format!("{} NEXT", "y".repeat(head));
        assert_eq!(
            bounded_provider_error_diagnostic(&long(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS - 1)),
            format!("{} ", "y".repeat(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS - 1))
        );
        assert_eq!(
            bounded_provider_error_diagnostic(&long(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS)),
            "y".repeat(MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS)
        );
    }
}
