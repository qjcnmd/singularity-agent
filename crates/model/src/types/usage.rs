use serde::{Deserialize, Serialize};

/// 从模型提供方完成中累积的真实令牌与缓存计数器。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    /// provider 是否明确上报了缓存输入用量，包括零。
    #[serde(default)]
    pub cached_input_tokens_present: bool,
    pub reasoning_tokens: u64,
    /// 输入与输出计数是否都已有效上报：单次解析以两者都存在为真，聚合按或合并。
    /// 原始 usage 对象存在但缺分项时为 false，各计数保持 unknown 的既有表示，
    /// 不把缺失伪装成零消费或其它可计算金额。
    pub usage_present: bool,
}

impl ModelUsage {
    /// 把另一次完成的真实 usage 聚合进本对象（计数器 saturating add，
    /// usage_present 按或合并）。输入是协议解析后的既成 usage，总数已在那里按
    /// 「显式优先、缺失时由已知输入输出补出」定好，聚合与消费者都不再推导。
    /// 生成与摘要请求均由 Agent 请求记账聚合。
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

#[cfg(test)]
mod tests {
    use super::ModelUsage;
    use serde_json::json;

    /// 聚合消费协议解析后的既成 usage：两次只上报输入输出的响应合并后，
    /// 总数与已知输入输出一致，聚合本身不再推导一遍。
    #[test]
    fn merge_aggregates_the_totals_normalized_at_the_protocol_boundary() {
        let parsed = || {
            crate::openai::parse::parse_usage(
                Some(&json!({"prompt_tokens": 10, "completion_tokens": 2})),
                "prompt_tokens",
                "completion_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/completion_tokens_details/reasoning_tokens",
            )
        };
        let mut aggregate = ModelUsage::default();
        aggregate.merge(&parsed());
        assert_eq!(aggregate.total_tokens, 12);

        aggregate.merge(&parsed());
        assert_eq!(aggregate.input_tokens, 20);
        assert_eq!(aggregate.output_tokens, 4);
        assert_eq!(aggregate.total_tokens, 24);
        assert!(aggregate.usage_present);
    }
}
