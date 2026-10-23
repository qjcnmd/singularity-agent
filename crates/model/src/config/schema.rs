//! 模型配置字段的校验：检查标识符、思考字段格式、Chat 输出上限字段名和推理档位。
//! 提供方解析和 `config.json` 读写见父模块 `config`。

use super::{ProviderApiProtocol, ProviderError, ThinkingWireFormat, configuration_error};
use crate::openai::wire::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD;

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty() || value.chars().any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(configuration_error(format!("模型配置无效：{label} 不能为空，也不能包含空白或控制字符")));
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
            configuration_error(format!("thinking_wire_format 只能填写：{}", ThinkingWireFormat::names()))
        })?,
    };
    if value.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error("thinking_wire_format 仅适用于 Chat Completions"));
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
        return Err(configuration_error("chat_output_tokens_field 仅适用于 Chat"));
    }
    match declared.unwrap_or(DEFAULT_CHAT_OUTPUT_TOKENS_FIELD) {
        "max_tokens" => Ok("max_tokens"),
        "max_completion_tokens" => Ok("max_completion_tokens"),
        _ => Err(configuration_error(
            "chat_output_tokens_field 只能填写 max_tokens 或 max_completion_tokens",
        )),
    }
}

pub(crate) fn validate_reasoning_variants(
    protocol: ProviderApiProtocol,
    thinking_wire_format: ThinkingWireFormat,
    variants: &[String],
) -> Result<(), ProviderError> {
    for (index, variant) in variants.iter().enumerate() {
        validate_identifier(variant, "推理档位")?;
        if variants[..index].contains(variant) {
            return Err(configuration_error("推理档位不能重复"));
        }
        if protocol == ProviderApiProtocol::Responses && variant == "on" {
            return Err(configuration_error("Responses 须选择具体思考档位，不支持 on 开关"));
        }
        if thinking_wire_format == ThinkingWireFormat::ReasoningEffort && variant == "on" {
            return Err(configuration_error("reasoning_effort 须选择具体思考档位，不支持 on 开关"));
        }
    }
    Ok(())
}
