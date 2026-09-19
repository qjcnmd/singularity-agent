//! 从冻结配置直接解析选中的提供方、模型能力与推理变体。

use super::*;

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
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

/// 一个完全解析的目录选择。把规范变体、启用状态与单一 wire effort 放在
/// 一起，避免第二张运行时映射表悄悄改变 provider 请求。
#[derive(Clone)]
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) reasoning_enabled: bool,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) chat_output_tokens_field: String,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
}

pub(crate) struct ParsedModelSelector<'a> {
    pub(crate) provider_name: &'a str,
    pub(crate) model_name: &'a str,
    pub(crate) reasoning_effort: Option<&'a str>,
}

/// 模型选择器各段：provider/model#effort。宽松拆分时任一段都可能缺省，
/// 不在此处校验合法性（校验由 parse_model_selector 与上游配置层负责）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectorParts<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
}

/// 宽松拆分 provider/model#effort 选择器：分隔符为 / 与 #，# 优先于 /
/// 拆分 effort。缺省字段在对应位返回 None，空字符串视为缺省。
pub fn split_model_selector(selector: &str) -> ModelSelectorParts<'_> {
    let (provider, model, effort) = selector_segments(selector);
    ModelSelectorParts {
        provider: provider.filter(|value| !value.is_empty()),
        model: Some(model).filter(|value| !value.is_empty()),
        effort: effort.filter(|value| !value.is_empty()),
    }
}

fn selector_segments(selector: &str) -> (Option<&str>, &str, Option<&str>) {
    let (without_effort, effort) = selector
        .rsplit_once('#')
        .map_or((selector, None), |(model, effort)| (model, Some(effort)));
    let (provider, model) = without_effort
        .split_once('/')
        .map_or((None, without_effort), |(provider, model)| {
            (Some(provider), model)
        });
    (provider, model, effort)
}

/// 组合 provider/model[#effort] 选择器；effort 为空时省略。与
/// split_model_selector 互逆（段内容不校验，合法性由配置层负责）。
pub fn compose_model_selector(provider: &str, model: &str, effort: Option<&str>) -> String {
    let mut selector = format!("{provider}/{model}");
    if let Some(effort) = effort.filter(|value| !value.is_empty()) {
        selector.push('#');
        selector.push_str(effort);
    }
    selector
}

pub(crate) fn parse_model_selector(
    selector: &str,
) -> Result<ParsedModelSelector<'_>, ProviderError> {
    let (Some(provider_name), model_name, reasoning_effort) = selector_segments(selector) else {
        return Err(configuration_error(
            "model selector must use provider_id/model_id[#variant]",
            "provider_selector_invalid",
        ));
    };
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
    if let Some(reasoning_effort) = reasoning_effort {
        super::validate_identifier(reasoning_effort, "reasoning variant").map_err(|_| {
            configuration_error(
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

pub(super) fn resolve_model_selection(
    data: &UserConfigData,
    selector: Option<&str>,
) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
    let selected = selector
        .or(data.config.default_model.as_deref())
        .ok_or_else(|| {
            configuration_error(
                "user provider config must declare default_model",
                "provider_selector_invalid",
            )
        })?;
    let parsed = parse_model_selector(selected)?;
    if selector.is_none()
        && data
            .config
            .default_provider
            .as_deref()
            .is_some_and(|provider| provider != parsed.provider_name)
    {
        return Err(configuration_error(
            "default_provider does not match default_model",
            "provider_selector_invalid",
        ));
    }
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
    let model = resolve_model_definition(model, parsed.model_name, parsed.reasoning_effort)?;
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
    model_name: &str,
    requested_variant: Option<&str>,
) -> Result<SelectedModel, ProviderError> {
    // api_protocol 必须由用户显式声明。
    let Some(api_protocol) = model_file.api_protocol.as_deref() else {
        return Err(configuration_error(
            "user config model must declare api_protocol (chat or responses)",
            "provider_configuration_invalid",
        ));
    };
    let protocol = parse_catalog_protocol(api_protocol)?;
    // 容量只有两个来源：用户显式声明，或未声明时的保守下界。不按模型 id 猜容量：
    // 同一个 id 在不同网关下的限额可以不同，猜大了会撞上下文溢出。
    let max_context_tokens = model_file
        .max_context_tokens
        .unwrap_or(crate::DEFAULT_MAX_CONTEXT_TOKENS);
    let max_output_tokens = model_file
        .max_output_tokens
        .unwrap_or(crate::DEFAULT_MAX_OUTPUT_TOKENS);
    let supports_developer_role = model_file.supports_developer_role.unwrap_or(false);
    let supports_tool_choice = model_file.supports_tool_choice.unwrap_or(true);
    let reasoning_variants = &model_file.reasoning_variants;
    validate_reasoning_variants(
        protocol,
        reasoning_variants,
        model_file.default_variant.as_deref(),
    )?;
    let thinking_wire_format =
        parse_thinking_wire_format(model_file.thinking_wire_format.as_deref(), protocol)?;
    let chat_output_tokens_field =
        parse_chat_output_tokens_field(model_file.chat_output_tokens_field.as_deref(), protocol)?;
    if model_file.requires_assistant_content_for_tool_calls && protocol != ProviderApiProtocol::Chat
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
        max_output_tokens,
        "max_output_tokens",
        MAX_CONFIGURED_OUTPUT_TOKENS,
    )?;
    if max_output_tokens >= max_context_tokens {
        return Err(configuration_error(
            "invalid model configuration: max_output_tokens must be smaller than max_context_tokens",
            "provider_configuration_invalid",
        ));
    }
    let requested_variant = requested_variant.or(model_file.default_variant.as_deref());
    let (reasoning_variant, reasoning_enabled, wire_reasoning_effort) = match requested_variant {
        None => (None, false, None),
        Some(requested_variant) => {
            let variant = model_file
                .reasoning_variants
                .get(requested_variant)
                .ok_or_else(|| {
                    configuration_error(
                        "model selector references an unknown or disallowed reasoning variant",
                        "provider_selector_unknown_reasoning_variant",
                    )
                })?;
            // validate_reasoning_variants 已保证 enabled=false 的变体只能是 “off”，
            // 这里的变体也因此必然可被选中。
            let reasoning_enabled = variant.enabled;
            (
                Some(requested_variant.to_string()),
                reasoning_enabled,
                variant.wire_effort.clone(),
            )
        }
    };
    Ok(SelectedModel {
        model_name: model_name.to_string(),
        api_protocol: protocol,
        max_context_tokens,
        max_output_tokens,
        reasoning_variant: reasoning_variant.clone(),
        reasoning_enabled,
        wire_reasoning_effort,
        thinking_wire_format,
        chat_output_tokens_field,
        supports_developer_role,
        supports_tool_choice,
        requires_reasoning_content_for_tool_calls: model_file
            .requires_reasoning_content_for_tool_calls
            && (reasoning_variant.is_none() || reasoning_enabled),
        requires_assistant_content_for_tool_calls: model_file
            .requires_assistant_content_for_tool_calls,
    })
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod selection_tests;
