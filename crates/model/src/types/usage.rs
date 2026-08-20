use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 从模型提供方完成中累积的真实令牌与缓存计数器。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 原始 usage 对象是否存在；缺失时各计数保持 unknown 的既有表示，
    /// 不把缺失伪装成零消费或其它可计算金额。
    #[serde(default = "default_usage_present")]
    pub usage_present: bool,
}

/// 旧版序列化数据无 `usage_present` 字段时按"存在"解释（保持历史语义）。
fn default_usage_present() -> bool {
    true
}

/// 模型侧请求或响应的校验错误和非致命警告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}
