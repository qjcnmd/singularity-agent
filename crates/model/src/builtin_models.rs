//! 内置模型静态表（Pi 式声明）与成本估算。
//!
//! 内置用户确认的 provider：opencode-go、longcat、dashscope。价格按模型官方
//! API 价格计算、不区分供应商：opencode-go 的 deepseek-v4-flash 与 dashscope 的
//! deepseek-v4-flash-0731（DeepSeek 官方模型版本号）均为 deepseek 官方定价；
//! longcat 取自 longcat.chat 官方定价文档。单价单位为每百万 token，币种均为 USD。
//!
//! DeepSeek 2026-08-17 00:00（北京时间）起执行峰谷双价（官方来源：
//! api-docs.deepseek.com/zh-cn/quick_start/pricing/）：高峰=北京 9:00-12:00、
//! 14:00-18:00，闲时=高峰一半。官方以 CNY 计价，本表按固定汇率 7.0（CNY/7.0）
//! 换算为 USD，汇率与生效日见下方常量注释。longcat 无峰谷价，峰价=闲价。
//! 用户 config.json 显式声明始终优先；本表只在声明缺省时兜底 context window /
//! max output tokens，并供 usage 聚合点查询成本估算。

/// DeepSeek 官方 CNY->USD 固定汇率（CNY / 7.0 = USD），仅适用于本表 deepseek 条目。
const DEEPSEEK_RATE: f64 = 7.0;
/// DeepSeek 峰谷定价（CNY/百万 token）换算 USD 后的常量。
/// 峰值：¥3.0/¥9.0/¥0.10；闲时：¥1.5/¥4.5/¥0.05（2026-08-17 00:00 北京生效）。
const DS_PEAK_INPUT: f64 = 3.0 / DEEPSEEK_RATE;
const DS_PEAK_OUTPUT: f64 = 9.0 / DEEPSEEK_RATE;
const DS_PEAK_CACHE_READ: f64 = 0.10 / DEEPSEEK_RATE;
const DS_OFF_INPUT: f64 = 1.5 / DEEPSEEK_RATE;
const DS_OFF_OUTPUT: f64 = 4.5 / DEEPSEEK_RATE;
const DS_OFF_CACHE_READ: f64 = 0.05 / DEEPSEEK_RATE;

