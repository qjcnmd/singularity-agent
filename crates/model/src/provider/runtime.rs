use crate::openai::chat_completions_endpoint;
use crate::provider::contract::ProviderProtocolContract;
use crate::{
    DEFAULT_MAX_TOOLS_PER_REQUEST, ProviderApiProtocol, ProviderToolReasoningMode,
    ThinkingWireFormat,
};
use std::fmt;

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

impl fmt::Debug for OpenAiProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProviderConfig")
            .field("provider_name", &self.provider_name)
            .field("model_name", &self.model_name)
            .field("base_url", &"[redacted]")
            .field("api_key", &"[redacted]")
            .field("max_context_tokens", &self.max_context_tokens)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl OpenAiProviderConfig {
    /// 返回当前请求 endpoint。
    pub fn endpoint(&self) -> String {
        chat_completions_endpoint(&self.base_url)
    }

    /// 返回当前 provider 的能力契约。
    pub fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract {
            supports_tools: true,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_system_message: true,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

/// 一个完全解析的目录选择。把规范变体、启用状态与单一 wire effort 放在
/// 一起，避免第二张运行时映射表悄悄改变 provider 请求。
#[derive(Clone)]
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) reasoning_enabled: bool,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) tool_reasoning_mode: ProviderToolReasoningMode,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
}

/// 一次请求的七个 wire 开关，由 [`SelectedModel`] 投影而来，供 openai 协议
/// builder 统一消费，避免逐项提取散落在传输层。
#[derive(Clone)]
pub(crate) struct WireRequestOptions {
    pub(crate) reasoning_enabled: bool,
    pub(crate) reasoning_disabled: bool,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
}

impl WireRequestOptions {
    /// 从 [`SelectedModel`] 投影七个 wire 开关。
    pub(crate) fn from_selection(selection: &SelectedModel) -> Self {
        Self {
            reasoning_enabled: selection.reasoning_enabled,
            reasoning_disabled: !selection.reasoning_enabled,
            wire_reasoning_effort: selection.wire_reasoning_effort.clone(),
            thinking_wire_format: selection.thinking_wire_format,
            supports_developer_role: selection.supports_developer_role,
            supports_tool_choice: selection.supports_tool_choice,
            requires_assistant_content_for_tool_calls: selection
                .requires_assistant_content_for_tool_calls,
        }
    }
}
