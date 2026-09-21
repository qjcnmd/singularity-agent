//! OpenAI Chat Completions/Responses 两种协议的请求编码、响应解码与响应结构校验。
//! 具体 Provider（选哪种协议、编排一次调用）见 [`provider`]；HTTP 客户端、SSE 帧切分
//! 与有界读取由 transport 提供。

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

/// 编码边界上挑选「私有续接材料」：只有身份与当前 provider、模型、协议都一致的数据
/// 才写进请求；不一致返回 `None` 时调用方只略过这部分私有载荷，公开内容与账本照常。
/// Chat 与 Responses 共用这一条身份规则，账本消息本身不被复制或改写。
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
