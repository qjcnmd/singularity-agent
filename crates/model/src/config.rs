//! provider 配置分层解析、脱敏状态和服务级配置快照。
use std::fmt;

use super::{
    CHAT_COMPLETIONS_PATH, DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS,
    DEFAULT_MAX_TOOLS_PER_REQUEST, DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL,
    ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS, ENV_MODEL, ENV_PROVIDER,
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS, ModelBlockerKind, ModelError,
    ModelErrorCategory, ModelErrorKind, ModelProviderConfig, OpenAiProvider, OpenAiProviderConfig,
    PROJECT_ENV_FILE, PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE, PROVIDER_SNAPSHOT_ID_PREFIX,
    PROVIDER_TIMEOUT_SECONDS, ProviderApiProtocol, ProviderConfigResolution,
    ProviderConfigSnapshot, ProviderConfigSource, ProviderConfigurationStatus, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderToolReasoningMode, RESPONSES_PATH,
    chat_completions_endpoint, validate_provider_config,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

impl fmt::Debug for ProviderConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfigSnapshot")
            .field("snapshot_id", &self.snapshot_id)
            .field("source", &self.source)
            .field("redacted_config", &self.redacted_config)
            .field("configuration", &self.configuration)
            .finish()
    }
}

impl ProviderConfigSnapshot {
    /// 从环境读取并固定一份 provider 配置快照。
    pub fn capture<F>(get_env: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_cache_path(get_env, None)
    }

    /// 从环境读取 provider 配置，并显式绑定可选的持久 capability cache 路径。
    pub fn capture_with_cache_path<F>(get_env: F, cache_path: Option<PathBuf>) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_provider(get_env, move |config| {
            OpenAiProvider::new_with_cache_path(config, cache_path)
        })
    }

    /// 从环境读取 provider 配置，并把传输绑定到调用方已有的 Tokio runtime handle。
    pub fn capture_with_runtime_handle<F>(
        get_env: F,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_runtime_handle_and_cache_path(get_env, None, runtime_handle)
    }

    /// 从环境读取 provider 配置，绑定调用方 runtime，并显式设置 capability cache 路径。
    pub fn capture_with_runtime_handle_and_cache_path<F>(
        get_env: F,
        cache_path: Option<PathBuf>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_provider(get_env, move |config| {
            OpenAiProvider::new_with_runtime_handle_and_cache_path(
                config,
                PROVIDER_TIMEOUT_SECONDS,
                cache_path,
                runtime_handle,
            )
        })
    }

    fn capture_with_provider<F, P>(get_env: F, provider_factory: P) -> Self
    where
        F: FnMut(&str) -> Option<String>,
        P: FnOnce(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
    {
        let project_dir = std::env::current_dir().ok();
        let values = resolve_provider_values(get_env, project_dir.as_deref());
        let source = values.source;
        let redacted_config = provider_config_resolution(&values).config;
        let provider =
            OpenAiProviderConfig::from_resolved_values(values).and_then(provider_factory);
        let mut configuration = ProviderConfigurationStatus::from_config(&redacted_config);
        if configuration.configured
            && let Err(error) = &provider
        {
            configuration.configured = false;
            configuration.blocker = provider_initialization_blocker(&error.error);
        }
        Self {
            snapshot_id: format!("{PROVIDER_SNAPSHOT_ID_PREFIX}{}", Uuid::new_v4().simple()),
            source,
            redacted_config,
            configuration,
            provider,
        }
    }

    /// 返回配置来源。
    pub fn source(&self) -> Option<ProviderConfigSource> {
        self.source
    }

    /// 返回脱敏后的 provider 配置。
    pub fn redacted_config(&self) -> &ModelProviderConfig {
        &self.redacted_config
    }

    /// 返回配置可用性状态。
    pub fn configuration(&self) -> &ProviderConfigurationStatus {
        &self.configuration
    }

    /// 返回快照稳定标识。
    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    /// 从快照创建 provider 实例。
    pub fn provider(&self) -> Result<OpenAiProvider, ProviderError> {
        self.provider.clone()
    }
}

pub(super) fn provider_initialization_blocker(error: &ModelError) -> Option<ModelBlockerKind> {
    if error.code.as_deref() == Some(PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE) {
        return Some(ModelBlockerKind::ProviderRuntimeUnavailable);
    }
    match error.category() {
        ModelErrorCategory::Authentication => Some(ModelBlockerKind::AuthenticationProviderError),
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            Some(ModelBlockerKind::BaseUrlNetworkError)
        }
        ModelErrorCategory::ModelConfiguration
        | ModelErrorCategory::InvalidRequest
        | ModelErrorCategory::UnsupportedCapability => Some(ModelBlockerKind::ModelNameConfigError),
        ModelErrorCategory::Cancelled
        | ModelErrorCategory::ContextLengthExceeded
        | ModelErrorCategory::BudgetExceeded
        | ModelErrorCategory::ToolCallParse
        | ModelErrorCategory::JsonSchema
        | ModelErrorCategory::ContentFilter
        | ModelErrorCategory::UnknownProviderError => None,
    }
}

