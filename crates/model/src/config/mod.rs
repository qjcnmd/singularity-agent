//! provider 配置分层解析、脱敏状态和服务级配置快照。
use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};

use super::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TOOLS_PER_REQUEST,
    DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL, ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS,
    ENV_MODEL, ENV_PROVIDER, MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS,
    MAX_DISCOVERED_MODEL_IDS, ModelBlockerKind, ModelCacheStatus, ModelDiscoveryStatus, ModelError,
    ModelErrorCategory, ModelErrorKind, ModelProviderConfig, OpenAiProvider, OpenAiProviderConfig,
    PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE, PROVIDER_SNAPSHOT_ID_PREFIX, ProviderApiProtocol,
    ProviderCapabilityDeclaration, ProviderConfigResolution, ProviderConfigSnapshot,
    ProviderConfigSource, ProviderConfigurationStatus, ProviderError, ProviderErrorStage,
    ProviderProtocolContract, ProviderToolReasoningMode, RESPONSES_PATH, ThinkingWireFormat,
    USER_AUTH_GENERATION_PREFIX, USER_AUTH_SCHEMA_VERSION, USER_CONFIG_DIR_NAME,
    USER_CONFIG_FILE_NAME, USER_MODELS_CACHE_FILE_NAME, USER_MODELS_CACHE_SCHEMA_VERSION,
    USER_MODELS_CACHE_TTL_SECONDS, UserConfigImportResult, UserModelCatalog, UserModelCatalogEntry,
    UserProviderModelCatalog, chat_completions_endpoint, validate_provider_config,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

mod filesystem;
mod schema;
mod selection;
mod user;
use filesystem::{
    BoundedTextError, read_bounded_text, read_bounded_text_from_file, write_json_file,
};
use schema::*;
pub(super) use selection::model_selector_error;
use selection::{parse_model_selector, provider_for_selection};
#[cfg(test)]
use user::{
    UserAuthFile, UserAuthProvider, UserConfigFile, UserConfigProvider, acquire_config_writer_lock,
    canonicalize_existing_prefix, ensure_home_outside_root, load_models_cache,
    normalize_absolute_path, parse_import_model_selector, path_exists_or_missing,
    read_private_auth_file, read_user_config_data_from_directory, repository_boundary_root,
    write_new_auth_generation,
};
use user::{UserConfigData, UserConfigModel, user_config_error, user_config_layer};
pub use user::{import_env_to_user_config, read_user_model_catalog};

/// Immutable, secret-bearing provider instances and their allowlisted model
/// selections. This type never implements `Debug`; the enclosing snapshot only
/// prints redacted status.
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    default_model: String,
    providers: BTreeMap<String, ConfiguredProvider>,
}

impl fmt::Debug for ProviderConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfigSnapshot")
            .field("snapshot_id", &self.snapshot_id)
            .field("source", &self.source)
            .field("redacted_config", &self.redacted_config)
            .field("configuration", &self.configuration)
            .field("model_selection_present", &self.model_selection.is_some())
            .finish()
    }
}

