use serde::{Deserialize, Serialize};

/// 从模型提供方完成中累积的真实令牌与缓存计数器。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 原始 usage 对象是否存在；缺失时各计数保持 unknown 的既有表示，
    /// 不把缺失伪装成零消费或其它可计算金额。
    pub usage_present: bool,
}

impl ModelUsage {
    /// 把另一次完成的真实 usage 聚合进本对象（计数器 saturating add，
    /// `usage_present` 按或合并）。agent 层与压缩引擎共用这一个聚合实现。
    pub fn merge(&mut self, other: &ModelUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.usage_present |= other.usage_present;
    }
}

/// 模型侧请求或响应的校验错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
}
