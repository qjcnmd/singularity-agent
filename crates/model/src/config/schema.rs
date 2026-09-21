//! Provider 模型配置结构与校验。
//!
//! 纯 schema 类型（config.json 反序列化目标）与无副作用的
//! 校验函数；快照捕获、provider 解析、用户配置文件生命周期见父模块 config。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{ProviderApiProtocol, ProviderError, ThinkingWireFormat, configuration_error};
use crate::openai::wire::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsFileReasoningVariant {
    pub enabled: bool,
    /// 缺省即「无独立 wire 档位」；未声明时不写回，避免保存动作给变体补出键。
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
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_model_id(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.chars().count() > crate::MAX_MODEL_ID_LENGTH
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '#'
        })
    {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is malformed"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}

pub(crate) fn parse_catalog_protocol(value: &str) -> Result<ProviderApiProtocol, ProviderError> {
    ProviderApiProtocol::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value),
    )
    .map_err(|error| {
        configuration_error(
            format!("invalid model configuration: api_protocol: {error}"),
            "provider_configuration_invalid",
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
                format!(
                    "thinking_wire_format must be one of: {}",
                    ThinkingWireFormat::names()
                ),
                "provider_configuration_invalid",
            )
        })?,
    };
    if format == ThinkingWireFormat::EnableThinking && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "enable_thinking is only valid for Chat Completions",
            "provider_configuration_invalid",
        ));
    }
    Ok(format)
}

/// 解析 Chat 输出上限的 wire 字段。取值就是要发送的 JSON 字段名，因此没有
/// 词表可校验；空值表示没有声明，按缺省处理。Responses 不使用该字段，声明了
/// 非空值即为配错。
pub(crate) fn parse_chat_output_tokens_field(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<String, ProviderError> {
    let declared = value.filter(|value| !value.is_empty());
    if declared.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "chat_output_tokens_field only applies to Chat",
            "provider_configuration_invalid",
        ));
    }
    Ok(declared
        .unwrap_or(DEFAULT_CHAT_OUTPUT_TOKENS_FIELD)
        .to_string())
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
                "provider_configuration_invalid",
            ));
        }
        return Ok(());
    }
    let Some(default_variant) = default_variant else {
        return Err(configuration_error(
            "reasoning_variants require an explicit default_variant",
            "provider_configuration_invalid",
        ));
    };
    if !variants.contains_key(default_variant) {
        return Err(configuration_error(
            "default_variant is not declared in reasoning_variants",
            "provider_configuration_invalid",
        ));
    }
    for (variant, descriptor) in variants {
        validate_identifier(variant, "reasoning variant")?;
        if variant == "off" && descriptor.enabled {
            return Err(configuration_error(
                "the off reasoning variant must be explicitly disabled",
                "provider_configuration_invalid",
            ));
        }
        if variant != "off" && !descriptor.enabled {
            return Err(configuration_error(
                "non-off reasoning variants must be enabled",
                "provider_configuration_invalid",
            ));
        }
        if !descriptor.enabled && descriptor.wire_effort.is_some() {
            return Err(configuration_error(
                "disabled reasoning variants cannot declare a wire effort",
                "provider_configuration_invalid",
            ));
        }
        if let Some(wire_effort) = descriptor.wire_effort.as_deref() {
            validate_identifier(wire_effort, "wire reasoning effort")?;
        }
        if descriptor.enabled
            && protocol == ProviderApiProtocol::Responses
            && descriptor.wire_effort.is_none()
        {
            return Err(configuration_error(
                "Responses enabled reasoning variants require wire_effort",
                "provider_configuration_invalid",
            ));
        }
    }
    if protocol == ProviderApiProtocol::Chat {
        // 无 wire 的启用变体只允许单独存在的 on：键唯一，另一个无 wire 启用项
        // 若不是 on 就已非法，若是 on 则会出现两个，同样非法。
        let illegal = variants.iter().any(|(variant, descriptor)| {
            descriptor.enabled && descriptor.wire_effort.is_none() && variant != "on"
        });
        if illegal {
            return Err(configuration_error(
                "Chat no-wire reasoning is only the single on variant",
                "provider_configuration_invalid",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_catalog_limit(
    value: u32,
    label: &str,
    upper_bound: u32,
) -> Result<(), ProviderError> {
    if value == 0 || value > upper_bound {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is outside the supported range"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}
