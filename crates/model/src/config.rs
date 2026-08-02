//! provider 配置分层解析、脱敏状态和服务级配置快照。
use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};
use std::marker::PhantomData;

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

/// Immutable, secret-bearing provider instances and their allowlisted model
/// selections. This type never implements `Debug`; the enclosing snapshot only
/// prints redacted status.
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    default_model: String,
    providers: BTreeMap<String, ConfiguredProvider>,
}

#[derive(Clone)]
struct ConfiguredProvider {
    provider: OpenAiProvider,
    models: BTreeMap<String, ConfiguredModel>,
}

#[derive(Clone)]
struct ConfiguredModel {
    protocol: ProviderApiProtocol,
    max_context_tokens: u32,
    max_output_tokens: u32,
    reasoning_variants: BTreeMap<String, ReasoningVariant>,
    default_variant: Option<String>,
    tool_reasoning_mode: ProviderToolReasoningMode,
    supports_developer_role: bool,
    supports_tool_choice: bool,
    requires_reasoning_content_for_tool_calls: bool,
    requires_assistant_content_for_tool_calls: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFile {
    default_model: String,
    #[serde(deserialize_with = "deserialize_unique_map")]
    providers: BTreeMap<String, ModelsFileProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFileProvider {
    adapter: String,
    base_url: String,
    api_key_env: String,
    #[serde(deserialize_with = "deserialize_unique_map")]
    models: BTreeMap<String, ModelsFileModel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFileModel {
    api_protocol: String,
    max_context_tokens: u32,
    max_output_tokens: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    #[serde(default)]
    default_variant: Option<String>,
    #[serde(default)]
    tool_reasoning_history: Option<String>,
    #[serde(default = "default_true")]
    supports_developer_role: bool,
    #[serde(default = "default_true")]
    supports_tool_choice: bool,
    #[serde(default)]
    requires_reasoning_content_for_tool_calls: bool,
    #[serde(default)]
    requires_assistant_content_for_tool_calls: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFileReasoningVariant {
    enabled: bool,
    #[serde(default)]
    wire_effort: Option<String>,
}

#[derive(Clone)]
struct ReasoningVariant {
    enabled: bool,
    wire_effort: Option<String>,
}

fn deserialize_unique_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Ord + DeserializeOwned,
    V: DeserializeOwned,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Ord + DeserializeOwned,
        V: DeserializeOwned,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut result = BTreeMap::new();
            while let Some(key) = access.next_key::<K>()? {
                if result.contains_key(&key) {
                    return Err(de::Error::custom("duplicate object key"));
                }
                result.insert(key, access.next_value()?);
            }
            Ok(result)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

fn default_true() -> bool {
    true
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
            OpenAiProvider::new_with_cache_path(config, cache_path.clone())
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
                cache_path.clone(),
                runtime_handle.clone(),
            )
        })
    }