impl ProviderConfigurationStatus {
    /// 从 provider 配置生成脱敏状态。
    pub fn from_config(config: &ModelProviderConfig) -> Self {
        let validation = validate_provider_config(config);
        Self {
            configured: validation.valid,
            provider_name: config.provider_name.clone(),
            model_name: config.model_name.clone(),
            api_key_status: redacted_presence(config.api_key_present),
            base_url_status: redacted_presence(config.base_url_present),
            blocker: if validation.valid {
                None
            } else {
                Some(ModelBlockerKind::RequiredEnvMissing)
            },
        }
    }
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
        let project_dir = std::env::current_dir().ok();
        Self::from_resolved_values(resolve_provider_values(get_env, project_dir.as_deref()))
    }

    fn from_resolved_values(values: ResolvedProviderValues) -> Result<Self, ProviderError> {
        validate_provider_value(values.provider_name.as_deref(), ENV_PROVIDER, values.source)?;
        validate_provider_value(values.model_name.as_deref(), ENV_MODEL, values.source)?;
        validate_provider_value(values.base_url.as_deref(), ENV_BASE_URL, values.source)?;
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
        let max_context_tokens = parse_provider_limit(
            values.context_tokens.as_deref(),
            ENV_CONTEXT_TOKENS,
            DEFAULT_MAX_CONTEXT_TOKENS,
            MAX_CONFIGURED_CONTEXT_TOKENS,
            source,
        )?;
        let max_output_tokens = parse_provider_limit(
            values.max_output_tokens.as_deref(),
            ENV_MAX_OUTPUT_TOKENS,
            DEFAULT_MAX_OUTPUT_TOKENS,
            MAX_CONFIGURED_OUTPUT_TOKENS,
            source,
        )?;
        if max_output_tokens >= max_context_tokens {
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

    /// 返回脱敏 provider 配置状态。
    pub fn redacted_status(&self) -> ProviderConfigurationStatus {
        ProviderConfigurationStatus::from_config(&ModelProviderConfig {
            provider_name: Some(self.provider_name.clone()),
            model_name: Some(self.model_name.clone()),
            base_url_present: true,
            api_key_present: true,
        })
    }

    /// 返回当前请求 endpoint。
    pub fn endpoint(&self) -> String {
        chat_completions_endpoint(&self.base_url)
    }

    pub(super) fn api_protocol_candidates(&self) -> Vec<ProviderApiProtocol> {
        let base_url = self.base_url.trim().trim_end_matches('/');
        if base_url.ends_with(RESPONSES_PATH) {
            vec![ProviderApiProtocol::OpenAiResponses]
        } else if base_url.ends_with(CHAT_COMPLETIONS_PATH) {
            vec![ProviderApiProtocol::OpenAiChatCompletions]
        } else {
            vec![
                ProviderApiProtocol::OpenAiResponses,
                ProviderApiProtocol::OpenAiChatCompletions,
            ]
        }
    }

    pub(super) fn completion_protocol_without_tools(&self) -> ProviderApiProtocol {
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
            supports_required_tool_choice: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_json_mode: false,
            supports_system_message: false,
            supports_developer_message: false,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

/// 解析模型提供方配置，同时只报告脱敏存在性和来源元数据。
pub fn resolve_provider_config<F>(get_env: F) -> ProviderConfigResolution
where
    F: FnMut(&str) -> Option<String>,
{
    let project_dir = std::env::current_dir().ok();
    let values = resolve_provider_values(get_env, project_dir.as_deref());
    provider_config_resolution(&values)
}

fn missing_provider_config_error(
    name: &str,
    source: Option<ProviderConfigSource>,
) -> ProviderError {
    let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            format!("required provider configuration is missing: {name} (source={source})"),
        )
        .with_provider_diagnostic(
            "provider_configuration_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn provider_source_missing_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "provider configuration source is missing",
        )
        .with_provider_diagnostic(
            "provider_configuration_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn parse_provider_limit(
    value: Option<&str>,
    name: &str,
    fallback: u32,
    upper_bound: u32,
    source: Option<ProviderConfigSource>,
) -> Result<u32, ProviderError> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    let parsed = value.parse::<u32>().ok().filter(|value| *value > 0);
    match parsed {
        Some(value) if value <= upper_bound => Ok(value),
        _ => {
            let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
            Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!(
                        "invalid model configuration: {name} must be between 1 and {upper_bound} (source={source})"
                    ),
                )
                .with_provider_diagnostic(
                    "provider_configuration_invalid",
                    ProviderErrorStage::ClientInitialization,
                ),
            ))
        }
    }
}

