pub(crate) mod attempt;
pub mod contract;
pub mod runtime;
pub mod telemetry;

pub use contract::*;
pub use telemetry::*;

use crate::error::ProviderError;
use crate::types::{ModelTurnRequest, ModelTurnResponse};
use singularity_core::CancellationToken;
use std::sync::Arc;

/// `AgentLoop` 用于能力协商和完成请求的模型提供方边界。
pub trait Provider {
    /// 返回模型提供方声明的基线契约。
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// 报告本 provider 所选协议的规范化流式能力。
    ///
    /// Legacy provider 默认不支持流式，即使其无关的协议元数据与
    /// OpenAI 兼容适配器使用相同的枚举值。
    fn streaming_capability(
        &self,
        _selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::Unsupported
    }

    /// 当所选协议支持时，流式输出规范化后的可见文本。
    ///
    /// 回调绝不接收 reasoning、原始 provider payload 或工具参数增量。
    /// 无此协议能力的 provider 保持使用 `complete` 不变。
    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(provider_streaming_unsupported_error())
    }

    /// 流式输出可见文本，并实时暴露每一次底层 HTTP attempt。
    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_stream(request, cancellation, on_event)
    }

    /// 完成一个已校验请求，同时保留取消和类型化模型提供方错误。
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;

    /// 完成一个请求，并实时暴露每一次底层 HTTP attempt。
    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete(request, cancellation)
    }
}

/// 允许 `Arc<dyn Provider>` 作为透明代理，使测试可以注入动态 provider。
impl Provider for Arc<dyn Provider + Send + Sync> {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        (**self).protocol_contract()
    }

    fn streaming_capability(
        &self,
        selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        (**self).streaming_capability(selected_protocol)
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_stream(request, cancellation, on_event)
    }

    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_stream_observed(request, cancellation, on_event, on_attempt)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete(request, cancellation)
    }

    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_observed(request, cancellation, on_attempt)
    }
}
