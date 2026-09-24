pub mod contract;
pub mod telemetry;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use telemetry::*;

use crate::config::ModelConfigurationSnapshot;
use crate::error::ProviderError;
use crate::types::{ModelTurnRequest, ModelTurnResponse};
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

/// 模型调用失败，或 attempt 记录提交不出去。
/// 记录类错误保留它的 IO 原因，绝不进入 provider 的重试策略。
#[derive(Debug, thiserror::Error)]
pub enum ProviderCallError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("attempt recording failed: {0}")]
    Recording(#[from] std::io::Error),
}

/// 一次模型请求的异步结果；借用本次请求和观察者直到完成。
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelTurnResponse, ProviderCallError>> + Send + 'a>>;

/// 流式事件与 attempt 记录共用的观察边界。记录完成后才能发送请求或返回结果。
pub trait ProviderObserver: Send {
    /// 发布已解码的公开流式增量。
    fn on_stream(&mut self, event: ProviderStreamEvent);

    /// 提交一次请求尝试的开始或结束记录；完成后提供方才能继续。
    fn record_attempt<'a>(
        &'a mut self,
        event: ProviderAttemptEvent,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>>;
}

/// Agent 完成模型请求的提供方边界；流式事件和请求尝试记录走同一个异步入口。
pub trait Provider {
    /// 返回该 provider 当前选择的容量快照（不可变）：只冻结请求前压缩和输出预算消费
    /// 这两项容量；模型身份、声明协议和能力合同不在这个快照里重复。
    fn model_configuration(&self) -> ModelConfigurationSnapshot;

    /// 流式完成一个已校验的请求：按顺序发射规范化的可见文本增量并返回终态。回调只接收
    /// 公开文本和思考文本的增量，不含私有续接数据、原始 payload 或工具参数增量。
    ///
    /// `record_attempt` 是必需的提交边界：Started 紧跟在请求校验之后且必须先成功再发送
    /// 请求，Finished 必须先成功再返回响应；它的 IO 错误直接返回，不重试也不取消。
    fn complete_stream<'a>(
        &'a self,
        request: &'a ModelTurnRequest,
        cancellation: &'a CancellationToken,
        observer: &'a mut dyn ProviderObserver,
    ) -> ProviderFuture<'a>;
}
