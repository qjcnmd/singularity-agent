use super::{
    ModelSelection, ProviderApiProtocol, ProviderError, UserConfigFile, UserConfigModel, configuration_error,
    validate_api_key, validate_base_url, validate_identifier, validate_reasoning_variants,
};

pub(crate) struct ProviderConfig {
    pub(crate) provider_name: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
}

pub(super) fn resolve_model_selection(
    data: &UserConfigFile,
    selected: &ModelSelection,
) -> Result<(ProviderConfig, SelectedModel), ProviderError> {
    let provider = data
        .providers
        .get(&selected.provider)
        .ok_or_else(|| configuration_error(format!("模型选择器指定的提供方不存在：{}", selected.provider)))?;
    let model = provider.models.get(&selected.model).ok_or_else(|| {
        configuration_error(format!(
            "模型选择器指定的模型未在提供方 {} 配置中声明：{}",
            selected.provider, selected.model
        ))
    })?;
    validate_identifier(&selected.provider, "提供方标识")?;
    validate_identifier(&selected.model, "模型标识")?;
    validate_base_url(&provider.base_url)?;
    let key = provider.api_key.as_str();
    validate_api_key(key)?;
    let model = resolve_model_definition(
        model,
        provider.api_protocol,
        &selected.model,
        selected.reasoning.as_deref(),
    )?;
    Ok((
        ProviderConfig {
            provider_name: selected.provider.clone(),
            base_url: provider.base_url.clone(),
            api_key: key.to_string(),
        },
        model,
    ))
}

pub(super) fn resolve_model_definition(
    model_file: &UserConfigModel,
    protocol: ProviderApiProtocol,
    model_name: &str,
    requested_variant: Option<&str>,
) -> Result<SelectedModel, ProviderError> {
    let max_context_tokens = model_file.max_context_tokens;
    let max_output_tokens = model_file.max_output_tokens;
    let reasoning_variants = model_file.reasoning_variants.as_deref().unwrap_or_default();
    validate_reasoning_variants(protocol, reasoning_variants)?;
    if max_context_tokens == 0 || max_output_tokens == 0 {
        return Err(configuration_error(format!("模型 {model_name} 的上下文窗口和最大输出 Token 须大于 0")));
    }
    if let Some(value) = requested_variant
        && !reasoning_variants.iter().any(|variant| variant == value)
    {
        return Err(configuration_error(format!(
            "模型选择器指定的推理档位未在模型 {model_name} 配置中声明：{value}"
        )));
    }
    Ok(SelectedModel {
        model_name: model_name.to_string(),
        protocol,
        max_context_tokens,
        max_output_tokens,
        reasoning_variant: requested_variant.map(str::to_string),
    })
}
