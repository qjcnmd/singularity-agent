//! Runtime provider selection, transport capability assembly, and immutable snapshots.
//!
//! User-facing catalog discovery, cache refresh, dotenv import, auth persistence, and
//! doctor projection remain in the sibling `user` module. This module only assembles
//! the provider instance and protocol capabilities required by AgentLoop execution.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use uuid::Uuid;

use super::*;
use crate::error::ModelErrorCategory;

/// Immutable, secret-bearing provider instances and their allowlisted model
/// selections. This type never implements `Debug`; the enclosing snapshot only
/// prints redacted status.
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    pub(crate) default_model: String,
    pub(crate) providers: BTreeMap<String, ConfiguredProvider>,
}

/// 服务级模型提供方配置快照，包含脱敏状态和已初始化的模型提供方。
///
/// 只捕获一次，使 `AppServer` 报告和使用同一份配置，同时不暴露 API 密钥或其他原始环境值。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    snapshot_id: String,
    source: Option<ProviderConfigSource>,
    redacted_config: ModelProviderConfig,
    configuration: ProviderConfigurationStatus,
    provider: Result<OpenAiProvider, ProviderError>,
    model_selection: Option<std::sync::Arc<ModelSelectionSnapshot>>,
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
    /// 从环境读取并固定一份 provider 配置快照；异步执行使用调用方注入的 runtime。
    pub fn capture<F>(get_env: F, runtime_handle: tokio::runtime::Handle) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let runtime_handle = runtime_handle.clone();
        Self::capture_with_provider(get_env, move |config| {
            OpenAiProvider::new(config, runtime_handle.clone())
        })
    }

    fn capture_with_provider<F, P>(get_env: F, provider_factory: P) -> Self
    where
        F: FnMut(&str) -> Option<String>,
        P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
    {
        Self::capture_with_provider_and_sources(get_env, provider_factory, user_config_layer)
    }

    pub(crate) fn capture_with_provider_and_sources<F, P, U>(
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

pub(crate) fn redacted_models_config() -> ModelProviderConfig {
    ModelProviderConfig {
        provider_name: None,
        model_name: None,
        base_url_present: false,
        api_key_present: false,
    }
}

pub(crate) fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
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
        read_bounded_text(Path::new(path), crate::MAX_DISCOVERY_RESPONSE_BYTES).map_err(|_| {
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

pub(crate) fn capture_models_file<F, P>(
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
            let max_context_tokens = model_file.max_context_tokens;
            let max_output_tokens = model_file.max_output_tokens.ok_or_else(|| {
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

pub(crate) fn provider_initialization_blocker(error: &ModelError) -> Option<ModelBlockerKind> {
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
