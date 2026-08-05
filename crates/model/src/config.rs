//! provider 配置分层解析、脱敏状态和服务级配置快照。
use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};
use std::marker::PhantomData;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};

use super::{
    CHAT_COMPLETIONS_PATH, DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS,
    DEFAULT_MAX_TOOLS_PER_REQUEST, DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL,
    ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS, ENV_MODEL, ENV_PROVIDER,
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS, MAX_DISCOVERED_MODEL_IDS,
    ModelBlockerKind, ModelCacheStatus, ModelDiscoveryStatus, ModelError, ModelErrorCategory,
    ModelErrorKind, ModelProviderConfig, OpenAiProvider, OpenAiProviderConfig,
    PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE, PROVIDER_SNAPSHOT_ID_PREFIX,
    PROVIDER_TIMEOUT_SECONDS, ProviderApiProtocol, ProviderConfigResolution,
    ProviderConfigSnapshot, ProviderConfigSource, ProviderConfigurationStatus, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderToolReasoningMode, RESPONSES_PATH,
    ThinkingWireFormat, USER_AUTH_GENERATION_PREFIX, USER_AUTH_SCHEMA_VERSION,
    USER_CONFIG_DIR_NAME, USER_CONFIG_FILE_NAME, USER_MODELS_CACHE_FILE_NAME,
    USER_MODELS_CACHE_SCHEMA_VERSION, USER_MODELS_CACHE_TTL_SECONDS, UserConfigImportResult,
    UserModelCatalog, UserModelCatalogEntry, UserProviderModelCatalog, chat_completions_endpoint,
    validate_provider_config,
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
    provider: Option<OpenAiProvider>,
    provider_error: Option<ProviderError>,
    models: BTreeMap<String, ConfiguredModel>,
}

