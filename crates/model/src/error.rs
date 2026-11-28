use serde_json::Value;
use std::fmt::Write as _;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelErrorKind {
    Cancelled,
    Temporary,
    ContextLengthExceeded,
    Other,
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    pub(crate) fn diagnostic(
        kind: ModelErrorKind,
        message: impl Into<String>,
        details: &[impl AsRef<str>],
    ) -> Self {
        let mut message = message.into();
        if !details.is_empty() {
            message.push_str(": ");
            for (index, detail) in details.iter().enumerate() {
                if index > 0 {
                    message.push_str(", ");
                }
                message.push_str(detail.as_ref());
            }
        }
        Self::new(kind, message)
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == ModelErrorKind::Cancelled
    }

    pub fn is_context_overflow(&self) -> bool {
        self.kind == ModelErrorKind::ContextLengthExceeded
    }

    pub fn is_retryable(&self) -> bool {
        self.kind == ModelErrorKind::Temporary
    }
}

const PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE: &str = "context_length_exceeded";
const PROVIDER_RATE_LIMIT_EXCEEDED_CODE: &str = "rate_limit_exceeded";
const PROVIDER_SERVER_ERROR_CODE: &str = "server_error";
const PROVIDER_INSUFFICIENT_QUOTA_CODE: &str = "insufficient_quota";
const INVALID_REQUEST_OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "maximum context length",
    "exceed context limit",
    "exceeds the context window",
    "context_length_exceeded",
    "too many tokens",
];
pub(crate) const MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS: usize = 256;

#[derive(Default)]
pub(crate) struct ProviderErrorBodyFields {
    pub(crate) code: Option<String>,
    pub(crate) wire_type: Option<String>,
    pub(crate) message: Option<String>,
}

pub(crate) fn provider_error_fields(error: &Value) -> ProviderErrorBodyFields {
    let text = |field: &str| error.get(field).and_then(Value::as_str).map(str::to_string);
    ProviderErrorBodyFields {
        code: text("code"),
        wire_type: text("type"),
        message: text("message"),
    }
}

pub(crate) fn provider_wire_facts(fields: &ProviderErrorBodyFields, api_key: &str) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(code) = fields.code.as_deref().filter(|code| !code.is_empty()) {
        facts.push(format!("provider_error_code={}", bounded_redacted_diagnostic(code, api_key)));
    }
    if let Some(wire_type) = fields.wire_type.as_deref().filter(|wire_type| !wire_type.is_empty()) {
        facts.push(format!("provider_error_type={}", bounded_redacted_diagnostic(wire_type, api_key)));
    }
    facts
}

fn parse_provider_error_body(body: &[u8]) -> ProviderErrorBodyFields {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("error").map(provider_error_fields))
        .unwrap_or_default()
}

pub(crate) fn apply_provider_error_body(error: &mut ProviderError, body: &[u8], api_key: &str) {
    let fields = parse_provider_error_body(body);
    if let Some(kind) = provider_error_kind_for_fields(&fields) {
        error.kind = kind;
    }
    let facts = provider_wire_facts(&fields, api_key);
    if !facts.is_empty() {
        let _ = write!(error.message, " ({})", facts.join(", "));
    }
    let diagnostic = match fields.message.as_deref() {
        Some(message) => bounded_redacted_diagnostic(message, api_key),
        None => bounded_redacted_diagnostic(&String::from_utf8_lossy(body), api_key),
    };
    if !diagnostic.is_empty() {
        let _ = write!(error.message, " Provider diagnostic: {diagnostic}");
    }
}

fn provider_error_kind_for_fields(fields: &ProviderErrorBodyFields) -> Option<ModelErrorKind> {
    match fields.code.as_deref() {
        Some(PROVIDER_CONTEXT_LENGTH_EXCEEDED_CODE) => return Some(ModelErrorKind::ContextLengthExceeded),
        Some(PROVIDER_RATE_LIMIT_EXCEEDED_CODE | PROVIDER_SERVER_ERROR_CODE) => {
            return Some(ModelErrorKind::Temporary);
        }
        Some(PROVIDER_INSUFFICIENT_QUOTA_CODE) => return Some(ModelErrorKind::Other),
        _ => {}
    }
    match fields.wire_type.as_deref() {
        Some("rate_limit_error" | "overloaded_error" | "api_error" | "server_error") => {
            Some(ModelErrorKind::Temporary)
        }
        Some(PROVIDER_INSUFFICIENT_QUOTA_CODE) => Some(ModelErrorKind::Other),
        Some("invalid_request_error") => fields.message.as_deref().and_then(|message| {
            let lower = message.to_ascii_lowercase();
            INVALID_REQUEST_OVERFLOW_PATTERNS
                .iter()
                .any(|pattern| lower.contains(pattern))
                .then_some(ModelErrorKind::ContextLengthExceeded)
        }),
        _ => None,
    }
}

pub(crate) fn provider_error_kind_for_http_status(status: u16) -> ModelErrorKind {
    match status {
        408 | 409 | 429 => ModelErrorKind::Temporary,
        status if (500..=599).contains(&status) => ModelErrorKind::Temporary,
        _ => ModelErrorKind::Other,
    }
}

pub(crate) fn bounded_provider_error_diagnostic(text: &str) -> String {
    let mut diagnostic = String::new();
    let mut remaining = MAX_PROVIDER_ERROR_DIAGNOSTIC_CHARS;
    let mut separator_pending = false;
    for character in text.chars() {
        if character.is_control() || character.is_whitespace() {
            separator_pending = !diagnostic.is_empty();
            continue;
        }
        if separator_pending {
            if remaining < 2 {
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

pub(crate) fn redact_diagnostic(text: &str, api_key: &str) -> String {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    if !api_key.is_empty() {
        for (start, _) in text.match_indices(api_key) {
            spans.push((start, start + api_key.len()));
        }
    }
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 6 < bytes.len() {
        if bytes[i..i + 6].eq_ignore_ascii_case(b"bearer") && bytes[i + 6].is_ascii_whitespace() {
            let word_start =
                text[..i].chars().next_back().is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
            if word_start {
                let mut token_start = i + 6;
                while token_start < bytes.len() && bytes[token_start].is_ascii_whitespace() {
                    token_start += 1;
                }
                let token_end = text[token_start..]
                    .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '}' | ']' | ';'))
                    .map_or(text.len(), |offset| token_start + offset);
                if token_end > token_start {
                    spans.push((token_start, token_end));
                    i = token_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if let Some((_, prev_end)) = merged.last_mut()
            && start < *prev_end
        {
            *prev_end = (*prev_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        out.push_str(&text[cursor..start]);
        out.push_str("[redacted]");
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

pub(crate) fn bounded_redacted_diagnostic(text: &str, api_key: &str) -> String {
    let redacted = redact_diagnostic(text, api_key);
    bounded_provider_error_diagnostic(&redacted)
}

pub(crate) fn provider_embedded_error(
    fields: &ProviderErrorBodyFields,
    fallback_message: &str,
    api_key: &str,
) -> ProviderError {
    let kind = provider_error_kind_for_fields(fields).unwrap_or(ModelErrorKind::Other);
    let message = fields
        .message
        .as_deref()
        .map(|text| bounded_redacted_diagnostic(text, api_key))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback_message.to_string());
    let details = provider_wire_facts(fields, api_key);
    ProviderError::diagnostic(kind, message, &details)
}
