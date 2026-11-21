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
const PROVIDER_CONNECT_TIMEOUT_SECONDS: u64 = 30;
pub(crate) const PROVIDER_USER_AGENT: &str = app_core::identity::USER_AGENT;

pub(crate) fn provider_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder()
        // 禁用全局 gzip，SSE 流按到达字节即时解析；禁用自动重定向，防止自定义鉴权头随跨源跳转外泄。
        .no_gzip()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(PROVIDER_CONNECT_TIMEOUT_SECONDS))
        .read_timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
        .user_agent(PROVIDER_USER_AGENT)
        .build()
        .map_err(provider_transport_error)?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

pub(crate) fn sensitive_header_value(value: &str) -> Result<reqwest::header::HeaderValue, ProviderError> {
    let mut header = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| crate::config::configuration_error("模型配置无效：api_key 只能包含可见 ASCII 字符"))?;
    header.set_sensitive(true);
    Ok(header)
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

pub(crate) const MAX_HTTP_ERROR_BODY_BYTES: usize = 64 * 1024;

pub(crate) enum BoundedBodyReadError {
    Cancelled,
    Transport(reqwest::Error),
}

/// 读取有大小上限的 HTTP 错误响应正文；超出上限后截断，避免大错误页占满内存。
pub(crate) async fn read_bounded_error_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, BoundedBodyReadError> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        if cancellation.is_cancelled() {
            return Err(BoundedBodyReadError::Cancelled);
        }
        let chunk = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(BoundedBodyReadError::Cancelled),
            result = response.chunk() => result.map_err(BoundedBodyReadError::Transport)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = max_bytes - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(body)
}

/// 读取有大小上限的 HTTP 响应正文（无取消令牌路径）；超出上限后截断。
pub(crate) async fn read_bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, reqwest::Error> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        let Some(chunk) = response.chunk().await? else {
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
        biased;
        () = cancellation.cancelled() => Err(provider_cancelled_error()),
        result = future => result
            .map_err(provider_transport_error),
    }
}