/// 单个模型的每百万 token 价格，按 provider 计费币种计价。
///
/// `input_per_mtok` / `output_per_mtok` / `cache_read_per_mtok` 为闲时（或全时）价；
/// `*_peak_*` 为高峰价。无峰谷价的模型（如 longcat）峰价=闲价。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelCost {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub input_peak_per_mtok: f64,
    pub output_peak_per_mtok: f64,
    pub cache_read_peak_per_mtok: f64,
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
            input_per_mtok: DS_OFF_INPUT,
            output_per_mtok: DS_OFF_OUTPUT,
            cache_read_per_mtok: DS_OFF_CACHE_READ,
            input_peak_per_mtok: DS_PEAK_INPUT,
            output_peak_per_mtok: DS_PEAK_OUTPUT,
            cache_read_peak_per_mtok: DS_PEAK_CACHE_READ,
            currency: "USD",
        }),
    },
    BuiltinModel {
        provider: "dashscope",
        model: "deepseek-v4-flash-0731",
        api_protocol: "chat",
        context_window: 1_000_000,
        max_output_tokens: 384_000,
        reasoning: true,
        cost: Some(ModelCost {
            input_per_mtok: DS_OFF_INPUT,
            output_per_mtok: DS_OFF_OUTPUT,
            cache_read_per_mtok: DS_OFF_CACHE_READ,
            input_peak_per_mtok: DS_PEAK_INPUT,
            output_peak_per_mtok: DS_PEAK_OUTPUT,
            cache_read_peak_per_mtok: DS_PEAK_CACHE_READ,
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
            input_peak_per_mtok: 0.75,
            output_peak_per_mtok: 2.95,
            cache_read_peak_per_mtok: 0.015,
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

/// 按 (provider, model) 查询内置价格；无内置价格的模型返回 `None`。
pub fn builtin_model_cost(provider: &str, model: &str) -> Option<ModelCost> {
    builtin_model(provider, model).and_then(|entry| entry.cost)
}

/// 按每百万 token 单价估算一次 usage 的成本（币种随单价），使用闲时价。
///
/// `cost = input×in_price + output×out_price + cached×cache_read_price`，再除以 1e6。
pub fn estimate_cost(
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cost: &ModelCost,
) -> f64 {
    estimate_cost_peak(
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cost,
        false,
    )
}

/// 按每百万 token 单价估算一次 usage 的成本（币种随单价）。
///
/// `peak=true` 使用高峰价格字段，否则使用闲时价格字段。公式与 `estimate_cost` 相同。
pub fn estimate_cost_peak(
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cost: &ModelCost,
    peak: bool,
) -> f64 {
    let (input, output, cache) = if peak {
        (
            cost.input_peak_per_mtok,
            cost.output_peak_per_mtok,
            cost.cache_read_peak_per_mtok,
        )
    } else {
        (
            cost.input_per_mtok,
            cost.output_per_mtok,
            cost.cache_read_per_mtok,
        )
    };
    (input_tokens as f64 * input
        + output_tokens as f64 * output
        + cached_input_tokens as f64 * cache)
        / 1e6
}

/// 判断 `now_utc`（UTC 时刻）对应的北京时间（UTC+8）是否处于 DeepSeek 高峰时段。
///
/// 高峰 = 北京 9:00-12:00 或 14:00-18:00（左闭右开：9:00 高峰、12:00 起非高峰，
/// 14:00 高峰、18:00 起非高峰）。时区换算用 SystemTime 直接做 UTC+8 小时回卷，无
/// 外部 chrono 依赖；把当前时刻作为参数传入以便测试确定性。
pub fn is_peak_hour_utc8(now_utc: std::time::SystemTime) -> bool {
    // 距 UNIX_EPOCH（1970-01-01 00:00 UTC）的整数秒；先取整日模 86400 落到 UTC 当日，
    // 再 +8h 得当前北京小时（0-23）。SystemTime 为浮点前即失败时按 0 秒处理。
    let secs = now_utc
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let beijing_hour = ((secs + 8 * 3600) % 86_400) / 3600;
    (9..12).contains(&beijing_hour) || (14..18).contains(&beijing_hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_cost_matches_spec_prices() {
        let opencode = builtin_model_cost("opencode-go", "deepseek-v4-flash")
            .expect("opencode-go builtin price");
        // 闲时价（8/17 起）：¥1.5/¥4.5/¥0.05 → 1.5/7.0、4.5/7.0、0.05/7.0。
        assert_eq!(opencode.input_per_mtok, DS_OFF_INPUT);
        assert_eq!(opencode.output_per_mtok, DS_OFF_OUTPUT);
        assert_eq!(opencode.cache_read_per_mtok, DS_OFF_CACHE_READ);
        // 高峰价（8/17 起）：¥3.0/¥9.0/¥0.10 → 3.0/7.0、9.0/7.0、0.10/7.0，恰为闲时 2 倍。
        assert_eq!(opencode.input_peak_per_mtok, DS_PEAK_INPUT);
        assert_eq!(opencode.output_peak_per_mtok, DS_PEAK_OUTPUT);
        assert_eq!(opencode.cache_read_peak_per_mtok, DS_PEAK_CACHE_READ);
        assert_eq!(opencode.currency, "USD");

        let longcat = builtin_model_cost("longcat", "LongCat-2.0").expect("longcat builtin price");
        assert_eq!(longcat.input_per_mtok, 0.75);
        assert_eq!(longcat.output_per_mtok, 2.95);
        assert_eq!(longcat.cache_read_per_mtok, 0.015);
        // longcat 无峰谷价，峰价=闲价，行为不变。
        assert_eq!(longcat.input_peak_per_mtok, longcat.input_per_mtok);
        assert_eq!(longcat.output_peak_per_mtok, longcat.output_per_mtok);
        assert_eq!(
            longcat.cache_read_peak_per_mtok,
            longcat.cache_read_per_mtok
        );
        assert_eq!(longcat.currency, "USD");

        // dashscope 托管 deepseek 官方模型版本 0731，按 deepseek 官方定价（不区分供应商）。
        let dashscope =
            builtin_model_cost("dashscope", "deepseek-v4-flash-0731").expect("dashscope price");
        assert_eq!(dashscope.input_per_mtok, DS_OFF_INPUT);
        assert_eq!(dashscope.output_per_mtok, DS_OFF_OUTPUT);
        assert_eq!(dashscope.cache_read_per_mtok, DS_OFF_CACHE_READ);
        assert_eq!(dashscope.input_peak_per_mtok, DS_PEAK_INPUT);
        assert_eq!(dashscope.output_peak_per_mtok, DS_PEAK_OUTPUT);
        assert_eq!(dashscope.cache_read_peak_per_mtok, DS_PEAK_CACHE_READ);
        assert_eq!(dashscope.currency, "USD");
    }

    #[test]
    fn builtin_cost_misses_for_unknown_provider_or_model() {
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
    fn estimate_cost_matches_off_peak_prices() {
        let cost = builtin_model_cost("opencode-go", "deepseek-v4-flash").expect("price");
        // 8/17 起闲时价：1M input + 1M output + 1M cache →
        // 1.5/7.0 + 4.5/7.0 + 0.05/7.0 = (1.5+4.5+0.05)/7.0 = 6.05/7.0。
        let estimate = estimate_cost(1_000_000, 1_000_000, 1_000_000, &cost);
        let expected = (1.5 + 4.5 + 0.05) / 7.0;
        assert!(
            (estimate - expected).abs() < 1e-9,
            "estimate was {estimate}"
        );
    }

    #[test]
    fn estimate_cost_peak_selects_price_by_peak_flag() {
        let cost = builtin_model_cost("opencode-go", "deepseek-v4-flash").expect("price");
        let input = 1_000_000u64;
        let output = 1_000_000u64;
        let cached = 1_000_000u64;
        // 高峰价 = 闲时价的 2 倍（3.0 vs 1.5、9.0 vs 4.5、0.10 vs 0.05，除以同一汇率 7.0），
        // 故高峰估算恰为闲时 2 倍。
        let off = estimate_cost_peak(input, output, cached, &cost, false);
        let peak = estimate_cost_peak(input, output, cached, &cost, true);
        assert!((peak - 2.0 * off).abs() < 1e-12, "off={off} peak={peak}");
    }

    #[test]
    fn estimate_cost_peak_longcat_is_identical_for_peak_and_off_peak() {
        let cost = builtin_model_cost("longcat", "LongCat-2.0").expect("price");
        let peak = estimate_cost_peak(2_000, 1_000, 0, &cost, true);
        let off = estimate_cost_peak(2_000, 1_000, 0, &cost, false);
        // longcat 峰价=闲价，两分支同实现路径应得相同值。
        assert!((peak - off).abs() < 1e-12);
        // 与既有单价的线性结果一致，行为不变。
        let expected = (2_000.0 * 0.75 + 1_000.0 * 2.95) / 1e6;
        assert!((peak - expected).abs() < 1e-12);
        // 无 token 时为零。
        assert_eq!(estimate_cost(0, 0, 0, &cost), 0.0);
    }

    /// 构造距 UNIX_EPOCH 若干整数秒的 SystemTime，使其北京小时（=`(raw_hour+8) mod 24`）
    /// 恰为 `beijing_hour`。`beijing_hour` 在 0..24，久远参照无跨日，映射确定。
    fn bj_hour(beijing_hour: u64) -> std::time::SystemTime {
        let raw_hour = (beijing_hour + 24 - 8) % 24;
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(raw_hour * 3600)
    }

    #[test]
    fn is_peak_hour_utc8_detects_peak_windows() {
        for h in [9u64, 10, 11] {
            assert!(is_peak_hour_utc8(bj_hour(h)), "9:00-12:00 应高峰，hour={h}");
        }
        for h in [14u64, 15, 16, 17] {
            assert!(
                is_peak_hour_utc8(bj_hour(h)),
                "14:00-18:00 应高峰，hour={h}"
            );
        }
    }

    #[test]
    fn is_peak_hour_utc8_non_peak_boundaries() {
        // 12:00 起、18:00 起，及其余小时均非高峰。
        for h in [0u64, 1, 8, 12, 13, 18, 19, 23] {
            assert!(!is_peak_hour_utc8(bj_hour(h)), "hour={h} 应非高峰");
        }
    }
}
