//! HTTP 请求执行、状态码分类与有界响应体读取。

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Response;
use singularity_core::CancellationToken;

use crate::error::{
    ModelError, ModelErrorKind, ProviderError, ProviderErrorStage, ProviderTransportCategory,
};
use crate::provider::runtime::ProviderRuntime;
use crate::provider::telemetry::ProviderAttemptMetadata;
use crate::transport::retry::PROVIDER_CANCELLATION_POLL_MS;
use crate::types::{ModelTurnResponse, ProviderToolReasoningMode};

pub const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
pub const HTTP_STATUS_FORBIDDEN: u16 = 403;
pub const HTTP_STATUS_NOT_FOUND: u16 = 404;
pub const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
pub const HTTP_STATUS_RATE_LIMITED: u16 = 429;
pub const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
pub const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE: &str = "provider_runtime_initialization_failed";

pub fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub fn model_error_from_http_status(
    status: u16,
    provider_name: &str,
    model_name: &str,
) -> ModelError {
    let kind = match status {
        HTTP_STATUS_UNAUTHORIZED | HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        HTTP_STATUS_REQUEST_TIMEOUT => ModelErrorKind::Timeout,
        HTTP_STATUS_NOT_FOUND => ModelErrorKind::InvalidRequest,
        HTTP_STATUS_RATE_LIMITED => ModelErrorKind::RateLimited,
        status if status >= HTTP_STATUS_INTERNAL_SERVER_ERROR => ModelErrorKind::ProviderOverloaded,
        _ => ModelErrorKind::UnknownProviderError,
    };
    let message = format!("Provider returned HTTP {status}.");
    let mut error = ModelError::new(kind, message)
        .with_provider(provider_name.to_string())
        .with_model(model_name.to_string())
        .with_provider_diagnostic("provider_http_status", ProviderErrorStage::ResponseStatus);
    error.http_status = Some(status);
    error
}

pub fn provider_transport_error(
    error: reqwest::Error,
    code: &'static str,
    stage: ProviderErrorStage,
    request_timeout_seconds: Option<u64>,
) -> ProviderError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    };
    let timeout = error.is_timeout();
    let category = if error.is_timeout() {
        ProviderTransportCategory::Timeout
    } else if error.is_connect() {
        ProviderTransportCategory::Connect
    } else if error.is_request() {
        ProviderTransportCategory::Request
    } else if error.is_body() {
        ProviderTransportCategory::BodyRead
    } else {
        ProviderTransportCategory::Unknown
    };
    let message = format!("provider transport failed: {}", error.without_url());
    let mut model_error = ModelError::new(kind, message).with_provider_diagnostic(code, stage);
    model_error.transport_category = Some(category);
    if timeout {
        model_error.timeout_seconds = request_timeout_seconds;
    }
    ProviderError::from_model_error(model_error)
}

pub fn provider_runtime_error(_error: std::io::Error) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider runtime initialization failed",
        )
        .with_provider_diagnostic(
            PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE,
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

pub fn provider_client_initialization_error(error: reqwest::Error) -> ProviderError {
    provider_transport_error(
        error,
        "provider_client_initialization_failed",
        ProviderErrorStage::ClientInitialization,
        None,
    )
}

pub fn provider_cancelled_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::Cancelled, "provider request cancelled")
            .with_provider_diagnostic("provider_request_cancelled", ProviderErrorStage::Cancelled),
    )
}

pub fn provider_tool_reasoning_history_error(
    response: &ModelTurnResponse,
    mode: ProviderToolReasoningMode,
) -> ProviderError {
    let (code, evidence) = if mode == ProviderToolReasoningMode::DisabledForToolCalls {
        (
            "provider_tool_reasoning_mode_not_honored",
            "tool_reasoning_disable_not_honored",
        )
    } else {
        (
            "provider_tool_reasoning_history_unsupported",
            "tool_reasoning_content_requires_adapter_history_support",
        )
    };
    let mut error = ModelError::new(
        ModelErrorKind::UnsupportedCapability,
        "provider returned tool reasoning that cannot be safely replayed",
    )
    .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(evidence.to_string());
    let provider_error = ProviderError::from_model_error(error);
    if let Some(metadata) = &response.provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata.clone())
    } else {
        provider_error
    }
}

pub fn provider_attempt_metadata(
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> ProviderAttemptMetadata {
    ProviderAttemptMetadata {
        attempt_count: metadata.attempt_count,
        retry_count: metadata.retry_count,
        latency_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        occurrences: metadata.occurrences.clone(),
    }
}

pub fn block_on_provider_future<C, F, T>(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    error_code: &'static str,
    error_stage: ProviderErrorStage,
    request_timeout_seconds: u64,
    create_future: C,
) -> Result<T, ProviderError>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let mut future = match runtime {
        ProviderRuntime::External(handle) => {
            let _runtime_context = handle.enter();
            Box::pin(create_future())
        }
        ProviderRuntime::Owned(runtime) => {
            let _runtime_context = runtime.enter();
            Box::pin(create_future())
        }
    };
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let poll = Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS);
        match runtime.block_on(async { tokio::time::timeout(poll, future.as_mut()).await }) {
            Ok(result) => {
                return result.map_err(|error| {
                    provider_transport_error(
                        error,
                        error_code,
                        error_stage.clone(),
                        Some(request_timeout_seconds),
                    )
                });
            }
            Err(_) => continue,
        }
    }
}

pub fn read_bounded_provider_response_body(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: Response,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_body_too_large_error());
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        )?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_body_too_large_error());
        }
        body.extend_from_slice(&chunk);
    }
}

pub fn provider_response_body_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response body exceeded the fixed safety limit",
    )
    .with_provider_diagnostic(
        "provider_response_body_too_large",
        ProviderErrorStage::ResponseBodyRead,
    );
    error.validation_errors = vec!["provider_response_body_too_large".to_string()];
    ProviderError::from_model_error(error)
}

pub fn provider_response_json_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response was not valid JSON",
    )
    .with_provider_diagnostic(
        "provider_response_json_decode_failed",
        ProviderErrorStage::ResponseJsonDecode,
    )
}