impl ProviderConfigSnapshot {
    /// 从环境读取并固定一份 provider 配置快照。
    ///
    /// runtime_handle 由已有异步宿主提供时复用该 runtime；否则 provider 自己拥有
    /// runtime。
    pub fn capture<F>(get_env: F, runtime_handle: Option<tokio::runtime::Handle>) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_provider(get_env, move |config| {
            if let Some(runtime_handle) = runtime_handle.as_ref() {
                OpenAiProvider::new_with_runtime_handle(config, runtime_handle.clone())
            } else {
                OpenAiProvider::new(config)
            }
        })
    }

    fn capture_with_provider<F, P>(get_env: F, provider_factory: P) -> Self
    where
        F: FnMut(&str) -> Option<String>,
        P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
    {
        Self::capture_with_provider_and_sources(get_env, provider_factory, user_config_layer)
    }

    fn capture_with_provider_and_sources<F, P, U>(
        get_env: F,
        provider_factory: P,
        user_config: U,
    ) -> Self
    where
        F: FnMut(&str) -> Option<String>,
        P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
        U: FnOnce() -> Option<ProviderConfigLayer>,
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
        let values = resolve_provider_values_with_user_config(&mut get_env_once, user_config);
        let source = values.source;
        let (redacted_config, provider, model_selection) =
            if let Some(error) = values.user_config_error.clone() {
                (redacted_models_config(), Err(error), None)
            } else if let Some(user_config) = values.user_config.as_ref() {
                match capture_user_model_selection(user_config, source, &provider_factory) {
                    Ok((catalog, redacted)) => {
                        let provider = provider_for_selection(&catalog, None);
                        (redacted, provider, Some(std::sync::Arc::new(catalog)))
                    }
                    Err(error) => (redacted_models_config(), Err(error), None),
                }
            } else if let Some(path) = values.models_config_path.as_deref() {
                match capture_model_selection(path, &mut get_env_once, source, &provider_factory) {
                    Ok((catalog, redacted)) => {
                        let provider = provider_for_selection(&catalog, None);
                        (redacted, provider, Some(std::sync::Arc::new(catalog)))
                    }
                    Err(error) => (redacted_models_config(), Err(error), None),
                }
            } else {
                let redacted_config = provider_config_resolution(&values).config;
                let provider =
                    OpenAiProviderConfig::from_resolved_values(values).and_then(provider_factory);
                (redacted_config, provider, None)
            };
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
            model_selection,
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

    /// 返回本次快照是否来自显式多-provider 模型配置。
    pub fn has_explicit_model_selection(&self) -> bool {
        self.model_selection.is_some()
    }

    /// 若当前配置能无歧义解析默认 selector，返回其完整字符串
    /// （catalog 为 `provider/model#effort`，legacy 为裸 model id）；
    /// provider 未配置或无法解析时返回 `None`（调用方保留 `Thread.model` 为 NULL）。
    pub fn resolved_default_selector(&self) -> Option<String> {
        self.provider().ok()?.resolved_selector()
    }

    /// 从快照创建 provider 实例。
    pub fn provider(&self) -> Result<OpenAiProvider, ProviderError> {
        self.provider_for_selector(None)
    }

    /// Resolve a persisted `provider_id/model_id` reference against this
    /// immutable snapshot. The returned provider clone has a bare model id and
    /// exactly one configured protocol.
    pub fn provider_for_selector(
        &self,
        selector: Option<&str>,
    ) -> Result<OpenAiProvider, ProviderError> {
        if let Some(selection) = &self.model_selection {
            return provider_for_selection(selection, selector);
        }
        let provider = self.provider.clone()?;
        if let Some(selector) = selector {
            if selector.contains('#') || selector.contains('/') {
                let parsed = parse_model_selector(selector)?;
                if parsed.provider_name != provider.configured_provider_name() {
                    return Err(model_selector_error(
                        "model selector references an unknown provider",
                        "provider_selector_unknown_provider",
                    ));
                }
                if parsed.model_name != provider.config_snapshot().model_name {
                    return Err(model_selector_error(
                        "model selector references an unknown or disallowed model",
                        "provider_selector_unknown_model",
                    ));
                }
                if parsed.reasoning_effort.is_some() {
                    return Err(model_selector_error(
                        "legacy provider configuration does not declare reasoning variants",
                        "provider_selector_reasoning_unsupported",
                    ));
                }
            } else if selector != provider.config_snapshot().model_name {
                return Err(model_selector_error(
                    "model selector references an unknown or disallowed model",
                    "provider_selector_unknown_model",
                ));
            }
            if selector.ends_with('/') {
                return Err(model_selector_error(
                    "provider/model selector must contain a model id",
                    "provider_selector_invalid",
                ));
            }
        }
        Ok(provider)
    }
}

fn redacted_models_config() -> ModelProviderConfig {
    ModelProviderConfig {
        provider_name: None,
        model_name: None,
        base_url_present: false,
        api_key_present: false,
    }
}

fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::InvalidRequest, message)
            .with_provider_diagnostic(code, ProviderErrorStage::ClientInitialization),
    )
}

