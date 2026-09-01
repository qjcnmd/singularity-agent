use crate::provider::runtime::OpenAiProviderConfig;
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

/// replay 与请求绑定的公共包络：provider/model 归属、请求时实际 selection
/// 的 reasoning 变体（provider 不回显 effort 时保持 None，不伪造禁用变体），
/// 以及全部工具调用 id（replay 的归属键）。
pub(crate) struct ReplayBinding {
    pub(crate) provider_name: String,
    pub(crate) model_name: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tool_call_ids: Vec<String>,
}

pub(crate) fn replay_binding(
    config: &OpenAiProviderConfig,
    model_name: &str,
    reasoning_effort: Option<&str>,
    tool_calls: &[crate::ModelToolCall],
) -> ReplayBinding {
    ReplayBinding {
        provider_name: config.provider_name.clone(),
        model_name: model_name.to_string(),
        reasoning_effort: reasoning_effort.map(str::to_string),
        tool_call_ids: tool_calls
            .iter()
            .map(|call| call.tool_call_id.clone())
            .collect(),
    }
}
