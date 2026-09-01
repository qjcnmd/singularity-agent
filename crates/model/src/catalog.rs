//! 编译期模型限额表。
//!
//! 用户配置未声明限额时，先查内置表；未知 provider/model 使用保守默认值。

use crate::{DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_PROVIDER_NAME};

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

    #[test]
    fn builtin_table_matches_exact_and_case_insensitive_model_ids() {
        let limits = resolve_model_limits("deepseek", "deepseek-v4-flash");
        assert_eq!(limits, (1_000_000, 384_000));
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
