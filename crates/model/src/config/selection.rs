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

/// 一次从配置解析完成的模型选择。把最终档位和对应的线上档位放在
/// 一起，免得再有第二张运行时映射表悄悄改掉发给提供方的请求。
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) chat_output_tokens_field: &'static str,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
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
            "model selector must use provider_id/model_id[#variant]",
            "provider_selector_invalid",
        )
    })?;
    super::validate_identifier(provider_name, "provider id").map_err(|_| {
        configuration_error(
            "model selector must contain a valid provider id",
            "provider_selector_invalid",
        )
    })?;
    super::validate_model_id(model_name, "model id").map_err(|_| {
        configuration_error(
            "model selector must contain a valid model id",
            "provider_selector_invalid",
        )
    })?;
    if let Some(reasoning_variant) = reasoning_variant {
        super::validate_identifier(reasoning_variant, "reasoning variant").map_err(|_| {
            configuration_error(
                "model selector must contain a valid reasoning variant",
                "provider_selector_invalid",
            )
        })?;
    }
    Ok(ParsedModelSelector {
        provider_name,
        model_name,
        reasoning_variant,
    })
}

pub(super) fn resolve_model_selection(
    data: &UserConfigData,
    selector: Option<&str>,
) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
    let selected = selector
        .or(data.config.default_model.as_deref())
        .ok_or_else(|| {
            configuration_error(
                "未选择模型，请指定 provider/model[#variant] 或在配置中设置 default_model。",
                "provider_selector_invalid",
            )
        })?;
    let parsed = parse_model_selector(selected)?;
    let provider = data
        .config
        .providers
        .get(parsed.provider_name)
        .ok_or_else(|| {
            configuration_error(
                "model selector references an unknown provider",
                "provider_selector_unknown_provider",
            )
        })?;
    let model = provider.models.get(parsed.model_name).ok_or_else(|| {
        configuration_error(
            "model selector references an unknown or disallowed model",
            "provider_selector_unknown_model",
        )
    })?;
    validate_base_url(&provider.base_url)?;
    let key = data
        .auth
        .providers
        .get(parsed.provider_name)
        .map(|credential| credential.api_key.as_str())
        .filter(|key| !key.is_empty())
        .ok_or_else(missing_provider_auth_error)?;
    validate_provider_value(key, "api_key")?;
    let model = resolve_model_definition(
        model,
        provider
            .api_protocol
            .as_deref()
            .or(model.api_protocol.as_deref()),
        parsed.model_name,
        parsed.reasoning_variant,
    )?;
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
    api_protocol: Option<&str>,
    model_name: &str,
    requested_variant: Option<&str>,
) -> Result<SelectedModel, ProviderError> {
    // api_protocol 只能由用户显式声明。
    let Some(api_protocol) = api_protocol else {
        return Err(configuration_error(
            "user config model must declare api_protocol (chat or responses)",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    };
    let protocol = parse_catalog_protocol(api_protocol)?;
    // 目录补全或用户填写的容量随配置保存；执行不猜测缺失值。
    let (Some(max_context_tokens), Some(max_output_tokens)) =
        (model_file.max_context_tokens, model_file.max_output_tokens)
    else {
        return Err(super::user_config_error(format!(
            "模型 {model_name} 缺少上下文窗口或最大输出 Token，请获取模型能力或手动填写后保存。"
        )));
    };
    let supports_developer_role = model_file.supports_developer_role.unwrap_or(false);
    let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
    let undeclared_variants = std::collections::BTreeMap::new();
    let reasoning_variants = model_file
        .reasoning_variants
        .as_ref()
        .unwrap_or(&undeclared_variants);
    validate_reasoning_variants(
        protocol,
        reasoning_variants,
        model_file.default_variant.as_deref(),
    )?;
    let thinking_wire_format =
        parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
    let chat_output_tokens_field =
        parse_chat_output_tokens_field(model_file.chat_output_tokens_field.as_deref(), protocol)?;
    if model_file
        .requires_reasoning_content_for_tool_calls
        .is_some()
        && protocol != ProviderApiProtocol::Chat
    {
        return Err(configuration_error(
            "requires_reasoning_content_for_tool_calls only applies to Chat",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    if model_file.requires_assistant_content_for_tool_calls && protocol != ProviderApiProtocol::Chat
    {
        return Err(configuration_error(
            "requires_assistant_content_for_tool_calls only applies to Chat",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    if max_context_tokens == 0 || max_output_tokens == 0 {
        return Err(configuration_error(
            "model context and output capacities must be positive",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    if max_output_tokens >= max_context_tokens {
        return Err(configuration_error(
            "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    let requested_variant = requested_variant.or(model_file.default_variant.as_deref());
    let (reasoning_variant, wire_reasoning_effort) = match requested_variant {
        None => (None, None),
        Some(requested_variant) => {
            let variant = reasoning_variants.get(requested_variant).ok_or_else(|| {
                configuration_error(
                    "model selector references an unknown or disallowed reasoning variant",
                    "provider_selector_unknown_reasoning_variant",
                )
            })?;
            (
                Some(requested_variant.to_string()),
                variant.wire_effort.clone(),
            )
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
        supports_developer_role,
        supports_tool_choice,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls
            .unwrap_or(false)
            && reasoning_variant.as_deref() != Some("off"),
        reasoning_variant,
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
    })
}
