//! Runtime provider selection, transport capability assembly, and immutable snapshots.
//!
//! User config and auth reading lives in the sibling `user` module. This module
//! only assembles the provider instance and protocol capabilities required by
//! AgentLoop execution.

use std::collections::BTreeMap;
use std::fmt;
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
