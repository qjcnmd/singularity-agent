pub mod contract;
pub mod telemetry;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use telemetry::*;

use crate::config::ModelConfigurationSnapshot;
use crate::error::ProviderError;
use crate::types::{ModelTurnRequest, ModelTurnResponse};
use singularity_core::CancellationToken;

/// 模型调用失败，或 attempt 记录提交不出去。
/// 记录类错误保留它的 IO 原因，绝不进入 provider 的重试策略。
#[derive(Debug, thiserror::Error)]
pub enum ProviderCallError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("attempt recording failed: {0}")]
    Recording(#[from] std::io::Error),
}

/// AgentLoop 用来完成请求的模型提供方边界：唯一入口就是流式完成，不需要增量输出的
/// 调用方传空回调，走同一个入口。
pub trait Provider {
    /// 返回该 provider 当前选择的容量快照（不可变）：只冻结请求前压缩和输出预算消费
    /// 这两项容量；模型身份、声明协议和能力合同不在这个快照里重复。
    fn model_configuration(&self) -> ModelConfigurationSnapshot;

    /// 流式完成一个已校验的请求：按顺序发射规范化的可见文本增量并返回终态。回调只接收
    /// 公开文本和思考文本的增量，不含私有续接数据、原始 payload 或工具参数增量。
    ///
    /// `record_attempt` 是必需的提交边界：Started 紧跟在请求校验之后且必须先成功再发送
    /// 请求，Finished 必须先成功再返回响应；它的 IO 错误直接返回，不重试也不取消。
    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, ProviderCallError>;
}
