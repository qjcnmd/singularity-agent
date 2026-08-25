use reqwest::header::HeaderMap;
use singularity_core::CancellationToken;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use crate::error::{
    ModelError, ModelErrorKind, ProviderError, ProviderErrorStage, ProviderTransportCategory,
};
use crate::{
    HTTP_STATUS_INTERNAL_SERVER_ERROR, HTTP_STATUS_RATE_LIMITED, HTTP_STATUS_REQUEST_TIMEOUT,
    PROVIDER_RETRY_BASE_BACKOFF_MS, PROVIDER_RETRY_MAX_BACKOFF_MS,
};

pub(super) fn provider_error_is_retryable(error: &ProviderError) -> bool {
    // 只对网络层快速失败重试（连接失败/中断）。挂起超时不重试：120s 无响应后
    // 重试大概率仍无响应，且 6 次重试 × 120s 会让单个挂起请求拖到 12 分钟。
    matches!(error.error.kind, ModelErrorKind::NetworkError)
        && !matches!(
            error.error.transport_category,
            Some(ProviderTransportCategory::Request)
        )
}

pub(super) fn http_status_is_retryable(status: u16) -> bool {
    // 可重试的 HTTP 状态码条件：408（服务端请求超时）、429（速率限制）及 5xx（服务端错误）。
    // 客户端发起的本地传输超时不重试（快速失败），仅当远端服务明确返回瞬时错误时才执行指数退避重试。
    status == HTTP_STATUS_REQUEST_TIMEOUT
        || status == HTTP_STATUS_RATE_LIMITED
        || status >= HTTP_STATUS_INTERNAL_SERVER_ERROR
}

pub(crate) fn provider_retry_backoff(retry_count: u32) -> Duration {
    // Full jitter avoids synchronized retries across independent provider
    // clients while retaining the bounded exponential window. Retry-After
    // is handled by `record_provider_retry` and remains authoritative.
    Duration::from_millis(full_jitter_delay_ms(retry_count, next_jitter_sample()))
}

/// 退避抖动的轻量随机源：进程内单调序列与时钟种子经 splitmix64 终结器混合。
/// 只用于抖动采样，不需要密码学强度，也不引入专门的随机依赖。
fn next_jitter_sample() -> u64 {
    static JITTER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let clock_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut mixed = JITTER_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(clock_seed | 1);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

pub(crate) fn retry_backoff_window_ms(retry_count: u32) -> u64 {
    let shift = retry_count.saturating_sub(1).min(10);
    PROVIDER_RETRY_BASE_BACKOFF_MS
        .saturating_mul(1_u64 << shift)
        .min(PROVIDER_RETRY_MAX_BACKOFF_MS)
}

pub(crate) fn full_jitter_delay_ms(retry_count: u32, sample: u64) -> u64 {
    // The upper bound is inclusive: [0, min(60s, 500ms * 2^(n-1))].
    let window = retry_backoff_window_ms(retry_count);
    sample % window.saturating_add(1)
}

pub(crate) fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|milliseconds| Duration::from_millis(milliseconds.min(PROVIDER_RETRY_MAX_BACKOFF_MS)))
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_value)
        })
}

pub(crate) fn parse_retry_after_value(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(
            Duration::from_secs(seconds).min(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS)),
        );
    }
    parse_http_date_delay(value)
}

pub(crate) fn parse_http_date_delay(value: &str) -> Option<Duration> {
    // HTTP-date 的现行 wire 形态是 IMF-fixdate（RFC 2822 固定格式，GMT 零区），
    // 交给 `time` crate 的 Rfc2822 解析器处理；无效或过时形态回退到有界本地
    // 指数退避（过期日期绝不产生 0 延迟紧连发）。
    let target = OffsetDateTime::parse(value.trim(), &Rfc2822).ok()?;
    let Ok(remaining) = Duration::try_from(target - OffsetDateTime::now_utc()) else {
        return None;
    };
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.min(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS)))
}

pub(super) fn wait_provider_backoff(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    duration: Duration,
) -> Result<(), ProviderError> {
    let deadline = Instant::now() + duration;
    loop {
        if cancellation.is_cancelled() {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::Cancelled,
                    "model request cancelled by client",
                )
                .with_provider_diagnostic("client_cancelled", ProviderErrorStage::Cancelled),
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        // 睡眠与取消通知竞争：取消事件即时唤醒，不再按固定周期轮询。
        let cancelled = runtime.block_on(async {
            tokio::select! {
                _ = cancellation.cancelled_notified() => true,
                _ = tokio::time::sleep(remaining) => false,
            }
        });
        if cancelled {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::Cancelled,
                    "model request cancelled by client",
                )
                .with_provider_diagnostic("client_cancelled", ProviderErrorStage::Cancelled),
            ));
        }
    }
}
