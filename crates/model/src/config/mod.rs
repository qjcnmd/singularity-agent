mod discovery;
pub(crate) mod manager;
pub(crate) mod selection;
pub(crate) mod user;

pub use discovery::discover as discover_models;
pub use manager::{ModelConfigManager, ModelConfigurationSnapshot};
pub(crate) use user::*;

use super::{ModelErrorKind, ProviderError};

use protocol::{ModelSelection, ProviderApiProtocol};
use selection::{ProviderConfig, SelectedModel, resolve_model_definition, resolve_model_selection};

pub(crate) fn configuration_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ModelErrorKind::Other, message)
}

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty() || value.chars().any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(configuration_error(format!("模型配置无效：{label} 不能为空，也不能包含空白或控制字符")));
    }
    Ok(())
}

pub(crate) fn validate_reasoning_variants(
    protocol: ProviderApiProtocol,
    variants: &[String],
) -> Result<(), ProviderError> {
    for (index, variant) in variants.iter().enumerate() {
        validate_identifier(variant, "推理档位")?;
        if variants[..index].contains(variant) {
            return Err(configuration_error(format!("推理档位不能重复：{variant}")));
        }
        if protocol == ProviderApiProtocol::Responses && variant == "on" {
            return Err(configuration_error("Responses 须选择具体思考档位，不支持 on 开关"));
        }
    }
    Ok(())
}

pub(crate) fn validate_provider_value(value: &str, name: &str) -> Result<(), ProviderError> {
    let invalid_boundary_whitespace = value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace);
    if value.chars().any(|character| matches!(character, '\r' | '\n' | '\0')) || invalid_boundary_whitespace {
        return Err(configuration_error(format!("模型配置无效：{name} 不能包含换行、空字符或首尾空白")));
    }
    Ok(())
}

pub(crate) fn validate_api_key(value: &str) -> Result<(), ProviderError> {
    validate_provider_value(value, "api_key")?;
    if reqwest::header::HeaderValue::from_str(value).is_err() {
        return Err(configuration_error("模型配置无效：api_key 只能包含可见 ASCII 字符"));
    }
    Ok(())
}

pub(crate) fn validate_base_url(value: &str) -> Result<(), ProviderError> {
    validate_provider_value(value, "base_url")?;
    if value.is_empty() {
        return Err(configuration_error("模型配置无效：base_url 不能为空"));
    }
    let url = reqwest::Url::parse(value).map_err(|error| {
        configuration_error(format!("模型配置无效：base_url 须填写完整的网址（{error}）"))
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(configuration_error(
            "模型配置无效：base_url 须使用 http/https 并包含主机，只能附带路径，不能包含用户名、密码、查询参数或片段",
        ));
    }
    Ok(())
}
