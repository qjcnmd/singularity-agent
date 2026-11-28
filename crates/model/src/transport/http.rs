use std::future::Future;
use std::time::Duration;

use reqwest::header::HeaderMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use tokio_util::sync::CancellationToken;

use crate::PROVIDER_TIMEOUT_SECONDS;
use crate::error::{ModelErrorKind, ProviderError};

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
    let target = OffsetDateTime::parse(value, &Rfc2822).ok()?;
    let remaining = Duration::try_from(target - OffsetDateTime::now_utc()).ok()?;
    (!remaining.is_zero()).then_some(remaining)
}

/// 模型调用、目录查询与联网工具共用的基础 HTTP 客户端构造器：
/// 安装 ring TLS provider、禁用重定向并统一 User-Agent。
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        // reqwest 跨源重定向时只移除预定义的鉴权头。
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(app_core::identity::USER_AGENT)
}

const PROVIDER_CONNECT_TIMEOUT_SECONDS: u64 = 30;

pub(crate) fn provider_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder()
        .no_gzip()
        .connect_timeout(Duration::from_secs(PROVIDER_CONNECT_TIMEOUT_SECONDS))
        .read_timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
        .build()
        .map_err(provider_transport_error)?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

pub(crate) fn apply_provider_auth(
    mut request: reqwest::RequestBuilder,
    protocol: protocol::ProviderApiProtocol,
    api_key: &str,
) -> Result<reqwest::RequestBuilder, ProviderError> {
    match protocol {
        protocol::ProviderApiProtocol::Messages => {
            request = request.header("anthropic-version", crate::ANTHROPIC_VERSION);
            if !api_key.is_empty() {
                request = request.header("x-api-key", sensitive_header_value(api_key)?);
            }
        }
        protocol::ProviderApiProtocol::Responses => {
            if !api_key.is_empty() {
                request = request.bearer_auth(api_key);
            }
        }
    }
    Ok(request)
}

fn sensitive_header_value(value: &str) -> Result<reqwest::header::HeaderValue, ProviderError> {
    let mut header = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| crate::config::configuration_error("模型配置无效：api_key 只能包含可见 ASCII 字符"))?;
    header.set_sensitive(true);
    Ok(header)
}

pub(crate) fn transport_error_source(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut source: &(dyn std::error::Error + 'static) = &error;
    while let Some(inner) = source.source() {
        source = inner;
    }
    crate::error::bounded_provider_error_diagnostic(&source.to_string())
}

pub(crate) const MAX_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;

pub(crate) enum BoundedBodyReadError {
    Cancelled,
    Transport(reqwest::Error),
}

pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<Vec<u8>, BoundedBodyReadError> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        let chunk = match cancellation {
            Some(token) => {
                if token.is_cancelled() {
                    return Err(BoundedBodyReadError::Cancelled);
                }
                tokio::select! {
                    biased;
                    () = token.cancelled() => return Err(BoundedBodyReadError::Cancelled),
                    result = response.chunk() => result.map_err(BoundedBodyReadError::Transport)?,
                }
            }
            None => response.chunk().await.map_err(BoundedBodyReadError::Transport)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = max_bytes - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(body)
}

pub(crate) fn provider_transport_error(error: reqwest::Error) -> ProviderError {
    let kind = if error.is_builder() {
        ModelErrorKind::Other
    } else {
        ModelErrorKind::Temporary
    };
    let message = format!("provider transport failed: {}", transport_error_source(error));
    ProviderError::new(kind, message)
}

pub(crate) fn provider_cancelled_error() -> ProviderError {
    ProviderError::new(ModelErrorKind::Cancelled, "provider request cancelled")
}

pub(crate) async fn provider_future_with<F, T>(
    cancellation: &CancellationToken,
    future: F,
    map_error: impl FnOnce(reqwest::Error) -> ProviderError,
) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(provider_cancelled_error()),
        result = future => result.map_err(map_error),
    }
}

pub(crate) async fn provider_future<F, T>(
    cancellation: &CancellationToken,
    future: F,
) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    provider_future_with(cancellation, future, provider_transport_error).await
}
