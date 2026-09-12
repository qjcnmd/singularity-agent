//! 编译期模型限额表。
//!
//! 用户配置未声明限额时，先查内置表；未知 provider/model 使用保守默认值。

use crate::{DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_PROVIDER_NAME};

pub(crate) const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
pub(crate) const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Presets supported by the current transports, sharing the model-limit catalog.
pub(crate) fn provider_presets() -> Vec<singularity_protocol::ProviderConfigurationInput> {
    use singularity_protocol::{
        ProviderApiProtocol, ProviderConfigurationInput, ProviderModelInput,
    };
    [
        (
            "deepseek",
            "DeepSeek",
            DEEPSEEK_BASE_URL,
            ProviderApiProtocol::Chat,
            DEEPSEEK_MODELS,
        ),
        (
            "openai",
            "OpenAI",
            OPENAI_BASE_URL,
            ProviderApiProtocol::Responses,
            OPENAI_MODELS,
        ),
    ]
    .into_iter()
    .map(|(id, name, url, api, models)| ProviderConfigurationInput {
        provider_id: id.to_string(),
        display_name: Some(name.to_string()),
        base_url: url.to_string(),
        make_default: false,
        models: models
            .iter()
            .map(|(id, context, output)| ProviderModelInput {
                model_id: (*id).to_string(),
                display_name: None,
                api_protocol: api,
                max_context_tokens: Some(*context),
                max_output_tokens: Some(*output),
                reasoning_variants: Vec::new(),
                default_variant: None,
                thinking_wire_format: None,
            })
            .collect(),
    })
    .collect()
}

pub(crate) fn resolve_model_limits(provider: &str, model: &str) -> (u32, u32) {
    let models = match provider {
        "deepseek" => DEEPSEEK_MODELS,
        "openai" | DEFAULT_PROVIDER_NAME => OPENAI_MODELS,
        "anthropic" => ANTHROPIC_MODELS,
        _ => &[],
    };
    models
        .iter()
        .find(|(id, _, _)| id.eq_ignore_ascii_case(model))
        .map(|(_, context, output)| (*context, *output))
        .unwrap_or((DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS))
}

const DEEPSEEK_MODELS: &[(&str, u32, u32)] = &[
    ("deepseek-v4-flash", 1_000_000, 384_000),
    ("deepseek-v4-flash-0731", 1_000_000, 384_000),
    ("deepseek-v4-pro", 1_000_000, 384_000),
    ("deepseek-chat", 1_000_000, 384_000),
    ("deepseek-reasoner", 1_000_000, 384_000),
];

const OPENAI_MODELS: &[(&str, u32, u32)] = &[
    ("gpt-5", 400_000, 128_000),
    ("gpt-5-mini", 400_000, 128_000),
    ("gpt-5-nano", 400_000, 128_000),
    ("gpt-5-pro", 400_000, 272_000),
    ("gpt-4.1", 1_047_576, 32_768),
    ("gpt-4.1-mini", 1_047_576, 32_768),
    ("gpt-4o", 128_000, 16_384),
    ("gpt-4o-mini", 128_000, 16_384),
    ("o3", 200_000, 100_000),
    ("o3-mini", 200_000, 100_000),
    ("o4-mini", 200_000, 100_000),
];

const ANTHROPIC_MODELS: &[(&str, u32, u32)] = &[
    ("claude-opus-4-5", 200_000, 64_000),
    ("claude-opus-4-6", 1_000_000, 128_000),
    ("claude-sonnet-4-5", 1_000_000, 64_000),
    ("claude-sonnet-4-6", 1_000_000, 128_000),
    ("claude-haiku-4-5", 200_000, 64_000),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 模型 id 大小写不敏感地命中同一档位；具体数值随内置表调整，不在此重抄。
    #[test]
    fn model_id_matching_is_case_insensitive() {
        let limits = resolve_model_limits("deepseek", "deepseek-v4-flash");
        assert_eq!(
            resolve_model_limits("deepseek", "DEEPSEEK-V4-FLASH"),
            limits
        );
    }

    #[test]
    fn unknown_model_uses_conservative_defaults() {
        assert_eq!(
            resolve_model_limits("unknown-provider", "unknown-model"),
            (DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }
}
