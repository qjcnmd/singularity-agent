use crate::config::schema::{validate_model_id, validate_provider_identifier};
use crate::config::{
    ResolvedProviderValues, configuration_error, missing_provider_config_error,
    parse_provider_limit, provider_source_missing_error, resolve_provider_values,
    validate_base_url, validate_provider_value,
};
use crate::error::{ModelError, ModelErrorKind, ProviderErrorStage};
use crate::openai::chat_completions_endpoint;
use crate::provider::contract::ProviderProtocolContract;
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TOOLS_PER_REQUEST,
    DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL, ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS,
    ENV_MODEL, ENV_PROVIDER, MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS,
    ProviderApiProtocol, ProviderCapabilityDeclaration, ProviderConfigSource, ProviderError,
    ProviderToolReasoningMode, RESPONSES_PATH, ThinkingWireFormat,
};
use std::fmt;
use std::future::Future;
use std::sync::Arc;

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
    /// 从环境加载并验证 OpenAI-compatible 配置。
    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut get_env = get_env;
        let mut captured_env = std::collections::HashMap::<String, Option<String>>::new();
        let mut get_env_once = |name: &str| {
            if let Some(value) = captured_env.get(name) {
                return value.clone();
            }
            let value = get_env(name);
            captured_env.insert(name.to_string(), value.clone());
            value
        };
        let values = resolve_provider_values(&mut get_env_once);
        if values.models_config_path.is_some() || values.user_config.is_some() {
            return Err(configuration_error(
                "OpenAiProviderConfig cannot represent a composite models selection; use OpenAiProvider::from_env",
                "provider_configuration_composite_selection_required",
            ));
        }
        Self::from_resolved_values(values)
    }

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
        let max_context_limit = parse_provider_limit(
            values.context_tokens.as_deref(),
            ENV_CONTEXT_TOKENS,
            DEFAULT_MAX_CONTEXT_TOKENS,
            MAX_CONFIGURED_CONTEXT_TOKENS,
            source,
        )?;
        let max_context_tokens = Some(max_context_limit);
        let max_output_tokens = parse_provider_limit(
            values.max_output_tokens.as_deref(),
            ENV_MAX_OUTPUT_TOKENS,
            DEFAULT_MAX_OUTPUT_TOKENS,
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
            supports_parallel_tool_calls: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_system_message: true,
            supports_developer_message: true,
            max_parallel_tool_calls: 1,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

/// One fully resolved catalog selection. Keeping the canonical variant,
/// enabled state and the single wire effort together prevents a second runtime
/// mapping table from silently changing the provider request.
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
    /// 合并后的用户显式能力声明；协议契约构造时叠加到静态基线。
    pub(crate) capability_overrides: Option<ProviderCapabilityDeclaration>,
}

/// Provider transport runtime ownership: an app-server borrows its existing handle, while
/// independent consumers own a dedicated runtime shared by provider clones.
#[derive(Clone)]
pub(crate) enum ProviderRuntime {
    External(tokio::runtime::Handle),
    Owned(Arc<tokio::runtime::Runtime>),
}

impl ProviderRuntime {
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        match self {
            Self::External(handle) => handle.block_on(future),
            Self::Owned(runtime) => runtime.block_on(future),
        }
    }
}
