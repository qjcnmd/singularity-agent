//! provider 模型 schema 与配置校验（自 `config.rs` 拆出；N8）。
//!
//! 纯 schema 类型（models.json/config.json 反序列化目标）与无副作用的
//! 校验函数；快照捕获、provider 解析、用户配置文件生命周期见父模块 `config`。

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::{
    OpenAiProvider, ProviderApiProtocol, ProviderError, ProviderToolReasoningMode,
    ThinkingWireFormat, configuration_error,
};

/// 脱敏的模型提供方配置存在性信息；这里永不存储敏感信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

/// 模型提供方初始化无法继续时报告的稳定阻塞类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelBlockerKind {
    RequiredConfigMissing,
    AuthenticationProviderError,
    BaseUrlNetworkError,
    ModelNameConfigError,
}

/// 快照内部的脱敏模型提供方就绪状态与阻塞信息；认证材料不进入本结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigurationStatus {
    pub configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub api_key_status: String,
    pub base_url_status: String,
    pub blocker: Option<ModelBlockerKind>,
}

#[derive(Clone)]
pub(crate) struct ConfiguredProvider {
    pub(crate) provider: Option<OpenAiProvider>,
    pub(crate) provider_error: Option<ProviderError>,
    pub(crate) models: BTreeMap<String, ConfiguredModel>,
}

#[derive(Clone)]
pub(crate) struct ConfiguredModel {
    pub(crate) protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    pub(crate) default_variant: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) tool_reasoning_mode: ProviderToolReasoningMode,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
}

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

pub(crate) fn validate_provider_identifier(value: &str, label: &str) -> Result<(), ProviderError> {
    validate_identifier(value, label)
}

pub(crate) fn parse_catalog_protocol(value: &str) -> Result<ProviderApiProtocol, ProviderError> {
    match value {
        "chat" => Ok(ProviderApiProtocol::OpenAiChatCompletions),
        "responses" => Ok(ProviderApiProtocol::OpenAiResponses),
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
    let format = match value.unwrap_or("thinking_type") {
        "thinking_type" => ThinkingWireFormat::ThinkingType,
        "enable_thinking" => ThinkingWireFormat::EnableThinking,
        "reasoning_effort" => ThinkingWireFormat::ReasoningEffort,
        _ => {
            return Err(configuration_error(
                "thinking_wire_format must be thinking_type, enable_thinking, or reasoning_effort",
                "provider_configuration_invalid",
            ));
        }
    };
    if format == ThinkingWireFormat::EnableThinking
        && protocol != ProviderApiProtocol::OpenAiChatCompletions
    {
        return Err(configuration_error(
            "enable_thinking is only valid for Chat Completions",
            "provider_configuration_invalid",
        ));
    }
    Ok(format)
}

pub(crate) fn parse_tool_reasoning_history(
    value: Option<&str>,
    protocol: ProviderApiProtocol,
) -> Result<ProviderToolReasoningMode, ProviderError> {
    match value.unwrap_or("disabled") {
        "disabled" => Ok(ProviderToolReasoningMode::Unspecified),
        "reasoning_content" if protocol == ProviderApiProtocol::OpenAiChatCompletions => {
            Ok(ProviderToolReasoningMode::ReplayReasoningContent)
        }
        "responses_items" if protocol == ProviderApiProtocol::OpenAiResponses => {
            Ok(ProviderToolReasoningMode::ReplayResponsesItems)
        }
        "reasoning_content" | "responses_items" => Err(configuration_error(
            "tool_reasoning_history does not match api_protocol",
            "provider_configuration_invalid",
        )),
        _ => Err(configuration_error(
            "tool_reasoning_history must be disabled, reasoning_content, or responses_items",
            "provider_configuration_invalid",
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
            && protocol == ProviderApiProtocol::OpenAiResponses
            && descriptor.wire_effort.is_none()
        {
            return Err(configuration_error(
                "Responses enabled reasoning variants require wire_effort",
                "provider_configuration_invalid",
            ));
        }
    }
    if protocol == ProviderApiProtocol::OpenAiChatCompletions {
        let enabled_without_wire = variants
            .iter()
            .filter(|(_, descriptor)| descriptor.enabled && descriptor.wire_effort.is_none())
            .map(|(variant, _)| variant.as_str())
            .collect::<Vec<_>>();
        if enabled_without_wire.len() > 1
            || enabled_without_wire
                .first()
                .is_some_and(|variant| *variant != "on")
        {
            return Err(configuration_error(
                "Chat no-wire reasoning is only the single on variant",
                "provider_configuration_invalid",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_catalog_limit(
    value: Option<u32>,
    label: &str,
    upper_bound: u32,
) -> Result<(), ProviderError> {
    if value.is_some_and(|value| value == 0 || value > upper_bound) {
        return Err(configuration_error(
            format!("invalid model configuration: {label} is outside the supported range"),
            "provider_configuration_invalid",
        ));
    }
    Ok(())
}