#[derive(Clone)]
struct ConfiguredModel {
    protocol: ProviderApiProtocol,
    max_context_tokens: Option<u32>,
    max_output_tokens: u32,
    reasoning_variants: BTreeMap<String, ReasoningVariant>,
    default_variant: Option<String>,
    thinking_wire_format: ThinkingWireFormat,
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
    #[serde(skip)]
    api_key_override: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_map")]
    models: BTreeMap<String, ModelsFileModel>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsFileModel {
    api_protocol: String,
    max_context_tokens: Option<u32>,
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
    #[serde(default)]
    thinking_wire_format: Option<String>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
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
    ///
    /// runtime_handle 由已有异步宿主提供时复用该 runtime；否则 provider 自己拥有
    /// runtime。cache_path 为空时不启用持久 capability cache。
    pub fn capture<F>(
        get_env: F,
        runtime_handle: Option<tokio::runtime::Handle>,
        cache_path: Option<PathBuf>,
    ) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::capture_with_provider(get_env, move |config| {
            if let Some(runtime_handle) = runtime_handle.as_ref() {
                OpenAiProvider::new_with_runtime_handle_and_cache_path(
                    config,
                    PROVIDER_TIMEOUT_SECONDS,
                    cache_path.clone(),
                    runtime_handle.clone(),
                )
            } else {
                OpenAiProvider::new_with_cache_path(config, cache_path.clone())
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

fn validate_model_id(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.chars().count() > super::MAX_MODEL_ID_LENGTH
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '#'
        })
    {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is malformed"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

fn validate_provider_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    validate_identifier(value, label)
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

fn parse_thinking_wire_format(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<ThinkingWireFormat, ProviderError> {
    let format = match value.unwrap_or("thinking_type") {
        "thinking_type" => ThinkingWireFormat::ThinkingType,
        "enable_thinking" => ThinkingWireFormat::EnableThinking,
        _ => {
            return Err(configuration_error(
                "thinking_wire_format must be thinking_type or enable_thinking",
                "provider_configuration_invalid",
            ));
        }
    };
    if format == ThinkingWireFormat::EnableThinking
        && protocol != ProviderApiProtocol::OpenAiChatCompletions
    {
        return Err(configuration_error(
            "enable_thinking is only valid for Chat Completions",
            "provider_configuration_invalid",
        ));
    }
    Ok(format)
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

fn validate_catalog_limit(
    value: Option<u32>,
    label: &str,
    upper_bound: u32,
) -> Result<(), ProviderError> {
    if value.is_some_and(|value| value == 0 || value > upper_bound) {
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
                model_file.max_context_tokens,
                "max_context_tokens",
                MAX_CONFIGURED_CONTEXT_TOKENS,
            )?;
            validate_catalog_limit(
                Some(model_file.max_output_tokens),
                "max_output_tokens",
                MAX_CONFIGURED_OUTPUT_TOKENS,
            )?;
            if model_file
                .max_context_tokens
                .is_some_and(|context| model_file.max_output_tokens >= context)
            {
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
                    thinking_wire_format,
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
    validate_provider_identifier(provider_name, "provider id").map_err(|_| {
        model_selector_error(
            "model selector must contain a valid provider id",
            "provider_selector_invalid",
        )
    })?;
    validate_model_id(model_name, "model id").map_err(|_| {
        model_selector_error(
            "model selector must contain a valid model id",
            "provider_selector_invalid",
        )
    })?;
    if let Some(reasoning_effort) = reasoning_effort {
        validate_identifier(reasoning_effort, "reasoning variant").map_err(|_| {
            model_selector_error(
                "model selector must contain a valid reasoning variant",
                "provider_selector_invalid",
            )
        })?;
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
    let provider_instance = provider.provider.as_ref().ok_or_else(|| {
        provider.provider_error.clone().unwrap_or_else(|| {
            missing_provider_auth_error(Some(ProviderConfigSource::UserConfigFile))
        })
    })?;
    let requested_variant = parsed.reasoning_effort.or(model.default_variant.as_deref());
    let Some(requested_variant) = requested_variant else {
        return Ok(provider_instance.with_selected_model(super::SelectedModel {
            model_name: parsed.model_name.to_string(),
            api_protocol: model.protocol,
            max_context_tokens: model.max_context_tokens,
            max_output_tokens: model.max_output_tokens,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            thinking_wire_format: model.thinking_wire_format,
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
    Ok(provider_instance.with_selected_model(super::SelectedModel {
        model_name: parsed.model_name.to_string(),
        api_protocol: model.protocol,
        max_context_tokens: model.max_context_tokens,
        max_output_tokens: model.max_output_tokens,
        reasoning_variant: Some(requested_variant.to_string()),
        reasoning_enabled,
        wire_reasoning_effort: variant.wire_effort.clone(),
        thinking_wire_format: model.thinking_wire_format,
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
) -> Result<ConfiguredModel, ProviderError> {
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
        model_file.max_context_tokens,
        "max_context_tokens",
        MAX_CONFIGURED_CONTEXT_TOKENS,
    )?;
    validate_catalog_limit(
        Some(model_file.max_output_tokens),
        "max_output_tokens",
        MAX_CONFIGURED_OUTPUT_TOKENS,
    )?;
    if model_file
        .max_context_tokens
        .is_some_and(|context| model_file.max_output_tokens >= context)
    {
        return Err(configuration_error(
            "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
            "provider_configuration_invalid",
        ));
    }
    Ok(ConfiguredModel {
        protocol,
        max_context_tokens: model_file.max_context_tokens,
        max_output_tokens: model_file.max_output_tokens,
        reasoning_variants,
        default_variant: model_file.default_variant,
        thinking_wire_format,
        tool_reasoning_mode,
        supports_developer_role: model_file.supports_developer_role,
        supports_tool_choice: model_file.supports_tool_choice,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls,
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
    })
}

fn configured_model_from_user_file(
    model_file: &UserConfigModel,
) -> Result<ConfiguredModel, ProviderError> {
    let (Some(api_protocol), Some(max_output_tokens)) = (
        model_file.api_protocol.as_deref(),
        model_file.max_output_tokens,
    ) else {
        return Err(configuration_error(
            "model override is incomplete; api_protocol and max_output_tokens are required",
            "provider_configuration_invalid",
        ));
    };
    configured_model_from_file(ModelsFileModel {
        api_protocol: api_protocol.to_string(),
        max_context_tokens: model_file.max_context_tokens,
        max_output_tokens,
        reasoning_variants: model_file.reasoning_variants.clone(),
        default_variant: model_file.default_variant.clone(),
        tool_reasoning_history: model_file.tool_reasoning_history.clone(),
        supports_developer_role: model_file.supports_developer_role.unwrap_or(true),
        supports_tool_choice: model_file.supports_tool_choice.unwrap_or(true),
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls,
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
        thinking_wire_format: model_file.thinking_wire_format.clone(),
    })
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
            match configured_model_from_user_file(model_file) {
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

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserConfigFile {
    #[serde(default = "default_user_config_version")]
    version: u32,
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    auth_generation: Option<String>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    providers: BTreeMap<String, UserConfigProvider>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserConfigProvider {
    base_url: String,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserConfigModel {
    #[serde(default)]
    api_protocol: Option<String>,
    #[serde(default)]
    max_context_tokens: Option<u32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    #[serde(default)]
    default_variant: Option<String>,
    #[serde(default)]
    tool_reasoning_history: Option<String>,
    #[serde(default)]
    supports_developer_role: Option<bool>,
    #[serde(default)]
    supports_tool_choice: Option<bool>,
    #[serde(default)]
    requires_reasoning_content_for_tool_calls: bool,
    #[serde(default)]
    requires_assistant_content_for_tool_calls: bool,
    #[serde(default)]
    thinking_wire_format: Option<String>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserAuthFile {
    #[serde(default = "default_auth_schema_version")]
    schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    providers: BTreeMap<String, UserAuthProvider>,
}

impl Default for UserAuthFile {
    fn default() -> Self {
        Self {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserAuthProvider {
    api_key: String,
}

#[derive(Clone)]
struct UserConfigData {
    directory: PathBuf,
    config: UserConfigFile,
    auth: UserAuthFile,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserModelsCacheFile {
    schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    providers: BTreeMap<String, UserModelsCacheRecord>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UserModelsCacheRecord {
    endpoint_sha256: String,
    fetched_at_unix_seconds: u64,
    #[serde(deserialize_with = "deserialize_unique_vec")]
    model_ids: Vec<String>,
}

fn default_user_config_version() -> u32 {
    1
}

fn default_auth_schema_version() -> u32 {
    USER_AUTH_SCHEMA_VERSION
}

fn deserialize_unique_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    let mut seen = std::collections::BTreeSet::new();
    if values.iter().any(|value| !seen.insert(value)) {
        return Err(de::Error::custom("duplicate model id"));
    }
    Ok(values)
}

fn user_config_error(message: impl Into<String>) -> ProviderError {
    configuration_error(message, "provider_configuration_invalid")
}

/// Resolve the user-level directory shared by all worktrees.
pub fn user_config_directory() -> Option<PathBuf> {
    user_config_directory_result().ok().flatten()
}

fn user_config_directory_result() -> Result<Option<PathBuf>, ProviderError> {
    let explicit_home = std::env::var_os("SINGULARITY_HOME");
    let home = explicit_home
        .clone()
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"));
    let Some(home) = home else {
        return Ok(None);
    };
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Err(user_config_error(
            "SINGULARITY_HOME must be a non-empty absolute path",
        ));
    }
    let home = normalize_absolute_path(&home)?;
    if explicit_home.is_some() {
        ensure_home_not_repo_controlled(&home)?;
        ensure_no_reparse_components(&home, true)?;
        Ok(Some(home))
    } else {
        let directory = home.join(USER_CONFIG_DIR_NAME);
        ensure_no_reparse_components(&directory, true)?;
        Ok(Some(directory))
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, ProviderError> {
    if !path.is_absolute() {
        return Err(user_config_error(
            "user config directory must be an absolute path",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if !normalized.is_absolute() || normalized.as_os_str().is_empty() {
        return Err(user_config_error(
            "user config directory could not be normalized",
        ));
    }
    Ok(normalized)
}

fn ensure_home_not_repo_controlled(path: &Path) -> Result<(), ProviderError> {
    let cwd = std::env::current_dir()
        .map_err(|_| user_config_error("current directory could not be read"))?;
    let repo = repository_boundary_root(&cwd)?;
    ensure_home_outside_root(path, &repo)
}

fn repository_boundary_root(cwd: &Path) -> Result<PathBuf, ProviderError> {
    let cwd = normalize_absolute_path(cwd)?;
    let mut current = cwd.clone();
    loop {
        let marker = current.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                return canonicalize_existing_prefix(&current);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(user_config_error(
                    "repository marker could not be inspected",
                ));
            }
        }
        if !current.pop() {
            break;
        }
    }
    canonicalize_existing_prefix(&cwd)
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, ProviderError> {
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    user_config_error("user config path could not be canonicalized")
                })?;
                missing.push(component.to_os_string());
                if !current.pop() {
                    return Err(user_config_error(
                        "user config path could not be canonicalized",
                    ));
                }
            }
            Err(_) => {
                return Err(user_config_error(
                    "user config path could not be canonicalized",
                ));
            }
        }
    }
}

fn ensure_home_outside_root(path: &Path, root: &Path) -> Result<(), ProviderError> {
    let canonical_home = canonicalize_existing_prefix(path)?;
    let canonical_root = canonicalize_existing_prefix(root)?;
    if path_starts_with(&canonical_home, &canonical_root) {
        return Err(user_config_error(
            "SINGULARITY_HOME must not be inside the current repository",
        ));
    }
    Ok(())
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path_components = path.components();
        for prefix_component in prefix.components() {
            let Some(path_component) = path_components.next() else {
                return false;
            };
            if !path_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&prefix_component.as_os_str().to_string_lossy())
            {
                return false;
            }
        }
        true
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

fn ensure_no_reparse_components(
    path: &Path,
    allow_missing_tail: bool,
) -> Result<(), ProviderError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        #[cfg(windows)]
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing_tail && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(_) => {
                return Err(user_config_error(
                    "user config path components could not be inspected",
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(user_config_error(
                "user config path must not contain a symlink",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(user_config_error(
                    "user config path must not contain a reparse point",
                ));
            }
        }
    }
    Ok(())
}

fn user_config_layer() -> Option<ProviderConfigLayer> {
    match read_user_config_data() {
        Ok(Some(user_config)) => {
            let mut layer = ProviderConfigLayer {
                user_config: Some(user_config.clone()),
                user_config_error: None,
                ..ProviderConfigLayer::default()
            };
            let default_provider = user_config
                .config
                .default_provider
                .clone()
                .or_else(|| {
                    user_config
                        .config
                        .default_model
                        .as_deref()
                        .and_then(|selector| parse_model_selector(selector).ok())
                        .map(|selector| selector.provider_name.to_string())
                })
                .or_else(|| user_config.config.providers.keys().next().cloned());
            if let Some(provider_name) = default_provider
                && let Some(provider) = user_config.config.providers.get(&provider_name)
            {
                layer.provider_name = Some(provider_name.clone());
                layer.base_url = Some(provider.base_url.clone());
                layer.api_key = user_config
                    .auth
                    .providers
                    .get(&provider_name)
                    .map(|provider| provider.api_key.clone());
                layer.model_name = user_config.config.default_model.clone();
            }
            Some(layer)
        }
        Ok(None) => None,
        Err(error) => Some(ProviderConfigLayer {
            user_config_error: Some(error),
            ..ProviderConfigLayer::default()
        }),
    }
}

fn read_private_auth_file(path: &Path) -> Result<UserAuthFile, ProviderError> {
    let mut file = open_user_config_file(path, true)?;
    ensure_private_secret_handle(&file)?;
    let text = read_bounded_text_from_file(&mut file, super::MAX_DISCOVERY_RESPONSE_BYTES)
        .map_err(|error| match error {
            BoundedTextError::TooLarge => {
                user_config_error("user provider auth exceeds the size limit")
            }
            BoundedTextError::Read(_) => user_config_error("user provider auth could not be read"),
        })?;
    let auth: UserAuthFile = serde_json::from_str(&text)
        .map_err(|_| user_config_error("user provider auth is invalid JSON"))?;
    if auth.schema_version != USER_AUTH_SCHEMA_VERSION {
        return Err(user_config_error("unsupported user provider auth version"));
    }
    Ok(auth)
}

#[cfg(test)]
fn ensure_private_secret_file(path: &Path) -> Result<(), ProviderError> {
    let file = open_user_config_file(path, true)?;
    ensure_private_secret_handle(&file)
}

fn open_user_config_file(path: &Path, private: bool) -> Result<std::fs::File, ProviderError> {
    ensure_no_reparse_components(path, false)?;
    let parent = path
        .parent()
        .ok_or_else(|| user_config_error("user provider config path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_config_error("user provider config path has no file name"))?;
    let directory = CapabilityDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|_| user_config_error("user provider auth could not be opened"))?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
            READ_CONTROL,
        };
        options
            .access_mode(FILE_GENERIC_READ | if private { READ_CONTROL } else { 0 })
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = directory
        .open_with(name, &options)
        .map_err(|_| user_config_error("user provider auth could not be opened"))?;
    let file = file.into_std();
    ensure_regular_user_config_handle(&file)?;
    if private {
        ensure_private_secret_handle(&file)?;
    }
    Ok(file)
}

fn ensure_regular_user_config_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file
        .metadata()
        .map_err(|_| user_config_error("user provider config metadata could not be checked"))?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "user provider config is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(user_config_error(
                "user provider config is not a regular file",
            ));
        }
    }
    Ok(())
}

fn ensure_private_secret_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    ensure_regular_user_config_handle(file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata().map_err(|_| {
            user_config_error("user provider auth permissions could not be checked")
        })?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
    }
    #[cfg(windows)]
    windows_auth_acl::ensure_owner_only_handle(file)?;
    Ok(())
}

fn create_private_secret_file(path: &Path) -> Result<std::fs::File, ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_config_error("user provider config path has no parent"))?;
    ensure_no_reparse_components(parent, false)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
            )
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            )
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|_| user_config_error("user provider auth file could not be created"))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_identity {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[repr(C)]
    struct FileTime {
        _low_date_time: u32,
        _high_date_time: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        _volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        number_of_links: u32,
        _file_index_high: u32,
        _file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: RawHandle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    pub(super) fn read(file: &File) -> io::Result<(u32, u32)> {
        let mut information = MaybeUninit::<ByHandleFileInformation>::zeroed();
        // SAFETY: `file` owns a live Windows handle and `information` points to
        // writable storage of the exact C ABI layout required by the API.
        let result = unsafe {
            get_file_information_by_handle(file.as_raw_handle(), information.as_mut_ptr())
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Windows initialized the complete structure when the call
        // returned nonzero.
        let information = unsafe { information.assume_init() };
        Ok((information.file_attributes, information.number_of_links))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_auth_acl {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{null, null_mut};

    use super::{ProviderError, user_config_error};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
        SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING, GetAclInformation, GetLengthSid,
        GetSecurityDescriptorDacl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;

    /// Apply and verify the owner-only DACL through the already-open file handle.
    /// The caller must set this before writing any secret bytes.
    pub(super) fn set_owner_only_handle(file: &File) -> Result<(), ProviderError> {
        let mut sid = current_user_sid()?;
        let trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_mut_ptr() as *mut u16,
        };
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            grfAccessMode: SET_ACCESS,
            grfInheritance: 0,
            Trustee: trustee,
        };
        let mut dacl: *mut ACL = null_mut();
        let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
        if status != ERROR_SUCCESS || dacl.is_null() {
            if !dacl.is_null() {
                unsafe { LocalFree(dacl as HLOCAL) };
            }
            return Err(user_config_error(
                "user provider auth permissions could not be set",
            ));
        }
        let handle = file.as_raw_handle() as HANDLE;
        let status = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        unsafe { LocalFree(dacl as HLOCAL) };
        if status != ERROR_SUCCESS {
            return Err(user_config_error(
                "user provider auth permissions could not be set",
            ));
        }
        ensure_owner_only_handle(file)
    }

    /// Verify the owner and DACL of the exact object pinned by `file`.
    pub(super) fn ensure_owner_only_handle(file: &File) -> Result<(), ProviderError> {
        let current_sid = current_user_sid()?;
        let handle = file.as_raw_handle() as HANDLE;
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS || descriptor.is_null() {
            if !descriptor.is_null() {
                unsafe { LocalFree(descriptor as HLOCAL) };
            }
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let result = inspect_descriptor(descriptor, owner, dacl, &current_sid);
        unsafe { LocalFree(descriptor as HLOCAL) };
        result
    }

    fn inspect_descriptor(
        descriptor: *mut c_void,
        owner: *mut c_void,
        dacl: *mut ACL,
        current_sid: &[u8],
    ) -> Result<(), ProviderError> {
        if owner.is_null() || current_sid.is_empty() {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let current_sid = current_sid.as_ptr() as *mut c_void;
        if unsafe { EqualSid(owner, current_sid) } == 0 || dacl.is_null() {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let mut dacl_present = 0;
        let mut descriptor_dacl = null_mut();
        let mut dacl_defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &mut dacl_present,
                &mut descriptor_dacl,
                &mut dacl_defaulted,
            )
        } == 0
            || dacl_present == 0
            || descriptor_dacl.is_null()
            || descriptor_dacl != dacl
        {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            GetAclInformation(
                dacl,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
            || info.AceCount != 1
        {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let mut ace = null_mut();
        if unsafe { windows_sys::Win32::Security::GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null()
        {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let sid = &allowed.SidStart as *const u32 as *mut c_void;
        if unsafe { EqualSid(sid, current_sid) } == 0 {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        let mut mask = allowed.Mask;
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        };
        unsafe { windows_sys::Win32::Security::MapGenericMask(&mut mask, &mapping) };
        if (mask & (FILE_GENERIC_READ | FILE_GENERIC_WRITE))
            != (FILE_GENERIC_READ | FILE_GENERIC_WRITE)
        {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
        Ok(())
    }

    fn current_user_sid() -> Result<Vec<u8>, ProviderError> {
        let mut token: HANDLE = 0;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let result = current_user_sid_from_token(token);
        unsafe { CloseHandle(token) };
        result
    }

    fn current_user_sid_from_token(token: HANDLE) -> Result<Vec<u8>, ProviderError> {
        let mut length = 0;
        let _ = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
        let length = usize::try_from(length).map_err(|_| {
            user_config_error("user provider auth permissions could not be checked")
        })?;
        if length == 0 || length > MAX_TOKEN_INFORMATION_BYTES {
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let mut buffer = vec![0u8; length];
        let mut return_length = length as u32;
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                length as u32,
                &mut return_length,
            )
        } == 0
        {
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let token_user = unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER) };
        if token_user.User.Sid.is_null() {
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let sid_length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
        if sid_length == 0 {
            return Err(user_config_error(
                "user provider auth permissions could not be checked",
            ));
        }
        let sid =
            unsafe { std::slice::from_raw_parts(token_user.User.Sid as *const u8, sid_length) };
        Ok(sid.to_vec())
    }
}

fn read_user_config_data() -> Result<Option<UserConfigData>, ProviderError> {
    let Some(directory) = user_config_directory_result()? else {
        return Ok(None);
    };
    read_user_config_data_from_directory(directory)
}

fn read_user_config_data_from_directory(
    directory: PathBuf,
) -> Result<Option<UserConfigData>, ProviderError> {
    let config_path = directory.join(USER_CONFIG_FILE_NAME);
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(user_config_error(
                "user provider config directory is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(user_config_error(
                "user provider config directory could not be inspected",
            ));
        }
    }
    ensure_no_reparse_components(&directory, false)?;
    if !path_exists_or_missing(&config_path, "user provider config could not be inspected")? {
        return Ok(None);
    }
    ensure_no_reparse_components(&config_path, false)?;
    let mut config_file = open_user_config_file(&config_path, false)
        .map_err(|_| user_config_error("user provider config could not be opened"))?;
    let config_text =
        read_bounded_text_from_file(&mut config_file, super::MAX_DISCOVERY_RESPONSE_BYTES)
            .map_err(|error| match error {
                BoundedTextError::TooLarge => {
                    user_config_error("user provider config exceeds the size limit")
                }
                BoundedTextError::Read(_) => {
                    user_config_error("user provider config could not be read")
                }
            })?;
    let config: UserConfigFile = serde_json::from_str(&config_text)
        .map_err(|_| user_config_error("user provider config is invalid JSON"))?;
    if config.version != 1 {
        return Err(user_config_error(
            "unsupported user provider config version",
        ));
    }
    let auth = if let Some(generation) = config.auth_generation.as_deref() {
        let auth_path = auth_generation_path(&directory, generation)?;
        read_private_auth_file(&auth_path)?
    } else {
        UserAuthFile::default()
    };
    Ok(Some(UserConfigData {
        directory,
        config,
        auth,
    }))
}

fn auth_generation_path(directory: &Path, generation: &str) -> Result<PathBuf, ProviderError> {
    if !generation.starts_with(USER_AUTH_GENERATION_PREFIX)
        || !generation.ends_with(".json")
        || generation.contains(['/', '\\', ':'])
        || generation
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(user_config_error(
            "user provider auth generation reference is invalid",
        ));
    }
    let path = directory.join(generation);
    ensure_no_reparse_components(directory, false)?;
    if path_exists_or_missing(&path, "user provider auth path could not be inspected")? {
        ensure_no_reparse_components(&path, false)?;
    }
    Ok(path)
}

fn path_exists_or_missing(path: &Path, message: &str) -> Result<bool, ProviderError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(user_config_error(message)),
    }
}

fn endpoint_fingerprint(base_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let identity = normalized_endpoint_identity(base_url).unwrap_or_else(|_| base_url.to_string());
    digest.update(identity.as_bytes());
    format!("{:x}", digest.finalize())
}

fn user_model_override_is_selectable(model: &UserConfigModel) -> bool {
    configured_model_from_user_file(model).is_ok()
}

fn unix_timestamp_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

struct ModelsCacheLoad {
    cache: UserModelsCacheFile,
    status: ModelCacheStatus,
}

fn load_models_cache(path: &Path) -> ModelsCacheLoad {
    let empty_cache = || UserModelsCacheFile {
        schema_version: USER_MODELS_CACHE_SCHEMA_VERSION,
        providers: BTreeMap::new(),
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::NotPresent,
            };
        }
        Ok(metadata) if metadata.is_file() => {}
        _ => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::ReadFailed,
            };
        }
    }
    let text = match read_bounded_text(path, super::MAX_DISCOVERY_RESPONSE_BYTES) {
        Ok(text) => text,
        Err(error) => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: if error.is_invalid_data() {
                    ModelCacheStatus::Invalid
                } else {
                    ModelCacheStatus::ReadFailed
                },
            };
        }
    };
    let cache: UserModelsCacheFile = match serde_json::from_str(&text) {
        Ok(cache) => cache,
        Err(_) => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::Invalid,
            };
        }
    };
    if cache.schema_version != USER_MODELS_CACHE_SCHEMA_VERSION {
        return ModelsCacheLoad {
            cache: empty_cache(),
            status: ModelCacheStatus::Invalid,
        };
    }
    if cache.providers.len() > super::MAX_DISCOVERED_MODEL_IDS
        || cache.providers.iter().any(|(provider_name, record)| {
            validate_provider_identifier(provider_name, "provider id").is_err()
                || record.endpoint_sha256.len() != 64
                || !record
                    .endpoint_sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
                || record.model_ids.len() > super::MAX_DISCOVERED_MODEL_IDS
                || record
                    .model_ids
                    .iter()
                    .any(|model_id| validate_model_id(model_id, "model id").is_err())
        })
    {
        return ModelsCacheLoad {
            cache: empty_cache(),
            status: ModelCacheStatus::Invalid,
        };
    }
    ModelsCacheLoad {
        cache,
        status: ModelCacheStatus::Valid,
    }
}

enum BoundedTextError {
    TooLarge,
    Read(std::io::Error),
}

impl BoundedTextError {
    fn is_invalid_data(&self) -> bool {
        match self {
            Self::TooLarge => true,
            Self::Read(error) => error.kind() == std::io::ErrorKind::InvalidData,
        }
    }
}

fn read_bounded_text(path: &Path, max_bytes: usize) -> Result<String, BoundedTextError> {
    let mut file = std::fs::File::open(path).map_err(BoundedTextError::Read)?;
    read_bounded_text_from_file(&mut file, max_bytes)
}

fn read_bounded_text_from_file(
    file: &mut std::fs::File,
    max_bytes: usize,
) -> Result<String, BoundedTextError> {
    use std::io::Read;
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let metadata_len = file.metadata().map_err(BoundedTextError::Read)?.len();
    if metadata_len > max_bytes_u64 {
        return Err(BoundedTextError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata_len).unwrap_or(max_bytes));
    file.take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedTextError::Read)?;
    if bytes.len() > max_bytes {
        return Err(BoundedTextError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| {
        BoundedTextError::Read(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "input was not UTF-8",
        ))
    })
}

fn write_json_file(path: &Path, contents: &str, secret: bool) -> Result<(), ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_config_error("user provider config path has no parent"))?;
    ensure_no_reparse_components(parent, true)?;
    std::fs::create_dir_all(parent)
        .map_err(|_| user_config_error("user provider config directory could not be created"))?;
    ensure_no_reparse_components(parent, false)?;
    if path_exists_or_missing(path, "user provider config path could not be inspected")? {
        ensure_no_reparse_components(path, false)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = if secret {
            let file = create_private_secret_file(&temporary)?;
            #[cfg(windows)]
            windows_auth_acl::set_owner_only_handle(&file)?;
            ensure_private_secret_handle(&file)?;
            file
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&temporary)
                .map_err(|_| user_config_error("user provider config file could not be opened"))?
        };
        use std::io::Write;
        file.write_all(contents.as_bytes())
            .map_err(|_| user_config_error("user provider config file could not be written"))?;
        file.sync_all()
            .map_err(|_| user_config_error("user provider config file could not be synced"))?;
        if secret {
            ensure_private_secret_handle(&file)?;
        }
        drop(file);
        atomic_replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn atomic_replace_file(from: &Path, to: &Path) -> Result<(), ProviderError> {
    #[cfg(windows)]
    {
        windows_atomic_replace(from, to)?;
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
            .map_err(|_| user_config_error("user provider config file could not be committed"))?;
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_atomic_replace(from: &Path, to: &Path) -> Result<(), ProviderError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
    from_wide.push(0);
    let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
    to_wide.push(0);
    if unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(user_config_error(
            "user provider config file could not be committed",
        ));
    }
    Ok(())
}

struct ConfigWriterLock {
    _file: std::fs::File,
}

fn acquire_config_writer_lock(directory: &Path) -> Result<ConfigWriterLock, ProviderError> {
    ensure_no_reparse_components(directory, false)?;
    let path = directory.join(".config.lock");
    let (file, created) = match config_writer_lock_options(true).open(&path) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_no_reparse_components(&path, false)?;
            let file = config_writer_lock_options(false).open(&path).map_err(|_| {
                user_config_error("provider config writer lock could not be acquired")
            })?;
            (file, false)
        }
        Err(_) => {
            return Err(user_config_error(
                "provider config writer lock could not be acquired",
            ));
        }
    };
    ensure_config_writer_lock_identity(&file)?;
    #[cfg(unix)]
    ensure_private_lock_handle(&file)?;
    #[cfg(windows)]
    let acl_needs_repair = windows_auth_acl::ensure_owner_only_handle(&file).is_err();
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(user_config_error(
                "another provider config import is in progress",
            ));
        }
        Err(std::fs::TryLockError::Error(_)) => {
            return Err(user_config_error(
                "provider config writer lock could not be acquired",
            ));
        }
    }
    #[cfg(windows)]
    if created || acl_needs_repair {
        windows_auth_acl::set_owner_only_handle(&file)?;
    }
    ensure_private_lock_handle(&file)?;
    Ok(ConfigWriterLock { _file: file })
}

fn config_writer_lock_options(create_new: bool) -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE
                    | windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                    | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
            )
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            )
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
}

