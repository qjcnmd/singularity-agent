//! 模型配置的读写、校验、模型发现与模型选择解析。

mod discovery;
pub(crate) mod manager;
pub(crate) mod schema;
pub(crate) mod selection;
pub(crate) mod user;

pub use discovery::discover as discover_models;
pub use manager::{ModelConfigManager, ModelConfigurationSnapshot};
pub(crate) use schema::*;
pub(crate) use user::*;

use super::{ModelErrorKind, ProviderApiProtocol, ProviderError};
use crate::openai::wire::ThinkingWireFormat;

use selection::{OpenAiProviderConfig, SelectedModel, resolve_model_definition, resolve_model_selection};
use singularity_protocol::ModelSelection;

pub(crate) fn configuration_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ModelErrorKind::InvalidRequest, message)
}

pub(crate) fn missing_provider_config_error(name: &str) -> ProviderError {
    configuration_error(format!("缺少提供方配置：{name}"))
}

pub(crate) fn validate_provider_value(value: &str, name: &str) -> Result<(), ProviderError> {
    let invalid_boundary_whitespace = value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace);
    if value.chars().any(|character| matches!(character, '\r' | '\n' | '\0')) || invalid_boundary_whitespace {
        return Err(configuration_error(format!("模型配置无效：{name} 不能包含换行、空字符或首尾空白")));
    }
    Ok(())
}

pub(crate) fn validate_base_url(value: &str) -> Result<(), ProviderError> {
    validate_provider_value(value, "base_url")?;
    if value.is_empty() {
        return Err(configuration_error("模型配置无效：base_url 不能为空"));
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| configuration_error("模型配置无效：base_url 须填写完整的网址"))?;
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
