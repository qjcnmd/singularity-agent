//! 从 `config.json` 解析选中的提供方连接参数、模型容量与推理档位。

use super::*;

/// 解析好的提供方连接设置。
pub(crate) struct OpenAiProviderConfig {
    pub(crate) provider_name: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

/// 一次从配置解析完成的模型选择。
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) chat_output_tokens_field: &'static str,
}

pub(super) fn resolve_model_selection(
    data: &UserConfigFile,
    selected: &ModelSelection,
) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
    let provider = data
        .providers
        .get(&selected.provider)
        .ok_or_else(|| configuration_error("模型选择器指定的提供方不存在"))?;
    let model = provider
        .models
        .get(&selected.model)
        .ok_or_else(|| configuration_error("模型选择器指定的模型未在提供方配置中声明"))?;
    validate_base_url(&provider.base_url)?;
    let key = provider.api_key.as_str();
    validate_provider_value(key, "api_key")?;
    let model = resolve_model_definition(
        model,
        provider.api_protocol,
        &selected.model,
        selected.reasoning.as_deref(),
    )?;
    Ok((
        OpenAiProviderConfig {
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
    let thinking_wire_format =
        parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
    validate_reasoning_variants(protocol, thinking_wire_format, reasoning_variants)?;
    let chat_output_tokens_field =
        parse_chat_output_tokens_field(model_file.chat_output_tokens_field.as_deref(), protocol)?;
    if max_context_tokens == 0 || max_output_tokens == 0 {
        return Err(configuration_error("模型上下文窗口和最大输出 Token 须大于 0"));
    }
    if requested_variant.is_some_and(|value| !reasoning_variants.iter().any(|variant| variant == value)) {
        return Err(configuration_error("模型选择器指定的推理档位未在模型配置中声明"));
    }
    let reasoning_variant = requested_variant.map(str::to_string);
    Ok(SelectedModel {
        model_name: model_name.to_string(),
        api_protocol: protocol,
        max_context_tokens,
        max_output_tokens,
        thinking_wire_format,
        chat_output_tokens_field,
        reasoning_variant,
    })
}