fn ensure_config_writer_lock_identity(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file.metadata().map_err(|_| {
        user_config_error("provider config writer lock identity could not be checked")
    })?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "provider config writer lock is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(user_config_error(
                "provider config writer lock must not have multiple hard links",
            ));
        }
    }
    #[cfg(windows)]
    {
        let (file_attributes, number_of_links) =
            windows_file_identity::read(file).map_err(|_| {
                user_config_error("provider config writer lock identity could not be checked")
            })?;
        if file_attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(user_config_error(
                "provider config writer lock must not be a reparse point",
            ));
        }
        if number_of_links != 1 {
            return Err(user_config_error(
                "provider config writer lock must not have multiple hard links",
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(user_config_error(
        "provider config writer lock identity is unsupported on this platform",
    ));
    Ok(())
}

fn ensure_private_lock_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file.metadata().map_err(|_| {
        user_config_error("provider config writer lock permissions could not be checked")
    })?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "provider config writer lock is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(user_config_error(
                "provider config writer lock is not owner-only",
            ));
        }
    }
    #[cfg(windows)]
    windows_auth_acl::ensure_owner_only_handle(file)?;
    Ok(())
}

fn new_auth_generation_name() -> String {
    format!(
        "{}{}.json",
        USER_AUTH_GENERATION_PREFIX,
        Uuid::new_v4().simple()
    )
}

