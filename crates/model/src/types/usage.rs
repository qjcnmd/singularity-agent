/// 从提供方返回的完成结果里累积的真实 token 数与缓存计数。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// None 为未上报；Some(0) 为明确上报零缓存用量。
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: u64,
    /// 输入与输出计数是否都有效上报：单次解析要求两者都有，聚合时任一为真即为真。原始 usage
    /// 对象在、但缺分项时为 false；各计数仍按「未知」表示，不把缺失当成零消费或其它可以算钱的数字。
    pub usage_present: bool,
}

impl ModelUsage {
    /// 累加已上报用量，缓存计数保留“未知”与零的区别。传入的已是协议解析后的 usage，总数在那里
    /// 就按「显式值优先、缺失时用已知的输入输出补出」定好，聚合方和消费方都不再推算。
    pub fn merge(&mut self, other: &ModelUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        if let Some(cached) = other.cached_input_tokens {
            self.cached_input_tokens =
                Some(self.cached_input_tokens.unwrap_or(0).saturating_add(cached));
        }
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.usage_present |= other.usage_present;
    }
}
