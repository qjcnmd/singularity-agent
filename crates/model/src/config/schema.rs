//! 提供方模型配置的结构与校验：只有纯结构类型（config.json 反序列化的目标）和不产生
//! 副作用的校验函数；快照捕获、提供方解析和用户配置文件的读写流程见父模块 config。

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{ProviderApiProtocol, ProviderError, ThinkingWireFormat, configuration_error};
use crate::openai::wire::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsFileReasoningVariant {
    /// 缺省表示「没有单独的线上档位」；未声明时不写回，不给变体凭空补这个键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_effort: Option<String>,
}

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    validate_identifier_characters(value, label, &['/', '#'])
}

pub(crate) fn validate_model_id(value: &str, label: &str) -> Result<(), ProviderError> {
    validate_identifier_characters(value, label, &['#'])
}

fn validate_identifier_characters(
    value: &str,
    label: &str,
    forbidden_separators: &[char],
) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || forbidden_separators.contains(&character)
        })
    {
        return Err(configuration_error(
            format!("模型配置无效：{label} 不能为空，也不能包含空白、控制字符或保留分隔符"),
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    Ok(())
}

pub(crate) fn parse_thinking_wire_format(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<ThinkingWireFormat, ProviderError> {
    let format = match value {
        None => ThinkingWireFormat::DEFAULT,
        Some(value) => ThinkingWireFormat::from_wire_name(value).ok_or_else(|| {
            configuration_error(
                format!("thinking_wire_format 只能填写：{}", ThinkingWireFormat::names()),
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            )
        })?,
    };
    if value.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "thinking_wire_format 仅适用于 Chat Completions",
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
            "chat_output_tokens_field 仅适用于 Chat",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    match declared.unwrap_or(DEFAULT_CHAT_OUTPUT_TOKENS_FIELD) {
        "max_tokens" => Ok("max_tokens"),
        "max_completion_tokens" => Ok("max_completion_tokens"),
        _ => Err(configuration_error(
            "chat_output_tokens_field 只能填写 max_tokens 或 max_completion_tokens",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        )),
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
                "reasoning_variants 为空时不能设置 default_variant",
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            ));
        }
        return Ok(());
    }
    if default_variant.is_some_and(|variant| !variants.contains_key(variant)) {
        return Err(configuration_error(
            "default_variant 指定的档位未在 reasoning_variants 中声明",
            crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
        ));
    }
    for (variant, descriptor) in variants {
        validate_identifier(variant, "推理档位")?;
        if variant == "off" && descriptor.wire_effort.is_some() {
            return Err(configuration_error(
                "off 推理档位不能设置 wire_effort",
                crate::error::PROVIDER_CONFIGURATION_INVALID_CODE,
            ));
        }
        if let Some(wire_effort) = descriptor.wire_effort.as_deref() {
            validate_identifier(wire_effort, "wire_effort")?;
        }
        if variant != "off" && descriptor.wire_effort.is_none() {
            let message = match protocol {
                ProviderApiProtocol::Responses => "Responses 的非 off 推理档位须设置 wire_effort",
                ProviderApiProtocol::Chat if variant != "on" => {
                    "Chat 中不设置 wire_effort 的非 off 推理档位只能命名为 on"
                }
                ProviderApiProtocol::Chat => continue,
            };
            return Err(configuration_error(message, crate::error::PROVIDER_CONFIGURATION_INVALID_CODE));
        }
    }
    Ok(())
}
