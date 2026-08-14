//! 内置模型静态表（Pi 式声明）与成本估算。
//!
//! 只内置用户确认的 provider（opencode-go、longcat；dashscope 按用户裁决排除）。
//! 价格是 Phase 8 规格合同的一手来源：opencode-go 取自 Pi
//! `pi-ai/dist/providers/data/opencode-go.json`；longcat 取自 longcat.chat 官方
//! 定价文档。单价单位为每百万 token，币种按 provider 计费币种（两者均为 USD）。
//! 用户 config.json 显式声明始终优先；本表只在声明缺省时兜底 context window /
//! max output tokens，并供 usage 聚合点查询成本估算。

/// 单个模型的每百万 token 价格，按 provider 计费币种计价。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCost {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub currency: &'static str,
}

/// 一个内置模型声明条目。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuiltinModel {
    pub provider: &'static str,
    pub model: &'static str,
    /// 线路协议（信息性字段，不参与成本匹配，也不参与配置合并）。
    pub api_protocol: &'static str,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub reasoning: bool,
    pub cost: Option<ModelCost>,
}

/// 内置模型静态表。按 (provider, model) 精确匹配。
pub const BUILTIN_MODELS: &[BuiltinModel] = &[
    BuiltinModel {
        provider: "opencode-go",
        model: "deepseek-v4-flash",
        api_protocol: "responses",
        context_window: 1_000_000,
        max_output_tokens: 384_000,
        reasoning: true,
        cost: Some(ModelCost {
            input_per_mtok: 0.14,
            output_per_mtok: 0.28,
            cache_read_per_mtok: 0.0028,
            currency: "USD",
        }),
    },
    BuiltinModel {
        provider: "longcat",
        model: "LongCat-2.0",
        api_protocol: "responses",
        context_window: 1_000_000,
        max_output_tokens: 131_072,
        reasoning: true,
        cost: Some(ModelCost {
            input_per_mtok: 0.75,
            output_per_mtok: 2.95,
            cache_read_per_mtok: 0.015,
            currency: "USD",
        }),
    },
];

/// 按 (provider, model) 查找内置模型条目；未命中返回 `None`。
pub(crate) fn builtin_model(provider: &str, model: &str) -> Option<&'static BuiltinModel> {
    BUILTIN_MODELS
        .iter()
        .find(|entry| entry.provider == provider && entry.model == model)
}

/// 按 (provider, model) 查询内置价格；无内置价格的模型（含 dashscope）返回 `None`。
pub fn builtin_model_cost(provider: &str, model: &str) -> Option<ModelCost> {
    builtin_model(provider, model).and_then(|entry| entry.cost)
}

/// 按每百万 token 单价估算一次 usage 的成本（币种随单价）。
///
/// `cost = input×in_price + output×out_price + cached×cache_read_price`，再除以 1e6。
pub fn estimate_cost(
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cost: &ModelCost,
) -> f64 {
    (input_tokens as f64 * cost.input_per_mtok
        + output_tokens as f64 * cost.output_per_mtok
        + cached_input_tokens as f64 * cost.cache_read_per_mtok)
        / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_cost_matches_spec_prices() {
        let opencode = builtin_model_cost("opencode-go", "deepseek-v4-flash")
            .expect("opencode-go builtin price");
        assert_eq!(opencode.input_per_mtok, 0.14);
        assert_eq!(opencode.output_per_mtok, 0.28);
        assert_eq!(opencode.cache_read_per_mtok, 0.0028);
        assert_eq!(opencode.currency, "USD");

        let longcat = builtin_model_cost("longcat", "LongCat-2.0").expect("longcat builtin price");
        assert_eq!(longcat.input_per_mtok, 0.75);
        assert_eq!(longcat.output_per_mtok, 2.95);
        assert_eq!(longcat.cache_read_per_mtok, 0.015);
        assert_eq!(longcat.currency, "USD");
    }

    #[test]
    fn builtin_cost_misses_for_unknown_provider_or_model() {
        // dashscope 按用户裁决排除在价格表之外。
        assert_eq!(
            builtin_model_cost("dashscope", "deepseek-v4-flash-0731"),
            None
        );
        assert_eq!(builtin_model_cost("opencode-go", "unknown-model"), None);
        assert_eq!(
            builtin_model_cost("unknown-provider", "deepseek-v4-flash"),
            None
        );
        assert_eq!(builtin_model_cost("", ""), None);
    }

    #[test]
    fn builtin_limits_match_spec() {
        let opencode = builtin_model("opencode-go", "deepseek-v4-flash").expect("entry");
        assert_eq!(opencode.context_window, 1_000_000);
        assert_eq!(opencode.max_output_tokens, 384_000);
        assert!(opencode.reasoning);

        let longcat = builtin_model("longcat", "LongCat-2.0").expect("entry");
        assert_eq!(longcat.context_window, 1_000_000);
        assert_eq!(longcat.max_output_tokens, 131_072);
        assert!(longcat.reasoning);
    }

    #[test]
    fn estimate_cost_matches_spec_example() {
        let cost = builtin_model_cost("opencode-go", "deepseek-v4-flash").expect("price");
        // 规格示例：1M input + 1M output + 1M cache → 0.14 + 0.28 + 0.0028 = 0.4228。
        let estimate = estimate_cost(1_000_000, 1_000_000, 1_000_000, &cost);
        assert!((estimate - 0.4228).abs() < 1e-9, "estimate was {estimate}");
    }

    #[test]
    fn estimate_cost_scales_linearly_and_is_zero_without_tokens() {
        let cost = builtin_model_cost("longcat", "LongCat-2.0").expect("price");
        assert!((estimate_cost(0, 0, 1_000_000, &cost) - 0.015).abs() < 1e-12);
        assert!(
            (estimate_cost(2_000, 1_000, 0, &cost) - (2_000.0 * 0.75 + 1_000.0 * 2.95) / 1e6).abs()
                < 1e-12
        );
        assert_eq!(estimate_cost(0, 0, 0, &cost), 0.0);
    }
}
