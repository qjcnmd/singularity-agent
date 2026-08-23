//! 内置常见模型静态能力表。
//!
//! 在用户未显式配置特定模型参数时，提供默认的上下文窗口（Context Window）与
//! 最大输出 Token 上限。用户配置中声明的模型参数具有更高优先级。

/// 一个内置模型静态能力条目。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuiltinModel {
    pub provider: &'static str,
    pub model: &'static str,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// 内置模型静态能力表，按 provider/model 精确匹配。
pub const BUILTIN_MODELS: &[BuiltinModel] = &[
    BuiltinModel {
        provider: "opencode-go",
        model: "deepseek-v4-flash",
        context_window: 1_000_000,
        max_output_tokens: 384_000,
    },
    BuiltinModel {
        provider: "dashscope",
        model: "deepseek-v4-flash-0731",
        context_window: 1_000_000,
        max_output_tokens: 384_000,
    },
    BuiltinModel {
        provider: "longcat",
        model: "LongCat-2.0",
        context_window: 1_000_000,
        max_output_tokens: 131_072,
    },
];

/// 按 provider/model 查找内置模型条目；未命中返回 None。
pub(crate) fn builtin_model(provider: &str, model: &str) -> Option<&'static BuiltinModel> {
    BUILTIN_MODELS
        .iter()
        .find(|entry| entry.provider == provider && entry.model == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_limits_match_spec() {
        let opencode = builtin_model("opencode-go", "deepseek-v4-flash").expect("entry");
        assert_eq!(opencode.context_window, 1_000_000);
        assert_eq!(opencode.max_output_tokens, 384_000);

        let longcat = builtin_model("longcat", "LongCat-2.0").expect("entry");
        assert_eq!(longcat.context_window, 1_000_000);
        assert_eq!(longcat.max_output_tokens, 131_072);
    }
}
