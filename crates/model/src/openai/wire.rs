use crate::types::ModelTurnResponse;
use crate::{CHAT_COMPLETIONS_PATH, RESPONSES_PATH, V1_CHAT_COMPLETIONS_PATH, V1_RESPONSES_PATH};

/// 将基础 URL 解析为兼容 OpenAI 的 Chat Completions 端点。
pub fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(CHAT_COMPLETIONS_PATH) {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{CHAT_COMPLETIONS_PATH}")
    } else {
        format!("{trimmed}{V1_CHAT_COMPLETIONS_PATH}")
    }
}

/// 将基础 URL 解析为兼容 OpenAI 的 Responses 端点。
pub fn responses_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(RESPONSES_PATH) {
        trimmed.to_string()
    } else if let Some(prefix) = trimmed.strip_suffix(CHAT_COMPLETIONS_PATH) {
        format!("{prefix}{RESPONSES_PATH}")
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{RESPONSES_PATH}")
    } else {
        format!("{trimmed}{V1_RESPONSES_PATH}")
    }
}

pub(crate) struct OpenAiCompletion {
    pub(crate) response: ModelTurnResponse,
    pub(crate) reasoning_content_present: bool,
}
