//! 提供方模型配置的结构与校验：只有纯结构类型（config.json 反序列化的目标）和不产生
//! 副作用的校验函数；快照捕获、提供方解析和用户配置文件的读写流程见父模块 config。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{ProviderApiProtocol, ProviderError, ThinkingWireFormat, configuration_error};
use crate::openai::wire::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsFileReasoningVariant {
    /// 缺省表示「没有单独的线上档位」；未声明时保存不写回，免得给变体凭空补出这个键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_effort: Option<String>,
}

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || matches!(character, '/' | '#')
        })
    {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is malformed"),
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    Ok(())
}

pub(crate) fn validate_model_id(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control() || character == '#')
    {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is malformed"),
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    Ok(())
}

pub(crate) fn parse_catalog_protocol(value: &str) -> Result<ProviderApiProtocol, ProviderError> {
    ProviderApiProtocol::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value))
        .map_err(|error| {
            configuration_error(
                format!("invalid model configuration: api_protocol: {error}"),
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            )
        })
}

pub(crate) fn parse_thinking_wire_format(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<ThinkingWireFormat, ProviderError> {
    let format = match value {
        None => ThinkingWireFormat::DEFAULT,
        Some(value) => ThinkingWireFormat::from_wire_name(value).ok_or_else(|| {
            configuration_error(
                format!("thinking_wire_format must be one of: {}", ThinkingWireFormat::names()),
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            )
        })?,
    };
    if value.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "thinking_wire_format is only valid for Chat Completions",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    Ok(format)
}

/// Chat 支持两种输出上限字段；Responses 使用自己的固定字段。
pub(crate) fn parse_chat_output_tokens_field(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<&'static str, ProviderError> {
    let declared = value.filter(|value| !value.is_empty());
    if declared.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "chat_output_tokens_field only applies to Chat",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    match declared.unwrap_or(DEFAULT_CHAT_OUTPUT_TOKENS_FIELD) {
        "max_tokens" => Ok("max_tokens"),
        "max_completion_tokens" => Ok("max_completion_tokens"),
        _ => Err(configuration_error(
            "chat_output_tokens_field must be max_tokens or max_completion_tokens",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        )),
    }
}

/// 编辑与模型发现共用的协议边界；Responses 不携带 Chat 专用请求选项。
pub(super) fn normalize_chat_fields(
    protocol: ProviderApiProtocol,
    thinking_wire_format: &mut Option<String>,
    chat_output_tokens_field: &mut Option<String>,
    requires_reasoning_content_for_tool_calls: &mut Option<bool>,
) {
    if protocol == ProviderApiProtocol::Responses {
        *thinking_wire_format = None;
        *chat_output_tokens_field = None;
        *requires_reasoning_content_for_tool_calls = None;
    }
}

pub(crate) fn validate_reasoning_variants(
    protocol: ProviderApiProtocol,
    variants: &BTreeMap<String, ModelsFileReasoningVariant>,
    default_variant: Option<&str>,
) -> Result<(), ProviderError> {
    if variants.is_empty() {
        if default_variant.is_some() {
            return Err(configuration_error(
                "default_variant must be omitted when reasoning_variants is empty",
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            ));
        }
        return Ok(());
    }
    let Some(default_variant) = default_variant else {
        return Err(configuration_error(
            "reasoning_variants require an explicit default_variant",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    };
    if !variants.contains_key(default_variant) {
        return Err(configuration_error(
            "default_variant is not declared in reasoning_variants",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    for (variant, descriptor) in variants {
        validate_identifier(variant, "reasoning variant")?;
        if variant == "off" && descriptor.wire_effort.is_some() {
            return Err(configuration_error(
                "the off reasoning variant cannot declare a wire effort",
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            ));
        }
        if let Some(wire_effort) = descriptor.wire_effort.as_deref() {
            validate_identifier(wire_effort, "wire reasoning effort")?;
        }
        if variant != "off" && descriptor.wire_effort.is_none() {
            let message = match protocol {
                ProviderApiProtocol::Responses => "Responses enabled reasoning variants require wire_effort",
                ProviderApiProtocol::Chat if variant != "on" => {
                    "Chat no-wire reasoning is only the single on variant"
                }
                ProviderApiProtocol::Chat => continue,
            };
            return Err(configuration_error(message, crate::error::PROVIDER_CONFIGURATION_INVALID_CODE));
        }
    }
    Ok(())
}
