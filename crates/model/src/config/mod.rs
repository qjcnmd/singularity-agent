//! provider 配置解析与服务级模型选择快照。

mod discovery;
pub(crate) mod runtime;
pub(crate) mod schema;
pub(crate) mod selection;
pub(crate) mod user;

pub use discovery::discover as discover_models;
pub use runtime::{ModelConfigOwner, ModelConfigurationSnapshot, ProviderConfigSnapshot};
pub(crate) use schema::*;
pub(crate) use user::*;

use super::{
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS, ModelErrorKind, OpenAiProvider,
    ProviderApiProtocol, ProviderError, ThinkingWireFormat,
};
use crate::provider::runtime::OpenAiProviderConfig;

pub use selection::{ModelSelectorParts, compose_model_selector, split_model_selector};
use selection::{parse_model_selector, resolve_model_definition, resolve_model_selection};

pub(crate) fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
    ProviderError::new(ModelErrorKind::InvalidRequest, message).with_code(code)
}

pub(crate) fn missing_provider_config_error(name: &str) -> ProviderError {
    configuration_error(
        format!("required provider configuration is missing: {name}"),
        "provider_configuration_missing",
    )
}

pub(crate) fn missing_provider_auth_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::AuthError,
        "required provider authentication is missing".to_string(),
    )
    .with_code("provider_auth_missing")
}

pub(crate) fn validate_provider_value(value: &str, name: &str) -> Result<(), ProviderError> {
    let invalid_boundary_whitespace = value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace);
    if value
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
        || invalid_boundary_whitespace
    {
        return Err(configuration_error(
            format!(
                "invalid model configuration: {name} contains forbidden control characters or boundary whitespace"
            ),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_base_url(value: &str) -> Result<(), ProviderError> {
    validate_provider_value(value, "base_url")?;
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
