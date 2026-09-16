//! Provider 模型配置结构与校验。
//!
//! 纯 schema 类型（config.json 反序列化目标）与无副作用的
//! 校验函数；快照捕获、provider 解析、用户配置文件生命周期见父模块 config。

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};

use super::{ProviderApiProtocol, ProviderError, ThinkingWireFormat, configuration_error};
use crate::provider::contract::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsFileReasoningVariant {
    pub enabled: bool,
    #[serde(default)]
    pub wire_effort: Option<String>,
}

pub(crate) fn deserialize_unique_map<'de, D, K, V>(
    deserializer: D,
) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Ord + DeserializeOwned,
    V: DeserializeOwned,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Ord + DeserializeOwned,
        V: DeserializeOwned,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut result = BTreeMap::new();
            while let Some(key) = access.next_key::<K>()? {
                if result.contains_key(&key) {
                    return Err(de::Error::custom("duplicate object key"));
                }
                result.insert(key, access.next_value()?);
            }
            Ok(result)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
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
    match value {
        "chat" => Ok(ProviderApiProtocol::Chat),
        "responses" => Ok(ProviderApiProtocol::Responses),
        _ => Err(configuration_error(
            "invalid model configuration: api_protocol must be chat or responses",
            "provider_configuration_invalid",
        )),
    }
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
/// 词表可校验；Responses 不使用该字段，声明即为配错。
pub(crate) fn parse_chat_output_tokens_field(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<String, ProviderError> {
    if value.is_some() && protocol != ProviderApiProtocol::Chat {
        return Err(configuration_error(
            "chat_output_tokens_field only applies to Chat",
            "provider_configuration_invalid",
        ));
    }
    Ok(value
        .filter(|value| !value.is_empty())
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
