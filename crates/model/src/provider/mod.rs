pub(crate) mod attempt;
pub mod contract;
pub mod runtime;
pub mod telemetry;

pub use contract::*;
pub use telemetry::*;

use crate::error::ProviderError;
use crate::types::{ModelTurnRequest, ModelTurnResponse};
use singularity_core::CancellationToken;

/// `AgentLoop` 用于完成请求的模型提供方边界。
///
/// 唯一入口是流式完成（一切模型调用走流）；`complete`
/// 是「流 + 排空」的便捷封装，供不需要增量投影的调用方（如压缩）使用。
pub trait Provider {
    /// 返回模型提供方声明的基线契约。
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// 流式完成一个已校验请求：按序发射规范化可见文本增量，并返回终态。
    ///
    /// 回调绝不接收 reasoning、原始 provider payload 或工具参数增量。
    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError>;

    /// 完成一个已校验请求（不消费增量事件），保留取消和类型化模型提供方错误。
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_stream(request, cancellation, &mut |_| {}, on_attempt)
    }
}
