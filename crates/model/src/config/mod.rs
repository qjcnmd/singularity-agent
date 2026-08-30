//! provider 配置解析、脱敏状态和服务级配置快照。
use std::collections::BTreeMap;

pub(crate) mod filesystem;
pub(crate) mod runtime;
pub(crate) mod schema;
pub(crate) mod selection;
pub(crate) mod user;

pub use runtime::ProviderConfigSnapshot;
pub(crate) use runtime::*;
pub(crate) use schema::ProviderConfigurationStatus;
pub(crate) use schema::*;
pub use schema::{ModelBlockerKind, ModelProviderConfig};
pub(crate) use user::*;

use super::{
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS, ModelError, ModelErrorKind,
    OpenAiProvider, OpenAiProviderConfig, ProviderApiProtocol, ProviderError, ProviderErrorStage,
    ProviderToolReasoningMode, ThinkingWireFormat, validate_provider_config,
};

pub(super) use selection::model_selector_error;
pub use selection::{ModelSelectorParts, compose_model_selector, split_model_selector};
use selection::{parse_model_selector, provider_for_selection};

pub(crate) fn missing_provider_config_error(name: &str) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            format!("required provider configuration is missing: {name}"),
        )
        .with_provider_diagnostic(
            "provider_configuration_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

pub(crate) fn missing_provider_auth_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::AuthError,
            "required provider authentication is missing".to_string(),
        )
        .with_provider_diagnostic(
            "provider_auth_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

pub(crate) fn validate_provider_value(
    value: Option<&str>,
    name: &str,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    let invalid_boundary_whitespace = value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace);
    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
        || invalid_boundary_whitespace
    {
        return Err(ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::InvalidRequest,
                format!(
                    "invalid model configuration: {name} contains forbidden control characters or boundary whitespace"
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

pub(crate) fn validate_base_url(value: Option<&str>) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    validate_provider_value(Some(value), "base_url")?;
    if value.is_empty() {
        return Err(configuration_error(
            "invalid model configuration: base_url must not be empty",
            "provider_configuration_invalid",
        ));
    }
    let url = reqwest::Url::parse(value).map_err(|_| {
        configuration_error(
            "invalid model configuration: base_url must be an absolute URL",
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
            "invalid model configuration: base_url must be an http/https URL with a host, path only, and no credentials, query, or fragment",
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

fn configured_model_from_user_file(
    model_file: &UserConfigModel,
    provider_name: &str,
    model_name: &str,
) -> Result<ConfiguredModel, ProviderError> {
    // api_protocol 必须由用户显式声明。
    let Some(api_protocol) = model_file.api_protocol.as_deref() else {
        return Err(configuration_error(
            "user config model must declare api_protocol (chat or responses)",
            "provider_configuration_invalid",
        ));
    };
    let protocol = parse_catalog_protocol(api_protocol)?;
    let max_context_tokens = model_file.max_context_tokens.unwrap_or_else(|| {
        let (ctx, _) = crate::catalog::resolve_model_limits(provider_name, model_name);
        ctx
    });
    let max_output_tokens = model_file.max_output_tokens.unwrap_or_else(|| {
        let (_, out) = crate::catalog::resolve_model_limits(provider_name, model_name);
        out
    });
    let supports_developer_role = model_file.supports_developer_role.unwrap_or(true);
    let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
    let reasoning_variants = model_file.reasoning_variants.clone();
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
        Some(max_context_tokens),
        "max_context_tokens",
        MAX_CONFIGURED_CONTEXT_TOKENS,
    )?;
    validate_catalog_limit(
        Some(max_output_tokens),
        "max_output_tokens",
        MAX_CONFIGURED_OUTPUT_TOKENS,
    )?;
    if max_output_tokens >= max_context_tokens {
        return Err(configuration_error(
            "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
            "provider_configuration_invalid",
        ));
    }
    Ok(ConfiguredModel {
        protocol,
        max_context_tokens: Some(max_context_tokens),
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

fn capture_user_model_selection(
    user_config: &UserConfigData,
    runtime_handle: &tokio::runtime::Handle,
) -> Result<(ModelSelectionSnapshot, ModelProviderConfig), ProviderError> {
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

    // 阶段 1：把全部 provider 条目规范化为类型化配置。阻断启动的错误只可能
    // 发生在默认提供者上；非默认条目的同类错误以 provider_error 记录或整体
    // 跳过，属于显式的降级策略，不混入默认项解析。
    let mut providers = BTreeMap::new();
    for (provider_name, provider_file) in &user_config.config.providers {
        let Some(configured) = normalize_provider_entry(
            provider_name,
            provider_file,
            &user_config.auth,
            provider_name == &default_provider_name,
            parsed_default.model_name,
            runtime_handle,
        )?
        else {
            continue;
        };
        providers.insert(provider_name.clone(), configured);
    }
    if providers.is_empty() {
        return Err(configuration_error(
            "user provider config has no model with explicit protocol and output token limit",
            "provider_configuration_invalid",
        ));
    }
    // 阶段 2：单点解析默认 selector，构造最终选择。
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
            .unwrap_or_else(missing_provider_auth_error));
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

/// 单个 provider 条目的规范化：校验 id/endpoint/key 与模型表，构造类型化
/// provider 配置（含按需的 adapter 实例）。阻断错误只作用于默认提供者
/// （`is_default`）；非默认条目的同类错误以 `provider_error` 记录、无效模型
/// 跳过、无有效模型时整体跳过（返回 `None`），不阻断启动。
fn normalize_provider_entry(
    provider_name: &str,
    provider_file: &UserConfigProvider,
    auth: &UserAuthFile,
    is_default: bool,
    default_model_name: &str,
    runtime_handle: &tokio::runtime::Handle,
) -> Result<Option<ConfiguredProvider>, ProviderError> {
    let is_default_model = |model_name: &str| is_default && model_name == default_model_name;

    if let Err(error) = validate_provider_identifier(provider_name, "provider id") {
        if is_default {
            return Err(error);
        }
        return Ok(None);
    }
    let endpoint_error = validate_base_url(Some(&provider_file.base_url)).err();
    if is_default && let Some(error) = endpoint_error.clone() {
        return Err(error);
    }
    let api_key = auth
        .providers
        .get(provider_name)
        .map(|provider| provider.api_key.clone())
        .filter(|value| !value.is_empty());
    if is_default && api_key.is_none() {
        return Err(missing_provider_auth_error());
    }
    let auth_error = api_key
        .as_deref()
        .and_then(|api_key| validate_provider_value(Some(api_key), "api_key").err());
    if is_default && let Some(error) = auth_error.clone() {
        return Err(error);
    }
    let mut models = BTreeMap::new();
    for (model_name, model_file) in &provider_file.models {
        if let Err(error) = validate_model_id(model_name, "model id") {
            if is_default_model(model_name) {
                return Err(error);
            }
            continue;
        }
        match configured_model_from_user_file(model_file, provider_name, model_name) {
            Ok(model) => {
                models.insert(model_name.clone(), model);
            }
            Err(error) if is_default_model(model_name) => return Err(error),
            Err(_) => continue,
        }
    }
    if models.is_empty() {
        return Ok(None);
    }
    let (provider, provider_error) = match (api_key, endpoint_error, auth_error) {
        (None, _, _) => (None, Some(missing_provider_auth_error())),
        (_, Some(error), _) => (None, Some(error)),
        (_, _, Some(error)) => (None, Some(error)),
        (Some(api_key), None, None) => {
            // 不变量：上方已对 models.is_empty() 早退，keys().next() 与 get() 必成功。
            #[allow(clippy::expect_used)]
            let base_model = models
                .keys()
                .next()
                .cloned()
                .expect("models checked non-empty");
            #[allow(clippy::expect_used)]
            let base_model_config = models.get(&base_model).expect("base model exists");
            let config = OpenAiProviderConfig {
                provider_name: provider_name.to_string(),
                model_name: base_model,
                base_url: provider_file.base_url.clone(),
                api_key,
                max_context_tokens: base_model_config.max_context_tokens,
                max_output_tokens: base_model_config.max_output_tokens,
            };
            match OpenAiProvider::new(config, runtime_handle.clone()) {
                Ok(provider) => (Some(provider), None),
                Err(error) if is_default => return Err(error),
                Err(error) => (None, Some(error)),
            }
        }
    };
    Ok(Some(ConfiguredProvider {
        provider,
        provider_error,
        models,
    }))
}

pub(crate) fn redacted_presence(present: bool) -> String {
    if present {
        "present(redacted)".to_string()
    } else {
        "missing".to_string()
    }
}
