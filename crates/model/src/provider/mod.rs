pub(crate) mod contract;
use crate::error::ProviderError;
use crate::provider_streaming_unsupported_error;
use crate::{ModelTurnRequest, ModelTurnResponse, ProviderApiProtocol, ProviderProtocolContract};
use singularity_core::CancellationToken;
use std::sync::Arc;

/// `AgentLoop` 用于能力协商和完成请求的模型提供方边界。
pub trait Provider {
    /// 返回模型提供方声明的基线契约。
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// Report the typed stream capability for the protocol selected by this provider.
    ///
    /// Legacy providers default to unsupported, even if their unrelated protocol metadata uses
    /// the same enum values as the OpenAI-compatible adapter.
    fn streaming_capability(
        &self,
        _selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::Unsupported
    }

    /// Stream normalized visible text when the selected protocol supports it.
    ///
    /// The callback never receives reasoning, raw provider payloads, or tool argument deltas.
    /// Providers without this protocol capability keep using `complete` unchanged.
    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(provider_streaming_unsupported_error())
    }

    /// Stream visible text and expose each underlying HTTP attempt in real time.
    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_stream(request, cancellation, on_event)
    }

    /// 完成一个已校验请求，同时保留取消和类型化模型提供方错误。
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;

    /// Complete a request and expose each underlying HTTP attempt in real time.
    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
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
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
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
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_observed(request, cancellation, on_attempt)
    }
}

pub(crate) mod runtime;
pub(crate) mod telemetry;
pub use telemetry::{
    ProviderAttemptEvent, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent, ProviderStreamingCapability,
};
