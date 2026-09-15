//! 编译期模型限额表。
//!
//! 用户配置未声明限额时，先按模型 id 查内置表；未知模型使用保守默认值。

use crate::{DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS};

pub(crate) const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
pub(crate) const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

const MODEL_TABLES: &[&[(&str, u32, u32)]] = &[DEEPSEEK_MODELS, OPENAI_MODELS, ANTHROPIC_MODELS];

/// 模型 id 自身区分厂商（deepseek-*、gpt-*、claude-*），限额因此只按 id 匹配，
/// 不受用户给 provider 起的名字影响。
pub(crate) fn resolve_model_limits(model: &str) -> (u32, u32) {
    MODEL_TABLES
        .iter()
        .flat_map(|table| table.iter())
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
        let limits = resolve_model_limits("deepseek-v4-flash");
        assert_eq!(resolve_model_limits("DEEPSEEK-V4-FLASH"), limits);
    }

    #[test]
    fn unknown_model_uses_conservative_defaults() {
        assert_eq!(
            resolve_model_limits("unknown-model"),
            (DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }
}
