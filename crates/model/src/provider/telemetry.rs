use crate::{ModelErrorCategory, ModelUsage, ProviderApiProtocol};
use serde::{Deserialize, Serialize};

/// 面向 `AgentLoop` 边界的规范化、安全的 provider 流数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 来自 Responses `response.output_text.delta` 事件的可见文本增量。
    OutputTextDelta { delta: String },
}

/// 一次真实 provider HTTP attempt 的安全运行时边界事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAttemptEvent {
    /// 在 HTTP 请求发送前立即发射。
    Started(ProviderAttemptStarted),
    /// 同一请求到达终态时发射一次。
    Finished(Box<ProviderAttemptOccurrence>),
}

/// provider HTTP attempt 开始时已知的稳定、非敏感字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptStarted {
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
}

/// 一次真实 provider HTTP attempt 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    /// attempt 产出了有效 provider 响应。
    Ok,
    /// attempt 以非取消错误结束。
    Error,
    /// attempt 因观察到取消而结束。
    Cancelled,
}

/// 一次真实 provider HTTP attempt 的脱敏运行期观测。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    pub terminal_status: ProviderAttemptStatus,
    /// 从 attempt 创建到响应解析或失败终结的墙钟时长。
    pub attempt_duration_ms: u64,
    pub error_category: Option<ModelErrorCategory>,
    pub diagnostic_code: Option<String>,
    /// 成功响应明确提供 usage 时才存在。
    pub usage: Option<ModelUsage>,
}
