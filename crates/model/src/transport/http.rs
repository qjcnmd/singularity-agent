use std::future::Future;
use std::time::Duration;

use reqwest::header::HeaderMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError, provider_error_kind_for_http_status};
use crate::{HTTP_STATUS_CONFLICT, PROVIDER_TIMEOUT_SECONDS};

pub(crate) fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_value)
        })
}

fn parse_retry_after_value(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // 无效或已过期的 HTTP-date 不触发立即重发，交给调用方使用有上限的指数退避。
    let target = OffsetDateTime::parse(value, &Rfc2822).ok()?;
    let remaining = Duration::try_from(target - OffsetDateTime::now_utc()).ok()?;
    (!remaining.is_zero()).then_some(remaining)
}

/// 模型调用与目录查询共用 ring；reqwest 的无默认加密实现模式要求先安装 provider。
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
}

/// 获取全局共享的 reqwest Client 单例（复用连接池与 TLS 会话）。
pub(crate) fn provider_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder()
        // 禁用全局 gzip，SSE 流按到达字节即时解析
        .no_gzip()
        .read_timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
        .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(provider_transport_error)?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

/// 根据非 2xx HTTP 状态码构造 ProviderError。
pub(crate) fn provider_error_from_http_status(status: u16) -> ProviderError {
    let kind = match status {
        HTTP_STATUS_CONFLICT => ModelErrorKind::Temporary,
        _ => provider_error_kind_for_http_status(status),
    };
    let message = format!("Provider returned HTTP {status}.");
    ProviderError::new(kind, message)
}

/// 提取网络传输异常的最内层错误原因；剥离可能含凭据的 URL，并限制最大长度。
pub(crate) fn transport_error_source(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut source: &(dyn std::error::Error + 'static) = &error;
    while let Some(inner) = source.source() {
        source = inner;
    }
    crate::error::bounded_provider_error_diagnostic(&source.to_string())
}

fn provider_transport_error(error: reqwest::Error) -> ProviderError {
    let message = format!("provider transport failed: {}", transport_error_source(error));
    ProviderError::new(ModelErrorKind::Temporary, message)
}

pub(crate) fn provider_cancelled_error() -> ProviderError {
    ProviderError::new(ModelErrorKind::Cancelled, "provider request cancelled")
}

pub(crate) async fn provider_future<F, T>(
    cancellation: &CancellationToken,
    future: F,
) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }
    tokio::select! {
        _ = cancellation.cancelled() => Err(provider_cancelled_error()),
        result = future => result
            .map_err(provider_transport_error),
    }
}
