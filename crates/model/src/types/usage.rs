use serde::{Deserialize, Serialize};

/// 从提供方返回的完成结果里累积的真实 token 数与缓存计数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    /// 提供方是否明确上报过缓存输入用量（上报 0 也算上报）。
    #[serde(default)]
    pub cached_input_tokens_present: bool,
    pub reasoning_tokens: u64,
    /// 输入与输出计数是否都有效上报：单次解析要求两者都有，聚合时任一为真即为真。原始 usage
    /// 对象在、但缺分项时为 false；各计数仍按「未知」表示，不把缺失当成零消费或其它可以算钱的数字。
    pub usage_present: bool,
}

impl ModelUsage {
    /// 把另一次完成的真实 usage 累加进本对象：计数器加到上限就不再增加，usage_present 与
    /// cached_input_tokens_present 任一为真即为真。传入的已是协议解析后的 usage，总数在那里
    /// 就按「显式值优先、缺失时用已知的输入输出补出」定好，聚合方和消费方都不再推算。
    pub fn merge(&mut self, other: &ModelUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.usage_present |= other.usage_present;
        self.cached_input_tokens_present |= other.cached_input_tokens_present;
    }
}