fn write_new_auth_generation(
    directory: &Path,
    generation: &str,
    contents: &str,
) -> Result<PathBuf, ProviderError> {
    let path = auth_generation_path(directory, generation)?;
    let mut file = create_private_secret_file(&path)?;
    let created = true;
    let result = (|| {
        #[cfg(windows)]
        windows_auth_acl::set_owner_only_handle(&file)?;
        ensure_private_secret_handle(&file)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())
            .map_err(|_| user_config_error("user provider auth could not be written"))?;
        file.sync_all()
            .map_err(|_| user_config_error("user provider auth could not be synced"))?;
        ensure_private_secret_handle(&file)?;
        Ok(())
    })();
    drop(file);
    if result.is_err() && created {
        let _ = std::fs::remove_file(&path);
    }
    result.map(|()| path)
}

/// Import the current or explicitly named dotenv file into split user config.
/// The API key is read from the file and written only to a versioned auth
/// generation; it is never accepted as a function argument or serialized in
/// the catalog.
pub fn import_env_to_user_config(
    path: Option<&Path>,
) -> Result<UserConfigImportResult, ProviderError> {
    let env_path = match path {
        Some(path) if path.is_file() => path.to_path_buf(),
        Some(_) => return Err(user_config_error("explicit dotenv file could not be read")),
        None => {
            let current_dir = std::env::current_dir()
                .map_err(|_| user_config_error("current directory could not be read"))?;
            find_import_env_file(&current_dir)
                .ok_or_else(|| user_config_error("no .env file was found"))?
        }
    };
    let layer = read_import_env_layer(&env_path);
    let base_url = layer
        .base_url
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_BASE_URL is required for import-env"))?;
    let api_key = layer
        .api_key
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_API_KEY is required for import-env"))?;
    let model_value = layer
        .model_name
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_MODEL is required for import-env"))?;
    validate_base_url(Some(&base_url), Some(ProviderConfigSource::UserConfigFile))?;
    validate_provider_value(
        Some(&api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let provider_name = layer
        .provider_name
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
    validate_provider_identifier(&provider_name, "provider id")?;
    validate_provider_value(
        Some(&model_value),
        ENV_MODEL,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let (default_selector, model_name) = parse_import_model_selector(&model_value, &provider_name)?;
    let directory = user_config_directory_result()?
        .ok_or_else(|| user_config_error("user config directory is unavailable"))?;
    ensure_no_reparse_components(&directory, true)?;
    std::fs::create_dir_all(&directory)
        .map_err(|_| user_config_error("user provider config directory could not be created"))?;
    ensure_no_reparse_components(&directory, false)?;
    let empty_existing = || UserConfigData {
        directory: directory.clone(),
        config: UserConfigFile {
            version: 1,
            default_provider: None,
            default_model: None,
            auth_generation: None,
            providers: BTreeMap::new(),
        },
        auth: UserAuthFile::default(),
    };
    let existing_before_lock = read_user_config_data()?;
    reject_import_endpoint_change(existing_before_lock.as_ref(), &provider_name, &base_url)?;
    let _writer_lock = acquire_config_writer_lock(&directory)?;
    let existing = read_user_config_data()?.unwrap_or_else(empty_existing);
    reject_import_endpoint_change(Some(&existing), &provider_name, &base_url)?;
    let mut config = existing.config;
    config.version = 1;
    config.default_provider = Some(provider_name.clone());
    config.default_model = Some(default_selector.clone());
    let provider = config
        .providers
        .entry(provider_name.clone())
        .or_insert_with(|| UserConfigProvider {
            base_url: base_url.clone(),
            models: BTreeMap::new(),
        });
    provider.base_url = base_url.clone();
    let model = provider.models.entry(model_name.clone()).or_default();
    if let Some(variant) = parse_model_selector(&default_selector)?.reasoning_effort
        && !model.reasoning_variants.contains_key(variant)
    {
        return Err(user_config_error(
            "reasoning variant must already be explicitly declared before import",
        ));
    }
    let mut auth = existing.auth;
    auth.schema_version = USER_AUTH_SCHEMA_VERSION;
    auth.providers
        .insert(provider_name.clone(), UserAuthProvider { api_key });
    validate_imported_user_config(&config, &auth)?;
    let selectable = imported_model_is_selectable(
        &config,
        &auth,
        &provider_name,
        &model_name,
        parse_model_selector(&default_selector)?.reasoning_effort,
    );
    let auth_text = serde_json::to_string_pretty(&auth)
        .map_err(|_| user_config_error("user provider auth could not be serialized"))?;
    let generation = new_auth_generation_name();
    config.auth_generation = Some(generation.clone());
    let config_text = serde_json::to_string_pretty(&config)
        .map_err(|_| user_config_error("user provider config could not be serialized"))?;
    let config_path = directory.join(USER_CONFIG_FILE_NAME);
    let auth_path = write_new_auth_generation(&directory, &generation, &auth_text)?;
    if let Err(error) = write_json_file(&config_path, &config_text, false) {
        let _ = std::fs::remove_file(&auth_path);
        return Err(error);
    }
    Ok(UserConfigImportResult {
        config_path: config_path.to_string_lossy().to_string(),
        auth_path: auth_path.to_string_lossy().to_string(),
        provider_name,
        default_selector: Some(default_selector),
        selectable,
    })
}

fn reject_import_endpoint_change(
    existing: Option<&UserConfigData>,
    provider_name: &str,
    base_url: &str,
) -> Result<(), ProviderError> {
    let Some(existing_provider) =
        existing.and_then(|data| data.config.providers.get(provider_name))
    else {
        return Ok(());
    };
    let old_identity = normalized_endpoint_identity(&existing_provider.base_url)?;
    let new_identity = normalized_endpoint_identity(base_url)?;
    if old_identity != new_identity {
        return Err(user_config_error(
            "provider id already points to a different endpoint; use a distinct provider id or edit config explicitly",
        ));
    }
    Ok(())
}

fn parse_import_model_selector(
    model_value: &str,
    provider_name: &str,
) -> Result<(String, String), ProviderError> {
    let provider_prefix = format!("{provider_name}/");
    if model_value.starts_with(&provider_prefix) {
        let parsed = parse_model_selector(model_value)?;
        validate_provider_identifier(parsed.provider_name, "provider id")?;
        validate_model_id(parsed.model_name, "model id")?;
        if let Some(variant) = parsed.reasoning_effort {
            validate_identifier(variant, "reasoning variant")?;
        }
        if parsed.provider_name != provider_name {
            return Err(user_config_error(
                "SINGULARITY_MODEL provider does not match SINGULARITY_MODEL_PROVIDER",
            ));
        }
        Ok((model_value.to_string(), parsed.model_name.to_string()))
    } else {
        validate_model_id(model_value, "model id")?;
        Ok((
            format!("{provider_name}/{model_value}"),
            model_value.to_string(),
        ))
    }
}

fn validate_imported_user_config(
    config: &UserConfigFile,
    auth: &UserAuthFile,
) -> Result<(), ProviderError> {
    let default_provider = config
        .default_provider
        .as_deref()
        .ok_or_else(|| user_config_error("user provider config must declare default_provider"))?;
    let default_model = config
        .default_model
        .as_deref()
        .ok_or_else(|| user_config_error("user provider config must declare default_model"))?;
    let parsed = parse_model_selector(default_model)?;
    if parsed.provider_name != default_provider {
        return Err(user_config_error(
            "default_provider does not match default_model",
        ));
    }
    let provider = config
        .providers
        .get(default_provider)
        .ok_or_else(|| user_config_error("default_model references an unknown provider"))?;
    validate_provider_identifier(default_provider, "provider id")?;
    validate_base_url(
        Some(&provider.base_url),
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let model = provider
        .models
        .get(parsed.model_name)
        .ok_or_else(|| user_config_error("default_model references an unknown model"))?;
    validate_model_id(parsed.model_name, "model id")?;
    if let Some(variant) = parsed.reasoning_effort
        && !model.reasoning_variants.contains_key(variant)
    {
        return Err(user_config_error(
            "default_model references an unknown reasoning variant",
        ));
    }
    let api_key = auth
        .providers
        .get(default_provider)
        .map(|provider| provider.api_key.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("default provider api_key is required"))?;
    validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )
}

fn imported_model_is_selectable(
    config: &UserConfigFile,
    auth: &UserAuthFile,
    provider_name: &str,
    model_name: &str,
    reasoning_variant: Option<&str>,
) -> bool {
    let Some(provider) = config.providers.get(provider_name) else {
        return false;
    };
    let Some(model) = provider.models.get(model_name) else {
        return false;
    };
    if validate_base_url(
        Some(&provider.base_url),
        Some(ProviderConfigSource::UserConfigFile),
    )
    .is_err()
    {
        return false;
    }
    let Some(api_key) = auth
        .providers
        .get(provider_name)
        .map(|provider| provider.api_key.as_str())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    if validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )
    .is_err()
        || configured_model_from_user_file(model).is_err()
    {
        return false;
    }
    reasoning_variant.is_none_or(|variant| model.reasoning_variants.contains_key(variant))
}

fn validate_discovered_model_ids(model_ids: Vec<String>) -> Result<Vec<String>, ProviderError> {
    if model_ids.len() > super::MAX_DISCOVERED_MODEL_IDS {
        return Err(configuration_error(
            "provider models response exceeded the model id safety limit",
            "provider_configuration_invalid",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for model_id in &model_ids {
        validate_model_id(model_id, "discovered model id")?;
        if !seen.insert(model_id) {
            return Err(configuration_error(
                "provider models response contained duplicate model ids",
                "provider_configuration_invalid",
            ));
        }
    }
    if model_ids.is_empty() {
        return Err(configuration_error(
            "provider models response did not contain model ids",
            "provider_configuration_invalid",
        ));
    }
    Ok(model_ids)
}

fn public_diagnostic(error: &ProviderError) -> String {
    error
        .message
        .chars()
        .map(|character| match character {
            '\r' => ' ',
            '\n' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

/// Read and, when stale or requested, refresh the user-level `/models` ids.
pub fn read_user_model_catalog(refresh: bool) -> Result<UserModelCatalog, ProviderError> {
    let Some(user_config) = read_user_config_data()? else {
        return Ok(UserModelCatalog {
            default_selector: None,
            cache_status: ModelCacheStatus::NotPresent,
            providers: Vec::new(),
        });
    };
    let cache_path = user_config.directory.join(USER_MODELS_CACHE_FILE_NAME);
    let cache_load = load_models_cache(&cache_path);
    let mut cache = cache_load.cache;
    let mut cache_status = cache_load.status;
    let mut cache_changed = false;
    let now = unix_timestamp_seconds();
    let mut provider_catalogs = Vec::new();
    for (provider_name, provider_file) in &user_config.config.providers {
        if validate_provider_identifier(provider_name, "provider id").is_err() {
            cache_status = ModelCacheStatus::Invalid;
            continue;
        }
        let mut diagnostics = Vec::new();
        let base_url_valid = match validate_base_url(
            Some(&provider_file.base_url),
            Some(ProviderConfigSource::UserConfigFile),
        ) {
            Ok(()) => true,
            Err(_) => {
                diagnostics.push("provider endpoint is invalid".to_string());
                false
            }
        };
        let api_key = user_config
            .auth
            .providers
            .get(provider_name)
            .map(|provider| provider.api_key.clone())
            .filter(|value| !value.is_empty());
        let auth_valid = api_key.as_deref().is_some_and(|api_key| {
            validate_provider_value(
                Some(api_key),
                ENV_API_KEY,
                Some(ProviderConfigSource::UserConfigFile),
            )
            .is_ok()
        });
        if api_key.is_some() && !auth_valid {
            diagnostics.push("provider authentication is invalid".to_string());
        }
        let explicit_ids = provider_file
            .models
            .keys()
            .filter(|id| validate_model_id(id, "model id").is_ok())
            .cloned()
            .collect::<Vec<_>>();
        if provider_file
            .models
            .keys()
            .any(|id| validate_model_id(id, "model id").is_err())
        {
            diagnostics.push("one or more model ids are invalid".to_string());
        }
        let selectable_ids = provider_file
            .models
            .iter()
            .filter_map(|(id, model)| {
                (validate_model_id(id, "model id").is_ok()
                    && base_url_valid
                    && auth_valid
                    && user_model_override_is_selectable(model))
                .then_some(id.clone())
            })
            .collect::<std::collections::BTreeSet<_>>();
        if explicit_ids.iter().any(|id| {
            provider_file
                .models
                .get(id)
                .is_some_and(|model| !user_model_override_is_selectable(model))
        }) {
            diagnostics.push("one or more model overrides are incomplete or invalid".to_string());
        }
        let endpoint_hash = if base_url_valid {
            endpoint_fingerprint(&provider_file.base_url)
        } else {
            String::new()
        };
        let cached_ids = cache
            .providers
            .get(provider_name)
            .filter(|record| {
                base_url_valid
                    && record.endpoint_sha256 == endpoint_hash
                    && record.model_ids.len() <= MAX_DISCOVERED_MODEL_IDS
            })
            .map(|record| record.model_ids.clone());
        let cached_fetched_at = cache
            .providers
            .get(provider_name)
            .filter(|record| {
                base_url_valid
                    && record.endpoint_sha256 == endpoint_hash
                    && record.model_ids.len() <= MAX_DISCOVERED_MODEL_IDS
            })
            .map(|record| record.fetched_at_unix_seconds);
        let fresh = cached_fetched_at.is_some_and(|fetched_at| {
            !refresh && fetched_at <= now && now - fetched_at <= USER_MODELS_CACHE_TTL_SECONDS
        });
        let cached_ids_for_fallback = cached_ids.clone();
        let had_cached_ids = cached_ids_for_fallback.is_some();
        let (discovered_ids, discovery, discovery_error) =
            if !base_url_valid || api_key.is_none() || !auth_valid {
                (
                    if base_url_valid {
                        cached_ids.unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    if base_url_valid {
                        ModelDiscoveryStatus::NotConfigured
                    } else {
                        ModelDiscoveryStatus::Unavailable
                    },
                    None,
                )
            } else if fresh {
                (
                    cached_ids.unwrap_or_default(),
                    ModelDiscoveryStatus::Fresh,
                    None,
                )
            } else {
                let discovery_config = OpenAiProviderConfig {
                    provider_name: provider_name.clone(),
                    model_name: "models".to_string(),
                    base_url: provider_file.base_url.clone(),
                    api_key: api_key.clone().unwrap_or_default(),
                    source: ProviderConfigSource::UserConfigFile,
                    max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                    max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                };
                match OpenAiProvider::new(discovery_config)
                    .and_then(|provider| provider.discover_model_ids())
                    .and_then(validate_discovered_model_ids)
                {
                    Ok(model_ids) => {
                        cache.providers.insert(
                            provider_name.clone(),
                            UserModelsCacheRecord {
                                endpoint_sha256: endpoint_hash,
                                fetched_at_unix_seconds: now,
                                model_ids: model_ids.clone(),
                            },
                        );
                        cache_changed = true;
                        (model_ids, ModelDiscoveryStatus::Fresh, None)
                    }
                    Err(error) => (
                        cached_ids_for_fallback.unwrap_or_default(),
                        if had_cached_ids {
                            ModelDiscoveryStatus::Stale
                        } else {
                            ModelDiscoveryStatus::Unavailable
                        },
                        Some(public_diagnostic(&error)),
                    ),
                }
            };
        if let Some(error) = discovery_error {
            diagnostics.push(error);
        }
        let discovered_set = discovered_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let explicit_set = explicit_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut ids = explicit_ids;
        ids.extend(discovered_ids);
        ids.sort();
        ids.dedup();
        provider_catalogs.push(UserProviderModelCatalog {
            provider_name: provider_name.clone(),
            base_url_present: !provider_file.base_url.is_empty(),
            api_key_present: api_key.is_some(),
            discovery,
            models: ids
                .into_iter()
                .map(|id| UserModelCatalogEntry {
                    discovered: discovered_set.contains(&id),
                    explicit: explicit_set.contains(&id),
                    selectable: selectable_ids.contains(&id),
                    max_context_tokens: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .and_then(|model| model.max_context_tokens),
                    reasoning_variants: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .map(|model| {
                            model
                                .reasoning_variants
                                .keys()
                                .filter(|variant| {
                                    validate_identifier(variant, "reasoning variant").is_ok()
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_variant: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .and_then(|model| model.default_variant.clone())
                        .filter(|variant| {
                            validate_identifier(variant, "reasoning variant").is_ok()
                        }),
                    id,
                })
                .collect(),
            error: (!diagnostics.is_empty()).then(|| diagnostics.join("; ")),
        });
    }
    if cache_changed {
        match serde_json::to_string_pretty(&cache) {
            Ok(cache_text) => {
                if write_json_file(&cache_path, &cache_text, false).is_err() {
                    cache_status = ModelCacheStatus::WriteFailed;
                }
            }
            Err(_) => cache_status = ModelCacheStatus::WriteFailed,
        }
    }
    Ok(UserModelCatalog {
        default_selector: user_config
            .config
            .default_model
            .as_deref()
            .and_then(|selector| {
                parse_model_selector(selector)
                    .ok()
                    .map(|_| selector.to_string())
            }),
        cache_status,
        providers: provider_catalogs,
    })
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
mod user_config_tests {
    use super::*;

    fn executable_user_model() -> UserConfigModel {
        UserConfigModel {
            api_protocol: Some("chat".to_string()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(4_096),
            ..UserConfigModel::default()
        }
    }

    fn user_provider() -> UserConfigProvider {
        UserConfigProvider {
            base_url: "https://example.invalid/v1".to_string(),
            models: BTreeMap::from([("gpt-test".to_string(), executable_user_model())]),
        }
    }

    fn user_config_with_two_providers(auth: UserAuthFile) -> UserConfigData {
        UserConfigData {
            directory: PathBuf::from("C:/singularity-test"),
            config: UserConfigFile {
                version: 1,
                default_provider: Some("primary".to_string()),
                default_model: Some("primary/gpt-test".to_string()),
                auth_generation: None,
                providers: BTreeMap::from([
                    ("primary".to_string(), user_provider()),
                    ("secondary".to_string(), user_provider()),
                ]),
            },
            auth,
        }
    }

    #[test]
    fn unselected_provider_without_auth_does_not_block_capture() {
        let auth = UserAuthFile {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserAuthProvider {
                    api_key: "sk-primary".to_string(),
                },
            )]),
        };
        let data = user_config_with_two_providers(auth);
        let (snapshot, redacted) = capture_user_model_selection(
            &data,
            Some(ProviderConfigSource::UserConfigFile),
            &OpenAiProvider::new,
        )
        .expect("selected provider is configured");
        assert!(redacted.api_key_present);
        assert!(snapshot.providers["secondary"].provider.is_none());
        let error = provider_for_selection(&snapshot, Some("secondary/gpt-test"))
            .expect_err("missing auth must fail when selected");
        assert_eq!(error.error.kind, ModelErrorKind::AuthError);
        assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
    }

    #[test]
    fn provider_config_snapshot_preserves_the_original_configuration_error() {
        let snapshot = ProviderConfigSnapshot::capture_with_provider_and_sources(
            |_| None,
            OpenAiProvider::new,
            || None,
        );

        assert_eq!(snapshot.source(), None);
        assert!(!snapshot.configuration().configured);
        let first = snapshot.provider().expect_err("missing provider config");
        let second = snapshot
            .provider()
            .expect_err("same missing provider config");
        assert_eq!(first, second);
        assert!(first.message.contains("SINGULARITY_MODEL"));
        assert_eq!(
            first.error.code.as_deref(),
            Some("provider_configuration_missing")
        );
        assert_eq!(
            first.error.stage,
            Some(ProviderErrorStage::ClientInitialization)
        );
    }

    #[test]
    fn selected_provider_without_auth_fails_closed_as_auth_error() {
        let data = user_config_with_two_providers(UserAuthFile::default());
        let result = capture_user_model_selection(
            &data,
            Some(ProviderConfigSource::UserConfigFile),
            &OpenAiProvider::new,
        );
        let error = match result {
            Ok(_) => panic!("default provider auth is required"),
            Err(error) => error,
        };
        assert_eq!(error.error.kind, ModelErrorKind::AuthError);
        assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
    }

    #[test]
    fn cache_read_failures_are_typed_and_non_blocking() {
        let directory = tempfile::tempdir().expect("cache directory");
        let invalid = directory.path().join("invalid.json");
        std::fs::write(&invalid, b"not-json").expect("write invalid cache");
        assert_eq!(
            load_models_cache(&invalid).status,
            ModelCacheStatus::Invalid
        );

        let missing = directory.path().join("missing.json");
        assert_eq!(
            load_models_cache(&missing).status,
            ModelCacheStatus::NotPresent
        );

        let read_failed = directory.path().join("cache-directory");
        std::fs::create_dir(&read_failed).expect("create cache directory");
        assert_eq!(
            load_models_cache(&read_failed).status,
            ModelCacheStatus::ReadFailed
        );
    }

    #[test]
    fn relative_home_is_rejected_before_path_use() {
        let error = normalize_absolute_path(Path::new("relative-home"))
            .expect_err("relative user home must fail closed");
        assert!(error.message.contains("absolute path"));
    }

    #[test]
    fn repository_boundary_uses_nearest_git_root_and_allows_ancestors() {
        let directory = tempfile::tempdir().expect("repository boundary directory");
        let workspace = directory.path().join("workspace");
        let repository = workspace.join("repository");
        let nested = repository.join("nested");
        std::fs::create_dir_all(&nested).expect("create repository tree");
        std::fs::write(repository.join(".git"), b"gitdir: test").expect("create worktree marker");

        let root = repository_boundary_root(&nested).expect("discover nearest repository root");
        assert_eq!(
            root,
            canonicalize_existing_prefix(&repository).expect("canonical repository root")
        );
        ensure_home_outside_root(&workspace, &root).expect("repository ancestors remain usable");
        let inside = repository.join("missing-home");
        let error = ensure_home_outside_root(&inside, &root)
            .expect_err("repository root descendants must be rejected");
        assert!(error.message.contains("current repository"));
    }

    #[cfg(windows)]
    #[test]
    fn repository_boundary_comparison_is_case_insensitive_with_missing_tail() {
        let directory = tempfile::tempdir().expect("repository boundary directory");
        let repository = directory.path().join("CaseSensitiveRepo");
        let nested = repository.join("nested");
        std::fs::create_dir_all(&nested).expect("create repository tree");
        std::fs::create_dir(repository.join(".git")).expect("create repository marker");
        let root = repository_boundary_root(&nested).expect("discover repository root");
        let case_variant =
            PathBuf::from(repository.to_string_lossy().to_ascii_lowercase()).join("missing-home");
        assert!(
            ensure_home_outside_root(&case_variant, &root).is_err(),
            "case variants of repository descendants must be rejected"
        );
    }

    #[test]
    fn metadata_errors_are_not_treated_as_missing_paths() {
        let directory = tempfile::tempdir().expect("metadata directory");
        let missing = directory.path().join("missing.json");
        assert!(!path_exists_or_missing(&missing, "metadata failed").expect("missing is allowed"));
        let invalid = Path::new("\0");
        let error = path_exists_or_missing(invalid, "metadata failed")
            .expect_err("metadata errors must fail closed");
        assert_eq!(error.message, "metadata failed");
    }

    #[test]
    fn import_selector_rejects_invalid_model_and_variant_identifiers() {
        assert!(parse_import_model_selector("default/model name#high", "default").is_err());
        assert!(parse_import_model_selector("default/model#high variant", "default").is_err());
        assert!(parse_import_model_selector("default/model#high/fast", "default").is_err());
    }

    #[test]
    fn import_selector_accepts_configured_provider_prefix_and_bare_slash_model_ids() {
        assert_eq!(
            parse_import_model_selector("default/models/gpt#high", "default")
                .expect("configured provider selector"),
            (
                "default/models/gpt#high".to_string(),
                "models/gpt".to_string()
            )
        );
        assert_eq!(
            parse_import_model_selector("models/gpt", "default")
                .expect("bare slash-containing model id"),
            ("default/models/gpt".to_string(), "models/gpt".to_string())
        );
    }

    #[test]
    fn import_selector_treats_mismatched_provider_prefix_as_a_bare_model_id() {
        assert_eq!(
            parse_import_model_selector("other/models/gpt", "default")
                .expect("slash-containing model id is not a selector for another provider"),
            (
                "default/other/models/gpt".to_string(),
                "other/models/gpt".to_string()
            )
        );
    }

    #[test]
    fn endpoint_validation_rejects_ambiguous_provider_urls() {
        assert!(
            validate_base_url(
                Some("https://provider.example/v1"),
                Some(ProviderConfigSource::UserConfigFile),
            )
            .is_ok()
        );
        for invalid in [
            "",
            "provider.example/v1",
            "ftp://provider.example/v1",
            "https://user:secret@provider.example/v1",
            "https://provider.example/v1?token=secret",
            "https://provider.example/v1#fragment",
        ] {
            assert!(
                validate_base_url(Some(invalid), Some(ProviderConfigSource::UserConfigFile))
                    .is_err(),
                "endpoint must be rejected: {invalid}"
            );
        }
    }

    #[test]
    fn selected_invalid_endpoint_precedes_missing_auth() {
        let mut data = user_config_with_two_providers(UserAuthFile::default());
        data.config
            .providers
            .get_mut("primary")
            .expect("default provider")
            .base_url = "not-an-absolute-url".to_string();
        let error = match capture_user_model_selection(
            &data,
            Some(ProviderConfigSource::UserConfigFile),
            &OpenAiProvider::new,
        ) {
            Ok(_) => panic!("invalid endpoint must fail before missing auth"),
            Err(error) => error,
        };
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_configuration_invalid")
        );
        assert!(error.message.contains("absolute URL"));
    }

    #[test]
    fn oversized_cache_is_invalid_while_io_failures_remain_read_failed() {
        let directory = tempfile::tempdir().expect("cache directory");
        let oversized = directory.path().join("oversized.json");
        std::fs::write(
            &oversized,
            vec![b'x'; crate::MAX_DISCOVERY_RESPONSE_BYTES + 1],
        )
        .expect("write oversized cache");
        assert_eq!(
            load_models_cache(&oversized).status,
            ModelCacheStatus::Invalid
        );

        let read_failed = directory.path().join("cache-directory");
        std::fs::create_dir(&read_failed).expect("create cache directory");
        assert_eq!(
            load_models_cache(&read_failed).status,
            ModelCacheStatus::ReadFailed
        );
    }

    #[test]
    fn oversized_user_config_and_private_auth_reads_are_rejected() {
        let directory = tempfile::tempdir().expect("user config directory");
        let oversized_contents = "x".repeat(crate::MAX_DISCOVERY_RESPONSE_BYTES + 1);
        let config_path = directory.path().join(USER_CONFIG_FILE_NAME);
        write_json_file(&config_path, &oversized_contents, true)
            .expect("write oversized user config");
        let config_error =
            match read_user_config_data_from_directory(directory.path().to_path_buf()) {
                Ok(_) => panic!("oversized user config must fail"),
                Err(error) => error,
            };
        assert_eq!(
            config_error.message,
            "user provider config exceeds the size limit"
        );
        assert_eq!(
            config_error.error.code.as_deref(),
            Some("provider_configuration_invalid")
        );
        assert_eq!(
            config_error.error.stage,
            Some(ProviderErrorStage::ClientInitialization)
        );
        assert!(
            !config_error
                .message
                .contains(&config_path.display().to_string())
        );

        let auth_path = write_new_auth_generation(
            directory.path(),
            "auth.v1-00000000000000000000000000000000.json",
            &oversized_contents,
        )
        .expect("write oversized private auth");
        let auth_error =
            read_private_auth_file(&auth_path).expect_err("oversized private auth must fail");
        assert_eq!(
            auth_error.message,
            "user provider auth exceeds the size limit"
        );
        assert_eq!(
            auth_error.error.code.as_deref(),
            Some("provider_configuration_invalid")
        );
        assert_eq!(
            auth_error.error.stage,
            Some(ProviderErrorStage::ClientInitialization)
        );
        assert!(
            !auth_error
                .message
                .contains(&auth_path.display().to_string())
        );
        assert!(!auth_error.message.contains(&oversized_contents));
    }

    #[test]
    fn config_writer_lock_is_exclusive_and_releases_cleanly() {
        let directory = tempfile::tempdir().expect("writer lock directory");
        let first = acquire_config_writer_lock(directory.path()).expect("first writer lock");
        let second = match acquire_config_writer_lock(directory.path()) {
            Ok(_) => panic!("second writer must observe the exclusive lock"),
            Err(error) => error,
        };
        assert!(second.message.contains("in progress"));
        drop(first);
        assert!(directory.path().join(".config.lock").exists());
        let third = acquire_config_writer_lock(directory.path()).expect("lock is released");
        drop(third);
        assert!(directory.path().join(".config.lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn config_writer_lock_rejects_preexisting_hardlink_without_mutating_target_acl() {
        let directory = tempfile::tempdir().expect("writer lock directory");
        let target = directory.path().join("target.txt");
        let lock_path = directory.path().join(".config.lock");
        std::fs::write(&target, b"target").expect("target file");
        let target_file = std::fs::File::open(&target).expect("open target file");
        assert!(
            windows_auth_acl::ensure_owner_only_handle(&target_file).is_err(),
            "the inherited target ACL must not already be owner-only"
        );
        std::fs::hard_link(&target, &lock_path).expect("create target hard link");

        let error = match acquire_config_writer_lock(directory.path()) {
            Ok(_) => panic!("a pre-existing lock hard link must fail closed"),
            Err(error) => error,
        };
        assert!(error.message.contains("writer lock"));
        assert_eq!(std::fs::read(&target).expect("read target file"), b"target");
        assert!(
            windows_auth_acl::ensure_owner_only_handle(&target_file).is_err(),
            "rejecting a pre-existing hard link must not change its target ACL"
        );
    }

    #[cfg(windows)]
    #[test]
    fn config_writer_lock_repairs_an_unfinished_single_link_lock() {
        let directory = tempfile::tempdir().expect("writer lock directory");
        let lock_path = directory.path().join(".config.lock");
        std::fs::write(&lock_path, b"").expect("reserve lock path");
        let unfinished = std::fs::File::open(&lock_path).expect("open unfinished lock");
        assert!(
            windows_auth_acl::ensure_owner_only_handle(&unfinished).is_err(),
            "unfinished lock must start with the inherited ACL"
        );
        drop(unfinished);

        let lock = acquire_config_writer_lock(directory.path())
            .expect("a single-link unfinished lock can be repaired after identity checks");
        windows_auth_acl::ensure_owner_only_handle(&lock._file)
            .expect("repaired lock must have an owner-only ACL");
        drop(lock);
        let repaired = std::fs::File::open(&lock_path).expect("open repaired lock");
        windows_auth_acl::ensure_owner_only_handle(&repaired)
            .expect("repaired lock must retain an owner-only ACL after release");
        assert_eq!(std::fs::read(&lock_path).expect("read repaired lock"), b"");
        assert!(lock_path.exists());
    }

    #[test]
    fn config_json_write_is_atomic() {
        let directory = tempfile::tempdir().expect("temporary user config directory");
        let path = directory.path().join(USER_CONFIG_FILE_NAME);
        write_json_file(&path, r#"{"providers":{}}"#, false).expect("write config file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read config file"),
            r#"{"providers":{}}"#
        );
        assert!(
            !directory
                .path()
                .read_dir()
                .expect("read temporary directory")
                .any(|entry| entry
                    .expect("read directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".config.json.tmp-"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_permissions_fail_closed_when_group_readable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary user config directory");
        let path = directory.path().join("auth.json");
        write_json_file(&path, r#"{"providers":{}}"#, true).expect("write auth file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("make auth file group-readable");
        assert!(ensure_private_secret_file(&path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn inherited_auth_permissions_fail_closed() {
        let directory = tempfile::tempdir().expect("temporary user config directory");
        let path = directory.path().join("auth.json");
        std::fs::write(&path, r#"{"providers":{}}"#).expect("write auth file");
        assert!(ensure_private_secret_file(&path).is_err());
    }
}