fn validate_provider_value(
    value: Option<&str>,
    name: &str,
    source: Option<ProviderConfigSource>,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    let invalid_boundary_whitespace = value
        .chars()
        .next()
        .is_some_and(|character| character.is_whitespace())
        || value
            .chars()
            .next_back()
            .is_some_and(|character| character.is_whitespace());
    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
        || invalid_boundary_whitespace
    {
        let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
        return Err(ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::InvalidRequest,
                format!(
                    "invalid model configuration: {name} contains forbidden control characters or boundary whitespace (source={source})"
                ),
            )
            .with_provider_diagnostic(
                "provider_configuration_invalid",
                ProviderErrorStage::ClientInitialization,
            ),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct ProviderConfigLayer {
    provider_name: Option<String>,
    model_name: Option<String>,
    context_tokens: Option<String>,
    max_output_tokens: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

impl ProviderConfigLayer {
    fn from_process_env<F>(get_env: &mut F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self {
            provider_name: get_env(ENV_PROVIDER),
            model_name: get_env(ENV_MODEL),
            context_tokens: get_env(ENV_CONTEXT_TOKENS),
            max_output_tokens: get_env(ENV_MAX_OUTPUT_TOKENS),
            base_url: get_env(ENV_BASE_URL),
            api_key: get_env(ENV_API_KEY),
        }
    }

    fn any_present(&self) -> bool {
        self.provider_name.is_some()
            || self.model_name.is_some()
            || self.context_tokens.is_some()
            || self.max_output_tokens.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
    }

    fn into_values(self, source: ProviderConfigSource) -> ResolvedProviderValues {
        ResolvedProviderValues {
            source: Some(source),
            provider_name: normalized_provider_value(self.provider_name),
            model_name: normalized_provider_value(self.model_name),
            context_tokens: normalized_provider_value(self.context_tokens),
            max_output_tokens: normalized_provider_value(self.max_output_tokens),
            base_url: normalized_provider_value(self.base_url),
            api_key: normalized_provider_value(self.api_key),
        }
    }
}

#[derive(Clone, Default)]
struct ResolvedProviderValues {
    source: Option<ProviderConfigSource>,
    provider_name: Option<String>,
    model_name: Option<String>,
    context_tokens: Option<String>,
    max_output_tokens: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

fn provider_config_resolution(values: &ResolvedProviderValues) -> ProviderConfigResolution {
    let provider_name = values.source.map(|_| {
        values
            .provider_name
            .clone()
            .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string())
    });
    ProviderConfigResolution {
        source: values.source,
        config: ModelProviderConfig {
            provider_name,
            model_name: values.model_name.clone(),
            base_url_present: values.base_url.is_some(),
            api_key_present: values.api_key.is_some(),
        },
    }
}

fn resolve_provider_values<F>(mut get_env: F, project_dir: Option<&Path>) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
{
    let process_layer = ProviderConfigLayer::from_process_env(&mut get_env);
    if process_layer.any_present() {
        return process_layer.into_values(ProviderConfigSource::ProcessEnvironment);
    }
    let Some(project_dir) = project_dir else {
        return ResolvedProviderValues::default();
    };
    let project_layer = project_env_layer(project_dir);
    if project_layer.any_present() {
        project_layer.into_values(ProviderConfigSource::ProjectEnvFile)
    } else {
        ResolvedProviderValues::default()
    }
}

fn normalized_provider_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn project_env_layer(project_dir: &Path) -> ProviderConfigLayer {
    let Some(path) = find_project_env_file(project_dir) else {
        return ProviderConfigLayer::default();
    };
    read_project_env_layer(&path)
}

fn find_project_env_file(project_dir: &Path) -> Option<PathBuf> {
    let mut dir = project_dir.to_path_buf();
    loop {
        let path = dir.join(PROJECT_ENV_FILE);
        if path.is_file() {
            return Some(path);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_project_env_layer(path: &Path) -> ProviderConfigLayer {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProviderConfigLayer::default();
    };
    let mut layer = ProviderConfigLayer::default();
    for (name, value) in text.lines().filter_map(parse_env_line) {
        let target = match name.as_str() {
            ENV_PROVIDER => &mut layer.provider_name,
            ENV_MODEL => &mut layer.model_name,
            ENV_CONTEXT_TOKENS => &mut layer.context_tokens,
            ENV_MAX_OUTPUT_TOKENS => &mut layer.max_output_tokens,
            ENV_BASE_URL => &mut layer.base_url,
            ENV_API_KEY => &mut layer.api_key,
            _ => continue,
        };
        if target.is_none() {
            *target = Some(value);
        }
    }
    layer
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let mut text = line.trim_start();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    if let Some(rest) = text.strip_prefix("export ") {
        text = rest.trim_start();
    }
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    let mut value = value;
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            value = &value[1..value.len() - 1];
        }
    }
    Some((name.to_string(), value.to_string()))
}

fn redacted_presence(present: bool) -> String {
    if present {
        "present(redacted)".to_string()
    } else {
        "missing".to_string()
    }
}
