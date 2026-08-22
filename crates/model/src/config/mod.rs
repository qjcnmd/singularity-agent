//! provider 配置分层解析、脱敏状态和服务级配置快照。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) mod filesystem;
pub(crate) mod runtime;
pub(crate) mod schema;
pub(crate) mod selection;
pub(crate) mod user;

pub(crate) use filesystem::read_bounded_text;
pub use runtime::ProviderConfigSnapshot;
pub(crate) use runtime::*;
pub(crate) use schema::*;
pub use schema::{
    ModelBlockerKind, ModelProviderConfig, ProviderConfigResolution, ProviderConfigSource,
    ProviderConfigurationStatus,
};
pub(crate) use user::*;
pub use user::{
    ModelCacheStatus, ModelDiscoveryStatus, UserConfigImportResult, UserModelCatalog,
    UserModelCatalogEntry, UserProviderModelCatalog, import_env_to_user_config,
    read_user_model_catalog,
};

use super::{
    DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_BASE_URL, ENV_CONTEXT_TOKENS, ENV_MAX_OUTPUT_TOKENS,
    ENV_MODEL, ENV_PROVIDER, MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS,
    ModelError, ModelErrorKind, OpenAiProvider, OpenAiProviderConfig, PROVIDER_SNAPSHOT_ID_PREFIX,
    ProviderApiProtocol, ProviderError, ProviderErrorStage, ProviderToolReasoningMode,
    ThinkingWireFormat, validate_provider_config,
};

#[cfg(test)]
use crate::error::ModelErrorCategory;
#[cfg(test)]
use filesystem::write_json_file;
#[cfg(test)]
pub(crate) use runtime::capture_models_file;
pub(super) use selection::model_selector_error;
pub use selection::{ModelSelectorParts, split_model_selector};
use selection::{parse_model_selector, provider_for_selection};

pub fn resolve_provider_config<F>(get_env: F) -> ProviderConfigResolution
where
    F: FnMut(&str) -> Option<String>,
{
    let values = resolve_provider_values(get_env);
    provider_config_resolution(&values)
}

pub(crate) fn missing_provider_config_error(
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

pub(crate) fn missing_provider_auth_error(source: Option<ProviderConfigSource>) -> ProviderError {
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

pub(crate) fn provider_source_missing_error() -> ProviderError {
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

pub(crate) fn parse_provider_limit(
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

pub(crate) fn validate_provider_value(
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

pub(crate) fn validate_base_url(
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

pub(crate) fn normalized_endpoint_identity(base_url: &str) -> Result<String, ProviderError> {
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
pub(crate) struct ProviderConfigLayer {
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
pub(crate) struct ResolvedProviderValues {
    pub(crate) source: Option<ProviderConfigSource>,
    pub(crate) provider_name: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) context_tokens: Option<String>,
    pub(crate) max_output_tokens: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) models_config_path: Option<String>,
    pub(crate) user_config: Option<UserConfigData>,
    pub(crate) user_config_error: Option<ProviderError>,
}

fn configured_model_from_user_file(
    provider_name: &str,
    model_name: &str,
    model_file: &UserConfigModel,
    directory_limits: Option<ModelTokenLimits>,
) -> Result<ConfiguredModel, ProviderError> {
    // 顶层字段为唯一权威；内置表与 models.dev 目录元数据依次兜底缺省的
    // max_context_tokens/max_output_tokens，任一级命中即停。
    // api_protocol 必须由用户显式声明。
    let builtin = super::builtin_models::builtin_model(provider_name, model_name);
    let (Some(api_protocol), Some(max_output_tokens)) = (
        model_file.api_protocol.as_deref(),
        model_file
            .max_output_tokens
            .or_else(|| builtin.map(|entry| entry.max_output_tokens))
            .or_else(|| directory_limits.map(|limits| limits.output)),
    ) else {
        return Err(configuration_error(
            "model override is incomplete; api_protocol and max_output_tokens are required",
            "provider_configuration_invalid",
        ));
    };
    let protocol = parse_catalog_protocol(api_protocol)?;
    let max_context_tokens = model_file
        .max_context_tokens
        .or_else(|| builtin.map(|entry| entry.context_window))
        .or_else(|| directory_limits.map(|limits| limits.context));
    let supports_developer_role = model_file.supports_developer_role.unwrap_or(true);
    let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
    let reasoning_variants = model_file
        .reasoning_variants
        .iter()
        .map(|(variant, descriptor)| {
            (
                variant.clone(),
                ReasoningVariant {
                    enabled: descriptor.enabled,
                    wire_effort: descriptor.wire_effort.clone(),
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
        default_variant: model_file.default_variant.clone(),
        thinking_wire_format,
        tool_reasoning_mode,
        supports_developer_role,
        supports_tool_choice,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls,
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
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
    // 读路径只读本地元数据缓存，永不联网；缓存缺失或过期时目录为空，
    // 能力解析保持与无第三级来源时相同的 fail closed 行为。
    let metadata_directory = load_user_metadata_directory(&user_config.directory);
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
        let endpoint_host = http_endpoint_host(&provider_file.base_url);
        for (model_name, model_file) in &provider_file.models {
            if let Err(error) = validate_model_id(model_name, "model id") {
                if provider_name.as_str() == default_provider_name
                    && model_name == parsed_default.model_name
                {
                    return Err(error);
                }
                continue;
            }
            let directory_limits =
                metadata_directory.limits_for(provider_name, model_name, endpoint_host.as_deref());
            match configured_model_from_user_file(
                provider_name,
                model_name,
                model_file,
                directory_limits,
            ) {
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

pub(crate) fn resolve_provider_values<F>(mut get_env: F) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
{
    resolve_provider_values_with_user_config(&mut get_env, user_config_layer)
}

pub(crate) fn resolve_provider_values_with_user_config<F, U>(
    mut get_env: F,
    user_config: U,
) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
    U: FnOnce() -> Option<ProviderConfigLayer>,
{
    // Provenance is selected atomically: any process-environment provider
    // field makes that complete layer authoritative. Never merge missing
    // process fields from user config, because doing so would make a snapshot
    // depend on two mutable sources and hide an incomplete process setup.
    let process_layer = ProviderConfigLayer::from_process_env(&mut get_env);
    if process_layer.any_present() {
        return process_layer.into_values(ProviderConfigSource::ProcessEnvironment);
    }
    user_config()
        .map(|layer| layer.into_values(ProviderConfigSource::UserConfigFile))
        .unwrap_or_default()
}

pub(crate) fn normalized_provider_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

pub(crate) fn find_import_env_file(project_dir: &Path) -> Option<PathBuf> {
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

pub(crate) fn read_import_env_layer(path: &Path) -> ProviderConfigLayer {
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

pub(crate) fn parse_env_line(line: &str) -> Option<(String, String)> {
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

pub(crate) fn redacted_presence(present: bool) -> String {
    if present {
        "present(redacted)".to_string()
    } else {
        "missing".to_string()
    }
}

#[cfg(test)]
#[path = "../config_tests.rs"]
mod user_config_tests;
