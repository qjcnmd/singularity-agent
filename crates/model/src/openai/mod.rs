//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。
//!
//! 具体 Provider（协议选择与一次调用编排）也在本包内，见 [`provider`]；
//! 传输能力（HTTP client、SSE 帧、有界读取）由 transport 提供。

pub(crate) mod chat;
pub(crate) mod parse;
pub(crate) mod provider;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::{openai_chat_stream_request_payload, read_chat_sse_stream};
pub use provider::OpenAiProvider;
pub(crate) use responses::{openai_responses_stream_request_payload, read_responses_sse_stream};
pub(crate) use wire::{
    api_root, canonical_base_url, chat_completions_endpoint, models_endpoint, responses_endpoint,
};

use crate::config::selection::SelectedModel;
use crate::types::{ModelMessage, ProviderReasoningReplay};

/// 编码边界上的私有续接选择：只有身份等于当前 provider/model/协议的数据才进入
/// wire；不匹配时返回 `None`，调用方只省略私有载荷，公开内容仍按账本发送。
///
/// Chat 与 Responses 两个 encoder 共用这一条身份规则。账本消息不被复制或改写，
/// 被筛掉的续接材料仍留在会话里。
pub(crate) fn reasoning_replay_for<'a>(
    message: &'a ModelMessage,
    selection: &SelectedModel,
    provider_name: &str,
) -> Option<&'a ProviderReasoningReplay> {
    message.provider_reasoning_replay.as_ref().filter(|replay| {
        replay.is_for_model(provider_name, &selection.model_name, selection.api_protocol)
    })
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection
            .reasoning_variant
            .as_ref()
            .map(|_| selection.reasoning_enabled),
        effort: selection.wire_reasoning_effort.as_deref(),
    }
}
