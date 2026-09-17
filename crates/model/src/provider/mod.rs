pub mod contract;
pub mod telemetry;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use telemetry::*;

use crate::config::ModelConfigurationSnapshot;
use crate::error::ProviderError;
use crate::types::{ModelTurnRequest, ModelTurnResponse};
use singularity_core::CancellationToken;

/// 模型失败，或无法提交的 attempt 记录。
/// 记录类错误保留其 IO 原因，绝不进入 provider 重试策略。
#[derive(Debug, thiserror::Error)]
pub enum ProviderCallError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("attempt recording failed: {0}")]
    Recording(#[from] std::io::Error),
}

/// AgentLoop 用于完成请求的模型提供方边界。
///
/// 唯一入口是流式完成；不需要增量投影的调用方使用空回调消费同一入口。
pub trait Provider {
    /// 返回该 provider 当前选择的不可变模型配置快照：provider、model、
    /// reasoning 变体、声明协议、能力合同与重试策略一次冻结。
    fn model_configuration(&self) -> ModelConfigurationSnapshot;

    /// 流式完成一个已校验请求：按序发射规范化可见文本增量，并返回终态。
    ///
    /// 回调只接收公开文本与思考文本增量，不包含私有续接数据、原始 payload 或工具参数增量。
    /// `record_attempt` 是必需的提交边界：Started 紧随请求校验，
    /// 必须先成功再发送；Finished 必须先成功再返回响应。
    /// 其 IO 错误直接返回，不重试也不取消。
    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, ProviderCallError>;
}
