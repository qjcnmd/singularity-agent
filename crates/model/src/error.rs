use serde_json::Value;
use std::time::Duration;

/// 提供方失败的处理类别，具体原因保存在错误文字里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelErrorKind {
    Cancelled,
    /// 可自动重试的网络故障、超时、限流和服务端故障。
    Temporary,
    AuthError,
    InvalidRequest,
    ContextLengthExceeded,
    JsonSchemaViolation,
    ContentFilter,
    UnknownProviderError,
}

/// 提供方失败，含错误分类、可显示的诊断信息和重试等待时间。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub kind: ModelErrorKind,
    pub message: String,
    /// 提供方要求的自动重试前最小等待时间。
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// 创建一条带稳定 kind 的提供方错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    /// 构造带分类的诊断错误，把具体原因并进各入口实际显示的错误文本里。
    pub(crate) fn diagnostic(kind: ModelErrorKind, message: impl Into<String>, details: Vec<String>) -> Self {
        let mut message = message.into();
        if !details.is_empty() {
            message.push_str(": ");
            message.push_str(&details.join(", "));
        }
        Self::new(kind, message)
    }

    /// 提供方是否明确拒绝了这次请求的上下文规模（会触发强制压缩）。
    pub fn is_context_overflow(&self) -> bool {
        self.kind == ModelErrorKind::ContextLengthExceeded
    }

    /// 判断这个错误是否允许自动重发同一请求。
    pub fn is_retryable(&self) -> bool {
        self.kind == ModelErrorKind::Temporary
    }

    /// 保留提供方要求的延迟，交给所属的重试策略使用。
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }
}

/// 提供方错误响应体里表示上下文超限的线上错误码；只做全等匹配。
const PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE: &str = "context_length_exceeded";
/// 提供方明确返回的限流错误码，允许自动重试。
const PROVIDER_RATE_LIMIT_EXCEEDED_CODE: &str = "rate_limit_exceeded";
/// 配额耗尽的线上错误码：重试没有意义，归到认证/账务这一类不可重试的错误。
const PROVIDER_INSUFFICIENT_QUOTA_CODE: &str = "insufficient_quota";
/// 附在非 2xx 错误上的提供方诊断文本长度上限（字符数）。
pub(crate) const MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS: usize = 256;

/// 从非 2xx 响应体里解析出的结构化错误字段。
#[derive(Default)]
pub(crate) struct ProviderErrorBodyFields {
    pub(crate) code: Option<String>,
    /// 响应里的 error.type；不用于分类，只附在错误消息里便于排查。
    pub(crate) wire_type: Option<String>,
    pub(crate) message: Option<String>,
}

/// 从提供方的 error 对象（{"code": "...", "type": "...", "message": "..."}）里取结构化
/// 字段；不是对象或字段类型不符，都当作没提供。流内事件、200 载荷里的内嵌错误和非
/// 2xx 响应体共用这一个提取点。
pub(crate) fn provider_error_fields(error: &Value) -> ProviderErrorBodyFields {
    let text = |field: &str| error.get(field).and_then(Value::as_str).map(str::to_string);
    ProviderErrorBodyFields {
        code: text("code"),
        wire_type: text("type"),
        message: text("message"),
    }
}

/// 提供方返回的错误码和类型，截断后附在错误消息里；HTTP 和 SSE 两条入口共用。
pub(crate) fn provider_wire_facts(fields: &ProviderErrorBodyFields) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(code) = fields.code.as_deref().filter(|code| !code.is_empty()) {
        facts.push(format!("provider_error_code={}", bounded_provider_error_diagnostic(code)));
    }
    if let Some(wire_type) = fields.wire_type.as_deref().filter(|wire_type| !wire_type.is_empty()) {
        facts.push(format!("provider_error_type={}", bounded_provider_error_diagnostic(wire_type)));
    }
    facts
}

/// 解析非 2xx 响应体中的 {"error": {"code": "...", "message": "..."}}；没有 error
/// 或 error 不是对象时，各字段都为空。
pub(crate) fn parse_provider_error_body(body: &[u8]) -> ProviderErrorBodyFields {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("error").map(provider_error_fields))
        .unwrap_or_default()
}

/// 线上错误码到具体 kind 的精确映射（全等匹配，不靠文本推断）；没命中就返回 None，分类由调用方决定。
pub(crate) fn provider_error_kind_for_code(code: Option<&str>) -> Option<ModelErrorKind> {
    match code {
        Some(PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE) => Some(ModelErrorKind::ContextLengthExceeded),
        Some(PROVIDER_RATE_LIMIT_EXCEEDED_CODE) => Some(ModelErrorKind::Temporary),
        Some(PROVIDER_INSUFFICIENT_QUOTA_CODE) => Some(ModelErrorKind::AuthError),
        _ => None,
    }
}

/// 超时、限流和服务端故障共用重试策略，具体状态码保留在错误文字里。
pub(crate) fn provider_error_kind_for_http_status(status: u16) -> ModelErrorKind {
    match status {
        crate::HTTP_STATUS_UNAUTHORIZED | crate::HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        crate::HTTP_STATUS_REQUEST_TIMEOUT | crate::HTTP_STATUS_RATE_LIMITED => ModelErrorKind::Temporary,
        status if (400..=499).contains(&status) => ModelErrorKind::InvalidRequest,
        status if (500..=599).contains(&status) => ModelErrorKind::Temporary,
        _ => ModelErrorKind::UnknownProviderError,
    }
}

/// 有长度上限的单行提供方诊断：控制字符和空白都并成一个空格，再截断到上限。
pub(crate) fn bounded_provider_error_diagnostic(text: &str) -> String {
    let mut diagnostic = String::new();
    let mut remaining = MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS;
    let mut separator_pending = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            // 第一个词之前的空白丢掉；之后的空白只当作下一次写入前的单个分隔符。
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

/// 从线上 message 取出的、有长度上限的非空诊断；回退内容由协议调用方决定。
pub(crate) fn bounded_wire_detail(fields: &ProviderErrorBodyFields) -> Option<String> {
    fields
        .message
        .as_deref()
        .map(bounded_provider_error_diagnostic)
        .filter(|text| !text.is_empty())
}

/// 构造内嵌的提供方错误（来自流内事件或 200 载荷）：已知线上错误码映射到对应的
/// kind（上下文溢出触发强制压缩、限流保持可重试、配额归入不可重试的认证类），
/// 未知码保持 UnknownProviderError，不自动重试，并带上提供方原文和码。
pub(crate) fn provider_embedded_error(
    fields: &ProviderErrorBodyFields,
    fallback_message: &str,
) -> ProviderError {
    let kind =
        provider_error_kind_for_code(fields.code.as_deref()).unwrap_or(ModelErrorKind::UnknownProviderError);
    let message = bounded_wire_detail(fields).unwrap_or_else(|| fallback_message.to_string());
    let details = provider_wire_facts(fields);
    ProviderError::diagnostic(kind, message, details)
}
