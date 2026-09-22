use crate::{ModelErrorCategory, ModelUsage, ProviderApiProtocol};
pub use singularity_protocol::ProviderAttemptStatus;

/// 面向 AgentLoop 边界的 provider 流数据：已规范化，且不含敏感内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 工具调用名称或参数的生成增量，仅用于记录生成开始时间。
    ToolCallDelta,
    /// 模型响应里对外可见的正文增量。
    OutputTextDelta { delta: String },
    /// 提供方公开的思考文本增量：Chat 的 reasoning 增量和 Responses 的
    /// reasoning summary 增量都从这里发出，不含 provider 私有的回放材料。
    ReasoningTextDelta { delta: String },
}

/// 一次真实 provider HTTP attempt 在安全运行时边界上产生的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAttemptEvent {
    /// 在发出 HTTP 请求之前立即发射。
    Started(ProviderAttemptStarted),
    /// 同一次请求到达终态时发射一次。
    Finished(Box<ProviderAttemptOccurrence>),
}

/// provider HTTP attempt 开始时就能确定的稳定、非敏感字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptStarted {
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
}

/// 一次真实 provider HTTP attempt 的终态。终态词形由 protocol 里的
/// ProviderAttemptStatus 单独定义，观测、持久记录和事件共用同一个枚举。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub started: ProviderAttemptStarted,
    pub terminal_status: ProviderAttemptStatus,
    /// 从 attempt 创建到响应解析完成或失败终结的墙钟时长。
    pub attempt_duration_ms: u64,
    /// 首个生成增量到响应解析完成的耗时；未观察到增量时未知。
    pub decode_ms: Option<u64>,
    pub error_category: Option<ModelErrorCategory>,
    pub diagnostic_code: Option<String>,
    pub retry_after_ms: Option<u64>,
    /// 只有本次 attempt 明确上报了用量（usage_present）时才存在；后续 replay 校验
    /// 拒绝该响应时，已上报的用量仍然保留，所以终态可以是失败。
    pub usage: Option<ModelUsage>,
}

impl ProviderAttemptOccurrence {
    pub fn finished(
        started: ProviderAttemptStarted,
        attempt_duration_ms: u64,
        usage: Option<ModelUsage>,
        error: Option<&crate::ProviderError>,
    ) -> Self {
        Self {
            started,
            terminal_status: match error {
                None => ProviderAttemptStatus::Ok,
                Some(error) if error.kind == crate::ModelErrorKind::Cancelled => {
                    ProviderAttemptStatus::Cancelled
                }
                Some(_) => ProviderAttemptStatus::Error,
            },
            attempt_duration_ms,
            decode_ms: None,
            error_category: error.map(crate::ProviderError::category),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            retry_after_ms: error
                .and_then(|error| error.retry_after)
                .map(singularity_core::duration_millis),
            usage,
        }
    }
}
