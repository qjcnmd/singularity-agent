use crate::{ModelErrorCategory, ModelUsage, ProviderApiProtocol};
pub use singularity_protocol::ProviderAttemptStatus;

/// 面向 AgentLoop 边界的规范化、安全的 provider 流数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 模型响应的可见文本增量。
    OutputTextDelta { delta: String },
    /// 提供方公开的思考文本增量：Chat 的 reasoning 增量与 Responses 的
    /// reasoning summary 增量都从这里发射，不含 provider 私有 replay。
    ReasoningTextDelta { delta: String },
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

/// 一次真实 provider HTTP attempt 的终态。终态词形由 protocol 的
/// ProviderAttemptStatus 单点拥有，观测、durable 记录与事件共用同一枚举。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub started: ProviderAttemptStarted,
    pub terminal_status: ProviderAttemptStatus,
    /// 从 attempt 创建到响应解析或失败终结的墙钟时长。
    pub attempt_duration_ms: u64,
    pub error_category: Option<ModelErrorCategory>,
    pub diagnostic_code: Option<String>,
    pub retry_after_ms: Option<u64>,
    /// 本次 attempt 明确上报了用量（usage_present）时才存在；后续 replay 校验
    /// 拒绝该响应时，已上报的用量仍然保留，终态可以是失败。
    pub usage: Option<ModelUsage>,
}