fn capture_model_selection<F, P>(
    path: &str,
    get_env: &mut F,
    source: Option<ProviderConfigSource>,
    provider_factory: &P,
) -> Result<(ModelSelectionSnapshot, ModelProviderConfig), ProviderError>
where
    F: FnMut(&str) -> Option<String>,
    P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
{
    let text =
        read_bounded_text(Path::new(path), super::MAX_DISCOVERY_RESPONSE_BYTES).map_err(|_| {
            configuration_error(
                "model configuration file could not be read",
                "provider_configuration_invalid",
            )
        })?;
    let file: ModelsFile = serde_json::from_str(&text).map_err(|_| {
        configuration_error(
            "model configuration file is invalid JSON",
            "provider_configuration_invalid",
        )
    })?;
    capture_models_file(file, get_env, source, provider_factory)
}

fn capture_models_file<F, P>(
    file: ModelsFile,
    get_env: &mut F,
    source: Option<ProviderConfigSource>,
    provider_factory: &P,
) -> Result<(ModelSelectionSnapshot, ModelProviderConfig), ProviderError>
where
    F: FnMut(&str) -> Option<String>,
    P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
{
    if file.providers.is_empty() {
        return Err(configuration_error(
            "model configuration must contain at least one provider",
            "provider_configuration_invalid",
        ));
    }
    let default_selector = parse_model_selector(&file.default_model)?;
    let default_provider_name = default_selector.provider_name.to_string();
    let default_model_name = default_selector.model_name.to_string();
    let mut providers = BTreeMap::new();
    for (provider_name, provider_file) in file.providers {
        validate_provider_identifier(&provider_name, "provider id")?;
        if provider_file.adapter != "openai_compatible" {
            return Err(configuration_error(
                "configured model provider adapter is unsupported",
                "provider_adapter_unsupported",
            ));
        }
        validate_base_url(Some(&provider_file.base_url), source)?;
        if provider_file.models.is_empty() {
            return Err(configuration_error(
                "configured provider must allowlist at least one model",
                "provider_configuration_invalid",
            ));
        }
        validate_identifier(&provider_file.api_key_env, "api_key_env")?;
        let api_key = provider_file
            .api_key_override
            .clone()
            .or_else(|| get_env(&provider_file.api_key_env))
            .filter(|value| !value.is_empty());
        let api_key = api_key
            .ok_or_else(|| missing_provider_config_error(&provider_file.api_key_env, source))?;
        validate_provider_value(Some(&api_key), &provider_file.api_key_env, source)?;
        let mut models = BTreeMap::new();
        for (model_name, model_file) in provider_file.models {
            validate_model_id(&model_name, "model id")?;
            let protocol = parse_catalog_protocol(&model_file.api_protocol)?;
            let declared_capabilities = model_file.capabilities.clone();
            let capabilities = declared_capabilities.clone().unwrap_or_default();
            let max_context_tokens = model_file
                .max_context_tokens
                .or(capabilities.max_context_tokens);
            let max_output_tokens = model_file
                .max_output_tokens
                .or(capabilities.max_output_tokens)
                .ok_or_else(|| {
                    configuration_error(
                        "model configuration must declare max_output_tokens or capabilities.max_output_tokens",
                        "provider_configuration_invalid",
                    )
                })?;
            let supports_developer_role = model_file
                .supports_developer_role
                .or(capabilities.supports_developer_message)
                .unwrap_or(true);
            let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
            let capability_overrides = declared_capabilities.map(|mut overrides| {
                overrides.max_context_tokens = max_context_tokens;
                overrides.max_output_tokens = Some(max_output_tokens);
                if model_file.supports_developer_role.is_some() {
                    overrides.supports_developer_message = None;
                }
                overrides
            });
            let reasoning_variants = model_file
                .reasoning_variants
                .into_iter()
                .map(|(variant, descriptor)| {
                    (
                        variant,
                        ReasoningVariant {
                            enabled: descriptor.enabled,
                            wire_effort: descriptor.wire_effort,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            validate_reasoning_variants(
                protocol,
                &reasoning_variants,
                model_file.default_variant.as_deref(),
            )?;
            let tool_reasoning_mode = parse_tool_reasoning_history(
                model_file.tool_reasoning_history.as_deref(),
                protocol,
            )?;
            let thinking_wire_format =
                parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
            if tool_reasoning_mode != ProviderToolReasoningMode::Unspecified
                && reasoning_variants
                    .get(model_file.default_variant.as_deref().unwrap_or(""))
                    .is_none_or(|variant| !variant.enabled)
            {
                return Err(configuration_error(
                    "tool_reasoning_history requires an enabled default reasoning variant",
                    "provider_configuration_invalid",
                ));
            }
            if model_file.requires_reasoning_content_for_tool_calls
                && tool_reasoning_mode != ProviderToolReasoningMode::ReplayReasoningContent
            {
                return Err(configuration_error(
                    "requires_reasoning_content_for_tool_calls requires Chat reasoning_content replay",
                    "provider_configuration_invalid",
                ));
            }
            if model_file.requires_assistant_content_for_tool_calls
                && protocol != ProviderApiProtocol::OpenAiChatCompletions
            {
                return Err(configuration_error(
                    "requires_assistant_content_for_tool_calls only applies to Chat",
                    "provider_configuration_invalid",
                ));
            }
            validate_catalog_limit(
                max_context_tokens,
                "max_context_tokens",
                MAX_CONFIGURED_CONTEXT_TOKENS,
            )?;
            validate_catalog_limit(
                Some(max_output_tokens),
                "max_output_tokens",
                MAX_CONFIGURED_OUTPUT_TOKENS,
            )?;
            if max_context_tokens.is_some_and(|context| max_output_tokens >= context) {
                return Err(configuration_error(
                    "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
                    "provider_configuration_invalid",
                ));
            }
            models.insert(
                model_name,
                ConfiguredModel {
                    protocol,
                    max_context_tokens,
                    max_output_tokens,
                    reasoning_variants,
                    default_variant: model_file.default_variant,
                    thinking_wire_format,
                    tool_reasoning_mode,
                    supports_developer_role,
                    supports_tool_choice,
                    requires_reasoning_content_for_tool_calls: model_file
                        .requires_reasoning_content_for_tool_calls,
                    requires_assistant_content_for_tool_calls: model_file
                        .requires_assistant_content_for_tool_calls,
                    capability_overrides,
                },
            );
        }
        let base_model_name = models
            .keys()
            .next()
            .cloned()
            .expect("models checked non-empty");
        let base_model = models[&base_model_name].clone();
        let config = OpenAiProviderConfig {
            provider_name: provider_name.clone(),
            model_name: base_model_name,
            base_url: provider_file.base_url,
            api_key,
            source: source.ok_or_else(provider_source_missing_error)?,
            max_context_tokens: base_model.max_context_tokens,
            max_output_tokens: base_model.max_output_tokens,
        };
        let provider = provider_factory(config)?;
        providers.insert(
            provider_name,
            ConfiguredProvider {
                provider: Some(provider),
                provider_error: None,
                models,
            },
        );
    }
    let default_provider = providers
        .get(default_provider_name.as_str())
        .ok_or_else(|| {
            model_selector_error(
                "default_model references an unknown provider",
                "provider_selector_unknown_provider",
            )
        })?;
    if !default_provider
        .models
        .contains_key(default_model_name.as_str())
    {
        return Err(model_selector_error(
            "default_model references an unknown model",
            "provider_selector_unknown_model",
        ));
    }
    let default_model = file.default_model;
    Ok((
        ModelSelectionSnapshot {
            default_model: default_model.clone(),
            providers,
        },
        ModelProviderConfig {
            provider_name: Some(default_provider_name),
            model_name: Some(default_model),
            base_url_present: true,
            api_key_present: true,
        },
    ))
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

    fn from_resolved_values(values: ResolvedProviderValues) -> Result<Self, ProviderError> {
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
            supports_system_message: true,
            supports_developer_message: true,
            max_parallel_tool_calls: 1,
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
    let values = resolve_provider_values(get_env);
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

fn missing_provider_auth_error(source: Option<ProviderConfigSource>) -> ProviderError {
    let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::AuthError,
            format!("required provider authentication is missing (source={source})"),
        )
        .with_provider_diagnostic(
            "provider_auth_missing",
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

/// Validate a provider endpoint before it can be used for transport or persisted discovery.
/// The original spelling is retained by callers; parsing is only used for trust-boundary checks.
fn validate_base_url(
    value: Option<&str>,
    source: Option<ProviderConfigSource>,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    validate_provider_value(Some(value), ENV_BASE_URL, source)?;
    if value.is_empty() {
        return Err(configuration_error(
            "invalid model configuration: SINGULARITY_BASE_URL must not be empty",
            "provider_configuration_invalid",
        ));
    }
    let url = reqwest::Url::parse(value).map_err(|_| {
        configuration_error(
            "invalid model configuration: SINGULARITY_BASE_URL must be an absolute URL",
            "provider_configuration_invalid",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(configuration_error(
            "invalid model configuration: SINGULARITY_BASE_URL must be an http/https URL with a host, path only, and no credentials, query, or fragment",
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

fn normalized_endpoint_identity(base_url: &str) -> Result<String, ProviderError> {
    validate_base_url(Some(base_url), Some(ProviderConfigSource::UserConfigFile))?;
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| user_config_error("user provider endpoint could not be normalized"))?;
    let mut identity = url.as_str().to_string();
    while identity.ends_with('/') {
        identity.pop();
    }
    if identity.is_empty() {
        return Err(user_config_error(
            "user provider endpoint could not be normalized",
        ));
    }
    Ok(identity)
}

#[derive(Default)]
struct ProviderConfigLayer {
    provider_name: Option<String>,
    model_name: Option<String>,
    context_tokens: Option<String>,
    max_output_tokens: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    models_config_path: Option<String>,
    user_config: Option<UserConfigData>,
    user_config_error: Option<ProviderError>,
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
            models_config_path: get_env(super::ENV_MODELS_CONFIG),
            user_config: None,
            user_config_error: None,
        }
    }

    fn any_present(&self) -> bool {
        self.provider_name.is_some()
            || self.model_name.is_some()
            || self.context_tokens.is_some()
            || self.max_output_tokens.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
            || self.models_config_path.is_some()
            || self.user_config.is_some()
            || self.user_config_error.is_some()
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
            models_config_path: normalized_provider_value(self.models_config_path),
            user_config: self.user_config,
            user_config_error: self.user_config_error,
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
    models_config_path: Option<String>,
    user_config: Option<UserConfigData>,
    user_config_error: Option<ProviderError>,
}

fn configured_model_from_file(
    model_file: ModelsFileModel,
    capability_overrides: Option<ProviderCapabilityDeclaration>,
) -> Result<ConfiguredModel, ProviderError> {
    let protocol = parse_catalog_protocol(&model_file.api_protocol)?;
    let max_context_tokens = model_file.max_context_tokens;
    let max_output_tokens = model_file
        .max_output_tokens
        .or_else(|| {
            capability_overrides
                .as_ref()
                .and_then(|capabilities| capabilities.max_output_tokens)
        })
        .ok_or_else(|| {
            configuration_error(
                "model configuration must declare max_output_tokens",
                "provider_configuration_invalid",
            )
        })?;
    let supports_developer_role = model_file.supports_developer_role.unwrap_or(true);
    let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
    let reasoning_variants = model_file
        .reasoning_variants
        .into_iter()
        .map(|(variant, descriptor)| {
            (
                variant,
                ReasoningVariant {
                    enabled: descriptor.enabled,
                    wire_effort: descriptor.wire_effort,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    validate_reasoning_variants(
        protocol,
        &reasoning_variants,
        model_file.default_variant.as_deref(),
    )?;
    let tool_reasoning_mode =
        parse_tool_reasoning_history(model_file.tool_reasoning_history.as_deref(), protocol)?;
    let thinking_wire_format =
        parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
    if tool_reasoning_mode != ProviderToolReasoningMode::Unspecified
        && reasoning_variants
            .get(model_file.default_variant.as_deref().unwrap_or(""))
            .is_none_or(|variant| !variant.enabled)
    {
        return Err(configuration_error(
            "tool_reasoning_history requires an enabled default reasoning variant",
            "provider_configuration_invalid",
        ));
    }
    if model_file.requires_reasoning_content_for_tool_calls
        && tool_reasoning_mode != ProviderToolReasoningMode::ReplayReasoningContent
    {
        return Err(configuration_error(
            "requires_reasoning_content_for_tool_calls requires Chat reasoning_content replay",
            "provider_configuration_invalid",
        ));
    }
    if model_file.requires_assistant_content_for_tool_calls
        && protocol != ProviderApiProtocol::OpenAiChatCompletions
    {
        return Err(configuration_error(
            "requires_assistant_content_for_tool_calls only applies to Chat",
            "provider_configuration_invalid",
        ));
    }
    validate_catalog_limit(
        max_context_tokens,
        "max_context_tokens",
        MAX_CONFIGURED_CONTEXT_TOKENS,
    )?;
    validate_catalog_limit(
        Some(max_output_tokens),
        "max_output_tokens",
        MAX_CONFIGURED_OUTPUT_TOKENS,
    )?;
    if max_context_tokens.is_some_and(|context| max_output_tokens >= context) {
        return Err(configuration_error(
            "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
            "provider_configuration_invalid",
        ));
    }
    Ok(ConfiguredModel {
        protocol,
        max_context_tokens,
        max_output_tokens,
        reasoning_variants,
        default_variant: model_file.default_variant,
        thinking_wire_format,
        tool_reasoning_mode,
        supports_developer_role,
        supports_tool_choice,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls,
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
        capability_overrides,
    })
}

fn configured_model_from_user_file(
    provider_name: &str,
    model_name: &str,
    model_file: &UserConfigModel,
) -> Result<ConfiguredModel, ProviderError> {
    // 合并优先级：顶层字段 > capabilities 内嵌 > 内置模型表 > 默认值。capabilities 块
    // 是旧 probe 时代 config.json 的显式声明残留，接受并投影到静态契约。内置表只兜
    // 底缺省的 context_window/max_output_tokens；api_protocol 仍必须由用户声明。
    let capabilities = model_file.capabilities.clone().unwrap_or_default();
    let builtin = super::builtin_models::builtin_model(provider_name, model_name);
    let (Some(api_protocol), Some(max_output_tokens)) = (
        model_file.api_protocol.as_deref(),
        model_file
            .max_output_tokens
            .or(capabilities.max_output_tokens)
            .or_else(|| builtin.map(|entry| entry.max_output_tokens)),
    ) else {
        return Err(configuration_error(
            "model override is incomplete; api_protocol and max_output_tokens are required",
            "provider_configuration_invalid",
        ));
    };
    let capability_overrides = ProviderCapabilityDeclaration {
        supports_tools: capabilities.supports_tools,
        supports_parallel_tool_calls: capabilities.supports_parallel_tool_calls,
        supports_required_tool_choice: capabilities.supports_required_tool_choice,
        supports_strict_tool_schema: capabilities.supports_strict_tool_schema,
        supports_json_mode: capabilities.supports_json_mode,
        supports_system_message: capabilities.supports_system_message,
        supports_developer_message: capabilities.supports_developer_message,
        supports_reasoning: capabilities.supports_reasoning,
        max_tools_per_request: capabilities.max_tools_per_request,
        max_parallel_tool_calls: capabilities.max_parallel_tool_calls,
        max_context_tokens: model_file
            .max_context_tokens
            .or(capabilities.max_context_tokens)
            .or_else(|| builtin.map(|entry| entry.context_window)),
        max_output_tokens: model_file
            .max_output_tokens
            .or(capabilities.max_output_tokens)
            .or_else(|| builtin.map(|entry| entry.max_output_tokens)),
    };
    configured_model_from_file(
        ModelsFileModel {
            api_protocol: api_protocol.to_string(),
            max_context_tokens: capability_overrides.max_context_tokens,
            max_output_tokens: Some(max_output_tokens),
            reasoning_variants: model_file.reasoning_variants.clone(),
            default_variant: model_file.default_variant.clone(),
            tool_reasoning_history: model_file.tool_reasoning_history.clone(),
            supports_developer_role: Some(
                model_file
                    .supports_developer_role
                    .or(capabilities.supports_developer_message)
                    .unwrap_or(true),
            ),
            supports_tool_choice: Some(model_file.supports_tool_choice.unwrap_or(true)),
            requires_reasoning_content_for_tool_calls: model_file
                .requires_reasoning_content_for_tool_calls,
            requires_assistant_content_for_tool_calls: model_file
                .requires_assistant_content_for_tool_calls,
            thinking_wire_format: model_file.thinking_wire_format.clone(),
            capabilities: None,
        },
        Some(capability_overrides),
    )
}

fn capture_user_model_selection<P>(
    user_config: &UserConfigData,
    source: Option<ProviderConfigSource>,
    provider_factory: &P,
) -> Result<(ModelSelectionSnapshot, ModelProviderConfig), ProviderError>
where
    P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
{
    let default_model = user_config.config.default_model.clone().ok_or_else(|| {
        model_selector_error(
            "user provider config must declare default_model",
            "provider_selector_invalid",
        )
    })?;
    let parsed_default = parse_model_selector(&default_model)?;
    let default_provider_name = user_config
        .config
        .default_provider
        .clone()
        .unwrap_or_else(|| parsed_default.provider_name.to_string());
    if default_provider_name != parsed_default.provider_name {
        return Err(model_selector_error(
            "default_provider does not match default_model",
            "provider_selector_invalid",
        ));
    }
    let mut providers = BTreeMap::new();
    for (provider_name, provider_file) in &user_config.config.providers {
        if let Err(error) = validate_provider_identifier(provider_name, "provider id") {
            if provider_name.as_str() == default_provider_name {
                return Err(error);
            }
            continue;
        }
        let endpoint_error = validate_base_url(Some(&provider_file.base_url), source).err();
        if provider_name.as_str() == default_provider_name
            && let Some(error) = endpoint_error.clone()
        {
            return Err(error);
        }
        let api_key = user_config
            .auth
            .providers
            .get(provider_name)
            .map(|provider| provider.api_key.clone())
            .filter(|value| !value.is_empty());
        if provider_name.as_str() == default_provider_name && api_key.is_none() {
            return Err(missing_provider_auth_error(source));
        }
        let auth_error = api_key
            .as_deref()
            .and_then(|api_key| validate_provider_value(Some(api_key), ENV_API_KEY, source).err());
        if provider_name.as_str() == default_provider_name
            && let Some(error) = auth_error.clone()
        {
            return Err(error);
        }
        let mut models = BTreeMap::new();
        for (model_name, model_file) in &provider_file.models {
            if let Err(error) = validate_model_id(model_name, "model id") {
                if provider_name.as_str() == default_provider_name
                    && model_name == parsed_default.model_name
                {
                    return Err(error);
                }
                continue;
            }
            match configured_model_from_user_file(provider_name, model_name, model_file) {
                Ok(model) => {
                    models.insert(model_name.clone(), model);
                }
                Err(error)
                    if provider_name.as_str() == default_provider_name
                        && model_name == parsed_default.model_name =>
                {
                    return Err(error);
                }
                Err(_) => continue,
            }
        }
        if models.is_empty() {
            continue;
        }
        let (provider, provider_error) = match (api_key, endpoint_error, auth_error) {
            (None, _, _) => (None, Some(missing_provider_auth_error(source))),
            (_, Some(error), _) => (None, Some(error)),
            (_, _, Some(error)) => (None, Some(error)),
            (Some(api_key), None, None) => {
                let base_model = models
                    .keys()
                    .next()
                    .cloned()
                    .expect("models checked non-empty");
                let base_model_config = models.get(&base_model).expect("base model exists");
                let config = OpenAiProviderConfig {
                    provider_name: provider_name.clone(),
                    model_name: base_model,
                    base_url: provider_file.base_url.clone(),
                    api_key,
                    source: source.ok_or_else(provider_source_missing_error)?,
                    max_context_tokens: base_model_config.max_context_tokens,
                    max_output_tokens: base_model_config.max_output_tokens,
                };
                match provider_factory(config) {
                    Ok(provider) => (Some(provider), None),
                    Err(error) if provider_name == &default_provider_name => return Err(error),
                    Err(error) => (None, Some(error)),
                }
            }
        };
        providers.insert(
            provider_name.clone(),
            ConfiguredProvider {
                provider,
                provider_error,
                models,
            },
        );
    }
    if providers.is_empty() {
        return Err(configuration_error(
            "user provider config has no model with explicit protocol and output token limit",
            "provider_configuration_invalid",
        ));
    }
    let default_provider = providers.get(&default_provider_name).ok_or_else(|| {
        model_selector_error(
            "default_model references an unknown provider",
            "provider_selector_unknown_provider",
        )
    })?;
    if !default_provider
        .models
        .contains_key(parsed_default.model_name)
    {
        return Err(model_selector_error(
            "default_model references an unknown model",
            "provider_selector_unknown_model",
        ));
    }
    if default_provider.provider.is_none() {
        return Err(default_provider
            .provider_error
            .clone()
            .unwrap_or_else(|| missing_provider_auth_error(source)));
    }
    Ok((
        ModelSelectionSnapshot {
            default_model: default_model.clone(),
            providers,
        },
        ModelProviderConfig {
            provider_name: Some(default_provider_name),
            model_name: Some(default_model),
            base_url_present: true,
            api_key_present: true,
        },
    ))
}

fn provider_config_resolution(values: &ResolvedProviderValues) -> ProviderConfigResolution {
    if let Some(user_config) = values.user_config.as_ref() {
        let default_selector = user_config.config.default_model.clone();
        let parsed = default_selector
            .as_deref()
            .and_then(|selector| parse_model_selector(selector).ok());
        let provider_name = parsed
            .as_ref()
            .map(|selector| selector.provider_name.to_string())
            .or_else(|| user_config.config.default_provider.clone());
        let model_name = default_selector;
        return ProviderConfigResolution {
            source: values.source,
            config: ModelProviderConfig {
                provider_name,
                model_name,
                base_url_present: values.base_url.is_some(),
                api_key_present: values.api_key.is_some(),
            },
        };
    }
    if let Some(path) = values.models_config_path.as_deref() {
        let config = read_bounded_text(Path::new(path), super::MAX_DISCOVERY_RESPONSE_BYTES)
            .ok()
            .and_then(|text| serde_json::from_str::<ModelsFile>(&text).ok())
            .and_then(|file| {
                let parsed = parse_model_selector(&file.default_model).ok()?;
                file.providers.get(parsed.provider_name)?;
                Some(ModelProviderConfig {
                    provider_name: Some(parsed.provider_name.to_string()),
                    model_name: Some(file.default_model.clone()),
                    base_url_present: true,
                    api_key_present: true,
                })
            })
            .unwrap_or_else(redacted_models_config);
        return ProviderConfigResolution {
            source: values.source,
            config,
        };
    }
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

fn resolve_provider_values<F>(mut get_env: F) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
{
    resolve_provider_values_with_user_config(&mut get_env, user_config_layer)
}

fn resolve_provider_values_with_user_config<F, U>(
    mut get_env: F,
    user_config: U,
) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
    U: FnOnce() -> Option<ProviderConfigLayer>,
{
    let process_layer = ProviderConfigLayer::from_process_env(&mut get_env);
    if process_layer.any_present() {
        return process_layer.into_values(ProviderConfigSource::ProcessEnvironment);
    }
    user_config()
        .map(|layer| layer.into_values(ProviderConfigSource::UserConfigFile))
        .unwrap_or_default()
}

fn normalized_provider_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn find_import_env_file(project_dir: &Path) -> Option<PathBuf> {
    let mut dir = project_dir.to_path_buf();
    loop {
        let path = dir.join(".env");
        if path.is_file() {
            return Some(path);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_import_env_layer(path: &Path) -> ProviderConfigLayer {
    let Ok(text) = read_bounded_text(path, super::MAX_DISCOVERY_RESPONSE_BYTES) else {
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
            super::ENV_MODELS_CONFIG => &mut layer.models_config_path,
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

#[cfg(test)]
#[path = "../config_tests.rs"]
mod user_config_tests;
