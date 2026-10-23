//! OpenAI Chat Completions 和 Responses 两种协议的请求编码、响应解码和检查。
//! 选择协议并发送一次请求见 [`provider`]；HTTP 客户端和 SSE 分帧在 transport。

pub(crate) mod chat;
pub(crate) mod parse;
pub(crate) mod provider;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::{openai_chat_stream_request_payload, read_chat_sse_stream};
pub use provider::OpenAiProvider;
pub(crate) use responses::{openai_responses_stream_request_payload, read_responses_sse_stream};
pub(crate) use wire::{canonical_base_url, chat_completions_endpoint, models_endpoint, responses_endpoint};

use crate::config::selection::SelectedModel;
use crate::types::{Message, ProviderMessageMetadata};

/// 消息里提供方私有的续接数据（加密思考、签名等），只有提供方、模型、协议和请求地址
/// 都与生成时相同才发送；不同时返回 `None`，调用方只省略这部分，正文照常发送。
/// Chat 和 Responses 共用这条规则，日志里的消息不会被修改。
pub(crate) fn message_metadata_for<'a>(
    message: &'a Message,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Option<&'a ProviderMessageMetadata> {
    message.provider_metadata().filter(|replay| {
        replay.is_for_model(provider_name, &selection.model_name, selection.api_protocol, endpoint)
    })
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection.reasoning_variant.as_ref().map(|variant| variant != "off"),
        effort: selection.reasoning_variant.as_deref().filter(|variant| !matches!(*variant, "off" | "on")),
    }
}