    fn capture_with_provider<F, P>(get_env: F, provider_factory: P) -> Self
    where
        F: FnMut(&str) -> Option<String>,
        P: Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError>,
    {
        let project_dir = std::env::current_dir().ok();
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
        let values = resolve_provider_values(&mut get_env_once, project_dir.as_deref());
        let source = values.source;
        let (redacted_config, provider, model_selection) =
            if let Some(path) = values.models_config_path.as_deref() {
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

pub(crate) fn model_selector_error(
    message: impl Into<String>,
    code: &'static str,
) -> ProviderError {
    configuration_error(message, code)
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || matches!(character, '/' | '#')
        })
    {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is malformed"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

fn parse_catalog_protocol(value: &str) -> Result<ProviderApiProtocol, ProviderError> {
    match value {
        "chat" => Ok(ProviderApiProtocol::OpenAiChatCompletions),
        "responses" => Ok(ProviderApiProtocol::OpenAiResponses),
        _ => Err(configuration_error(
            "invalid model configuration: api_protocol must be chat or responses",
            "provider_configuration_invalid",
        )),
    }
}

fn parse_tool_reasoning_history(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<ProviderToolReasoningMode, ProviderError> {
    match value.unwrap_or("disabled") {
        "disabled" => Ok(ProviderToolReasoningMode::Unspecified),
        "reasoning_content" if protocol == ProviderApiProtocol::OpenAiChatCompletions => {
            Ok(ProviderToolReasoningMode::ReplayReasoningContent)
        }
        "responses_items" if protocol == ProviderApiProtocol::OpenAiResponses => {
            Ok(ProviderToolReasoningMode::ReplayResponsesItems)
        }
        "reasoning_content" | "responses_items" => Err(configuration_error(
            "tool_reasoning_history does not match api_protocol",
            "provider_configuration_invalid",
        )),
        _ => Err(configuration_error(
            "tool_reasoning_history must be disabled, reasoning_content, or responses_items",
            "provider_configuration_invalid",
        )),
    }
}

fn validate_reasoning_variants(
    protocol: ProviderApiProtocol,
    variants: &BTreeMap<String, ReasoningVariant>,
    default_variant: Option<&str>,
) -> Result<(), ProviderError> {
    if variants.is_empty() {
        if default_variant.is_some() {
            return Err(configuration_error(
                "default_variant must be omitted when reasoning_variants is empty",
                "provider_configuration_invalid",
            ));
        }
        return Ok(());
    }
    let Some(default_variant) = default_variant else {
        return Err(configuration_error(
            "reasoning_variants require an explicit default_variant",
            "provider_configuration_invalid",
        ));
    };
    if !variants.contains_key(default_variant) {
        return Err(configuration_error(
            "default_variant is not declared in reasoning_variants",
            "provider_configuration_invalid",
        ));
    }
    for (variant, descriptor) in variants {
        validate_identifier(variant, "reasoning variant")?;
        if variant == "off" && descriptor.enabled {
            return Err(configuration_error(
                "the off reasoning variant must be explicitly disabled",
                "provider_configuration_invalid",
            ));
        }
        if variant != "off" && !descriptor.enabled {
            return Err(configuration_error(
                "non-off reasoning variants must be enabled",
                "provider_configuration_invalid",
            ));
        }
        if !descriptor.enabled && descriptor.wire_effort.is_some() {
            return Err(configuration_error(
                "disabled reasoning variants cannot declare a wire effort",
                "provider_configuration_invalid",
            ));
        }
        if let Some(wire_effort) = descriptor.wire_effort.as_deref() {
            validate_identifier(wire_effort, "wire reasoning effort")?;
        }
        if descriptor.enabled
            && protocol == ProviderApiProtocol::OpenAiResponses
            && descriptor.wire_effort.is_none()
        {
            return Err(configuration_error(
                "Responses enabled reasoning variants require wire_effort",
                "provider_configuration_invalid",
            ));
        }
    }
    if protocol == ProviderApiProtocol::OpenAiChatCompletions {
        let enabled_without_wire = variants
            .iter()
            .filter(|(_, descriptor)| descriptor.enabled && descriptor.wire_effort.is_none())
            .map(|(variant, _)| variant.as_str())
            .collect::<Vec<_>>();
        if enabled_without_wire.len() > 1
            || enabled_without_wire
                .first()
                .is_some_and(|variant| *variant != "on")
        {
            return Err(configuration_error(
                "Chat no-wire reasoning is only the single on variant",
                "provider_configuration_invalid",
            ));
        }
    }
    Ok(())
}

fn validate_catalog_limit(value: u32, label: &str, upper_bound: u32) -> Result<(), ProviderError> {
    if value == 0 || value > upper_bound {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is outside the supported range"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
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
    let text = std::fs::read_to_string(path).map_err(|_| {
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
        validate_identifier(&provider_name, "provider id")?;
        if provider_file.adapter != "openai_compatible" {
            return Err(configuration_error(
                "configured model provider adapter is unsupported",
                "provider_adapter_unsupported",
            ));
        }
        validate_provider_value(Some(&provider_file.base_url), ENV_BASE_URL, source)?;
        if provider_file.models.is_empty() {
            return Err(configuration_error(
                "configured provider must allowlist at least one model",
                "provider_configuration_invalid",
            ));
        }
        validate_identifier(&provider_file.api_key_env, "api_key_env")?;
        let api_key = get_env(&provider_file.api_key_env).filter(|value| !value.is_empty());
        let api_key = api_key
            .ok_or_else(|| missing_provider_config_error(&provider_file.api_key_env, source))?;
        validate_provider_value(Some(&api_key), &provider_file.api_key_env, source)?;
        let mut models = BTreeMap::new();
        for (model_name, model_file) in provider_file.models {
            validate_identifier(&model_name, "model id")?;
            let protocol = parse_catalog_protocol(&model_file.api_protocol)?;
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
                model_file.max_context_tokens,
                "max_context_tokens",
                MAX_CONFIGURED_CONTEXT_TOKENS,
            )?;
            validate_catalog_limit(
                model_file.max_output_tokens,
                "max_output_tokens",
                MAX_CONFIGURED_OUTPUT_TOKENS,
            )?;
            if model_file.max_output_tokens >= model_file.max_context_tokens {
                return Err(configuration_error(
                    "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
                    "provider_configuration_invalid",
                ));
            }
            models.insert(
                model_name,
                ConfiguredModel {
                    protocol,
                    max_context_tokens: model_file.max_context_tokens,
                    max_output_tokens: model_file.max_output_tokens,
                    reasoning_variants,
                    default_variant: model_file.default_variant,
                    tool_reasoning_mode,
                    supports_developer_role: model_file.supports_developer_role,
                    supports_tool_choice: model_file.supports_tool_choice,
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
        providers.insert(provider_name, ConfiguredProvider { provider, models });
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

struct ParsedModelSelector<'a> {
    provider_name: &'a str,
    model_name: &'a str,
    reasoning_effort: Option<&'a str>,
}

fn parse_model_selector(selector: &str) -> Result<ParsedModelSelector<'_>, ProviderError> {
    let Some((provider_name, model_and_effort)) = selector.split_once('/') else {
        return Err(model_selector_error(
            "model selector must use provider_id/model_id[#variant]",
            "provider_selector_invalid",
        ));
    };
    let (model_name, reasoning_effort) = match model_and_effort.rsplit_once('#') {
        Some((model_name, reasoning_effort)) => (model_name, Some(reasoning_effort)),
        None => (model_and_effort, None),
    };
    if provider_name.is_empty()
        || model_name.is_empty()
        || provider_name.contains(['/', '#'])
        || model_name.contains('#')
        || reasoning_effort.is_some_and(str::is_empty)
    {
        return Err(model_selector_error(
            "model selector must contain non-empty provider, model, and variant ids",
            "provider_selector_invalid",
        ));
    }
    Ok(ParsedModelSelector {
        provider_name,
        model_name,
        reasoning_effort,
    })
}

fn provider_for_selection(
    catalog: &ModelSelectionSnapshot,
    selector: Option<&str>,
) -> Result<OpenAiProvider, ProviderError> {
    let selector = selector.unwrap_or(&catalog.default_model);
    let parsed = parse_model_selector(selector)?;
    let provider = catalog.providers.get(parsed.provider_name).ok_or_else(|| {
        model_selector_error(
            "model selector references an unknown provider",
            "provider_selector_unknown_provider",
        )
    })?;
    let model = provider.models.get(parsed.model_name).ok_or_else(|| {
        model_selector_error(
            "model selector references an unknown or disallowed model",
            "provider_selector_unknown_model",
        )
    })?;
    let requested_variant = parsed.reasoning_effort.or(model.default_variant.as_deref());
    let Some(requested_variant) = requested_variant else {
        return Ok(provider.provider.with_selected_model(super::SelectedModel {
            model_name: parsed.model_name.to_string(),
            api_protocol: model.protocol,
            max_context_tokens: model.max_context_tokens,
            max_output_tokens: model.max_output_tokens,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            supports_developer_role: model.supports_developer_role,
            supports_tool_choice: model.supports_tool_choice,
            requires_reasoning_content_for_tool_calls: false,
            requires_assistant_content_for_tool_calls: model
                .requires_assistant_content_for_tool_calls,
        }));
    };
    let variant = model
        .reasoning_variants
        .get(requested_variant)
        .ok_or_else(|| {
            model_selector_error(
                "model selector references an unknown or disallowed reasoning variant",
                "provider_selector_unknown_reasoning_variant",
            )
        })?;
    if !variant.enabled && requested_variant != "off" {
        return Err(model_selector_error(
            "only the explicitly disabled off variant may be selected",
            "provider_selector_unknown_reasoning_variant",
        ));
    }
    let reasoning_enabled = variant.enabled;
    let tool_reasoning_mode = if reasoning_enabled {
        model.tool_reasoning_mode
    } else {
        ProviderToolReasoningMode::DisabledForToolCalls
    };
    let requires_reasoning_content_for_tool_calls =
        model.requires_reasoning_content_for_tool_calls && reasoning_enabled;
    Ok(provider.provider.with_selected_model(super::SelectedModel {
        model_name: parsed.model_name.to_string(),
        api_protocol: model.protocol,
        max_context_tokens: model.max_context_tokens,
        max_output_tokens: model.max_output_tokens,
        reasoning_variant: Some(requested_variant.to_string()),
        reasoning_enabled,
        wire_reasoning_effort: variant.wire_effort.clone(),
        tool_reasoning_mode,
        supports_developer_role: model.supports_developer_role,
        supports_tool_choice: model.supports_tool_choice,
        requires_reasoning_content_for_tool_calls,
        requires_assistant_content_for_tool_calls: model.requires_assistant_content_for_tool_calls,
    }))
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
        let values = resolve_provider_values(&mut get_env_once, project_dir.as_deref());
        if let Some(path) = values.models_config_path.as_deref() {
            let _ = path;
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
    models_config_path: Option<String>,
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
}

fn provider_config_resolution(values: &ResolvedProviderValues) -> ProviderConfigResolution {
    if let Some(path) = values.models_config_path.as_deref() {
        let config = std::fs::read_to_string(path)
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
