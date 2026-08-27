use crate::config::schema::{validate_model_id, validate_provider_identifier};
use crate::config::{
    ResolvedProviderValues, missing_provider_config_error, parse_provider_limit,
    provider_source_missing_error, validate_base_url, validate_provider_value,
};
use crate::error::{ModelError, ModelErrorKind, ProviderErrorStage};
use crate::openai::chat_completions_endpoint;
use crate::provider::contract::ProviderProtocolContract;
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TOOLS_PER_REQUEST,
    DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL, ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS,
    ENV_MODEL, ENV_PROVIDER, MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS,
    ProviderApiProtocol, ProviderConfigSource, ProviderError, ProviderToolReasoningMode,
    RESPONSES_PATH, ThinkingWireFormat,
};
use std::fmt;

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
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
            .field("source", &self.source)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl OpenAiProviderConfig {
    pub(crate) fn from_resolved_values(
        values: ResolvedProviderValues,
    ) -> Result<Self, ProviderError> {
        validate_provider_value(values.provider_name.as_deref(), ENV_PROVIDER, values.source)?;
        validate_provider_value(values.model_name.as_deref(), ENV_MODEL, values.source)?;
        if let Some(provider_name) = values.provider_name.as_deref() {
            validate_provider_identifier(provider_name, ENV_PROVIDER)?;
        }
        if let Some(model_name) = values.model_name.as_deref() {
            validate_model_id(model_name, ENV_MODEL)?;
        }
        validate_base_url(values.base_url.as_deref(), values.source)?;
        validate_provider_value(values.api_key.as_deref(), ENV_API_KEY, values.source)?;
        validate_provider_value(
            values.context_tokens.as_deref(),
            ENV_CONTEXT_TOKENS,
            values.source,
        )?;
        validate_provider_value(
            values.max_output_tokens.as_deref(),
            ENV_MAX_OUTPUT_TOKENS,
            values.source,
        )?;
        let source = values.source;
        // 与用户配置路径同源：未显式给限时按 catalog 解析模型限额（provider
        // 标识取 env 可用值，缺省落 DEFAULT_PROVIDER_NAME），catalog 无条目
        // 由 resolve_model_limits 落保守默认；env 模式本就无目录保证，不报错。
        let (catalog_context, catalog_output) = match values.model_name.as_deref() {
            Some(model_name) => {
                let provider_name =
                    values.provider_name.as_deref().unwrap_or(DEFAULT_PROVIDER_NAME);
                crate::catalog::resolve_model_limits(provider_name, model_name)
            }
            None => (DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS),
        };
        let max_context_limit = parse_provider_limit(
            values.context_tokens.as_deref(),
            ENV_CONTEXT_TOKENS,
            catalog_context,
            MAX_CONFIGURED_CONTEXT_TOKENS,
            source,
        )?;
        let max_context_tokens = Some(max_context_limit);
        let max_output_tokens = parse_provider_limit(
            values.max_output_tokens.as_deref(),
            ENV_MAX_OUTPUT_TOKENS,
            catalog_output,
            MAX_CONFIGURED_OUTPUT_TOKENS,
            source,
        )?;
        if max_output_tokens >= max_context_limit {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!(
                        "invalid model configuration: {ENV_MAX_OUTPUT_TOKENS} must be smaller than {ENV_CONTEXT_TOKENS}"
                    ),
                )
                .with_provider_diagnostic(
                    "provider_configuration_invalid",
                    ProviderErrorStage::ClientInitialization,
                ),
            ));
        }
        let provider_name = values
            .provider_name
            .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
        if provider_name != DEFAULT_PROVIDER_NAME {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::UnsupportedCapability,
                    "configured model provider has no registered production adapter",
                )
                .with_provider(provider_name)
                .with_provider_diagnostic(
                    "provider_adapter_unsupported",
                    ProviderErrorStage::ClientInitialization,
                ),
            ));
        }
        let model_name = values
            .model_name
            .ok_or_else(|| missing_provider_config_error(ENV_MODEL, source))?;
        let base_url = values
            .base_url
            .ok_or_else(|| missing_provider_config_error(ENV_BASE_URL, source))?;
        let api_key = values
            .api_key
            .ok_or_else(|| missing_provider_config_error(ENV_API_KEY, source))?;
        let source = source.ok_or_else(provider_source_missing_error)?;
        Ok(Self {
            provider_name,
            model_name,
            base_url,
            api_key,
            source,
            max_context_tokens,
            max_output_tokens,
        })
    }

    /// 返回当前请求 endpoint。
    pub fn endpoint(&self) -> String {
        chat_completions_endpoint(&self.base_url)
    }

    pub(crate) fn completion_protocol_without_tools(&self) -> ProviderApiProtocol {
        if self
            .base_url
            .trim()
            .trim_end_matches('/')
            .ends_with(RESPONSES_PATH)
        {
            ProviderApiProtocol::OpenAiResponses
        } else {
            ProviderApiProtocol::OpenAiChatCompletions
        }
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
    /// 从 [`SelectedModel`] 投影七个 wire 开关。无选择（legacy/env 路径）时
    /// 使用与历史逐项提取完全一致的缺省：developer 角色不支持（OpenAI 兼容
    /// 端点普遍不接受 developer 角色，wire 用 system role）、tool_choice 支持、
    /// thinking 走 `thinking: {"type": ...}`。
    pub(crate) fn from_selection(selection: Option<&SelectedModel>) -> Self {
        Self {
            reasoning_enabled: selection.is_some_and(|s| s.reasoning_enabled),
            reasoning_disabled: selection.is_some_and(|s| !s.reasoning_enabled),
            wire_reasoning_effort: selection.and_then(|s| s.wire_reasoning_effort.clone()),
            thinking_wire_format: selection
                .map(|s| s.thinking_wire_format)
                .unwrap_or(ThinkingWireFormat::ThinkingType),
            supports_developer_role: selection.is_some_and(|s| s.supports_developer_role),
            supports_tool_choice: selection.is_none_or(|s| s.supports_tool_choice),
            requires_assistant_content_for_tool_calls: selection
                .is_some_and(|s| s.requires_assistant_content_for_tool_calls),
        }
    }
}
