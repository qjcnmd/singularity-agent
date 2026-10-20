//! 直接从冻结的配置里解析选中的提供方、模型能力和推理档位。

use super::*;

/// 解析好的 OpenAI 兼容连接设置；敏感信息只为传输而保留。
pub(crate) struct OpenAiProviderConfig {
    pub(crate) provider_name: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

impl std::fmt::Debug for OpenAiProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiProviderConfig")
            .field("provider_name", &self.provider_name)
            .field("base_url", &"[redacted]")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

/// 一次从配置解析完成的模型选择，最终档位和对应的线上档位放在一起。
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) chat_output_tokens_field: &'static str,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
}

/// 已校验的模型选择器，持久化和模型解析共用。
pub struct ParsedModelSelector<'a> {
    pub provider_name: &'a str,
    pub model_name: &'a str,
    pub reasoning_variant: Option<&'a str>,
}

/// 拼出 provider/model[#variant] 选择器，variant 为空就省略。
pub fn compose_model_selector(provider: &str, model: &str, variant: Option<&str>) -> String {
    let mut selector = format!("{provider}/{model}");
    if let Some(variant) = variant.filter(|value| !value.is_empty()) {
        selector.push('#');
        selector.push_str(variant);
    }
    selector
}

/// 严格解析 provider/model[#variant]；拒绝缺段和非法标识。
pub fn parse_model_selector(selector: &str) -> Result<ParsedModelSelector<'_>, ProviderError> {
    let (without_variant, reasoning_variant) = selector
        .rsplit_once('#')
        .map_or((selector, None), |(model, variant)| (model, Some(variant)));
    let (provider_name, model_name) = without_variant.split_once('/').ok_or_else(|| {
        configuration_error(
            "模型选择器须使用 provider_id/model_id[#variant] 格式",
            "provider_selector_invalid",
        )
    })?;
    super::validate_identifier(provider_name, "provider id")
        .map_err(|_| configuration_error("模型选择器中的提供方标识无效", "provider_selector_invalid"))?;
    super::validate_model_id(model_name, "model id")
        .map_err(|_| configuration_error("模型选择器中的模型标识无效", "provider_selector_invalid"))?;
    if let Some(reasoning_variant) = reasoning_variant {
        super::validate_identifier(reasoning_variant, "reasoning variant")
            .map_err(|_| configuration_error("模型选择器中的推理档位无效", "provider_selector_invalid"))?;
    }
    Ok(ParsedModelSelector {
        provider_name,
        model_name,
        reasoning_variant,
    })
}

pub(super) fn resolve_model_selection(
    data: &UserConfigFile,
    selector: Option<&str>,
) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
    let selected = selector.or(data.default_model.as_deref()).ok_or_else(|| {
        configuration_error(
            "未选择模型，请指定 provider/model[#variant] 或在配置中设置 default_model。",
            "provider_selector_invalid",
        )
    })?;
    let parsed = parse_model_selector(selected)?;
    let provider = data.providers.get(parsed.provider_name).ok_or_else(|| {
        configuration_error("模型选择器指定的提供方不存在", "provider_selector_unknown_provider")
    })?;
    let model = provider.models.get(parsed.model_name).ok_or_else(|| {
        configuration_error("模型选择器指定的模型未在提供方配置中声明", "provider_selector_unknown_model")
    })?;
    validate_base_url(&provider.base_url)?;
    let key = (!provider.api_key.is_empty())
        .then_some(provider.api_key.as_str())
        .ok_or_else(missing_provider_auth_error)?;
    validate_provider_value(key, "api_key")?;
    let model =
        resolve_model_definition(model, provider.api_protocol, parsed.model_name, parsed.reasoning_variant)?;
    Ok((
        OpenAiProviderConfig {
            provider_name: parsed.provider_name.to_string(),
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
    // 目录补全或用户填写的容量随配置保存；执行不猜测缺失值。
    let (Some(max_context_tokens), Some(max_output_tokens)) =
        (model_file.max_context_tokens, model_file.max_output_tokens)
    else {
        return Err(super::user_config_error(format!(
            "模型 {model_name} 缺少上下文窗口或最大输出 Token，请获取模型能力或手动填写后保存。"
        )));
    };
    let undeclared_variants = std::collections::BTreeMap::new();
    let reasoning_variants = model_file.reasoning_variants.as_ref().unwrap_or(&undeclared_variants);
    validate_reasoning_variants(protocol, reasoning_variants, model_file.default_variant.as_deref())?;
    let thinking_wire_format =
        parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
    let chat_output_tokens_field =
        parse_chat_output_tokens_field(model_file.chat_output_tokens_field.as_deref(), protocol)?;
    if model_file.requires_reasoning_content_for_tool_calls.is_some() && protocol != ProviderApiProtocol::Chat
    {
        return Err(configuration_error(
            "requires_reasoning_content_for_tool_calls 仅适用于 Chat",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    if max_context_tokens == 0 || max_output_tokens == 0 {
        return Err(configuration_error(
            "模型上下文窗口和最大输出 Token 须大于 0",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    if max_output_tokens >= max_context_tokens {
        return Err(configuration_error(
            "模型配置无效：max_output_tokens 须小于 max_context_tokens",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    let requested_variant = requested_variant.or(model_file.default_variant.as_deref());
    let (reasoning_variant, wire_reasoning_effort) = match requested_variant {
        None => (None, None),
        Some(requested_variant) => {
            let variant = reasoning_variants.get(requested_variant).ok_or_else(|| {
                configuration_error(
                    "模型选择器指定的推理档位未在模型配置中声明",
                    "provider_selector_unknown_reasoning_variant",
                )
            })?;
            (Some(requested_variant.to_string()), variant.wire_effort.clone())
        }
    };
    Ok(SelectedModel {
        model_name: model_name.to_string(),
        api_protocol: protocol,
        max_context_tokens,
        max_output_tokens,
        wire_reasoning_effort,
        thinking_wire_format,
        chat_output_tokens_field,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls
            .unwrap_or(false)
            && reasoning_variant.as_deref() != Some("off"),
        reasoning_variant,
    })
}
