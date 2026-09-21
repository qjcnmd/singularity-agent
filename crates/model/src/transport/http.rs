use std::future::Future;
use std::time::Duration;

use reqwest::Response;
use singularity_core::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError, provider_error_kind_for_http_status};
use crate::{HTTP_STATUS_CONFLICT, MAX_PROVIDER_RESPONSE_BODY_BYTES, PROVIDER_TIMEOUT_SECONDS};

/// 进程内唯一的上游 HTTP 客户端：连接池与 TLS 会话跨 turn 复用；配置固定，构造点只此一处。
pub(crate) fn provider_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
        .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| {
            provider_transport_error(error, "provider_client_initialization_failed")
        })?;
    // 并发构造时保留先到的那个：两者配置相同，落败的实例直接丢掉。
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// 生成请求返回非 2xx 时的失败：类别取自共同的状态分类，只有本路径特有的两个
/// 例外在这里显式写出——409 冲突按可重试的限流类处理，600 以上按提供方过载。
pub(crate) fn provider_error_from_http_status(status: u16) -> ProviderError {
    let kind = match status {
        HTTP_STATUS_CONFLICT => ModelErrorKind::RateLimited,
        status if status > 599 => ModelErrorKind::ProviderOverloaded,
        _ => provider_error_kind_for_http_status(status),
    };
    let message = format!("Provider returned HTTP {status}.");
    ProviderError::new(kind, message).with_code("provider_http_status")
}

fn provider_transport_error(error: reqwest::Error, code: &'static str) -> ProviderError {
    let kind = crate::error::provider_error_kind_for_transport(&error);
    let message = format!("provider transport failed: {}", error.without_url());
    ProviderError::new(kind, message).with_code(code)
}

pub(crate) fn provider_cancelled_error() -> ProviderError {
    ProviderError::new(ModelErrorKind::Cancelled, "provider request cancelled")
        .with_code("provider_request_cancelled")
}

pub(crate) fn provider_reasoning_history_error(message: &'static str) -> ProviderError {
    ProviderError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_code("provider_reasoning_history_invalid")
}

pub(crate) fn block_on_provider_future<C, F, T>(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    error_code: &'static str,
    create_future: C,
) -> Result<T, ProviderError>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let _runtime_context = runtime.enter();
    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }
    let future = create_future();
    runtime.block_on(async {
        tokio::select! {
            _ = cancellation.cancelled_notified() => Err(provider_cancelled_error()),
            result = future => result
                .map_err(|error| provider_transport_error(error, error_code)),
        }
    })
}

pub(crate) fn read_bounded_provider_response_body(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
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

fn provider_response_body_too_large_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response body exceeded the fixed safety limit",
    )
    .with_code("provider_response_body_too_large")
}
