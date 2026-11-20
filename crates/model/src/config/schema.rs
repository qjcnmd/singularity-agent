//! 模型配置字段的校验：检查标识符和推理档位。
//! 提供方解析和 `config.json` 读写见父模块 `config`。

use super::{ProviderApiProtocol, ProviderError, configuration_error};

pub(crate) fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    if value.is_empty() || value.chars().any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(configuration_error(format!("模型配置无效：{label} 不能为空，也不能包含空白或控制字符")));
    }
    Ok(())
}

pub(crate) fn validate_reasoning_variants(
    protocol: ProviderApiProtocol,
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
    }
    Ok(())
}
