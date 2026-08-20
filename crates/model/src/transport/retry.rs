use reqwest::header::HeaderMap;
use singularity_core::CancellationToken;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::error::{
    ModelError, ModelErrorKind, ProviderError, ProviderErrorStage, ProviderTransportCategory,
};
use crate::provider::runtime::ProviderRuntime;
use crate::{
    HTTP_STATUS_INTERNAL_SERVER_ERROR, HTTP_STATUS_RATE_LIMITED, HTTP_STATUS_REQUEST_TIMEOUT,
    PROVIDER_CANCELLATION_POLL_MS, PROVIDER_RETRY_BASE_BACKOFF_MS, PROVIDER_RETRY_MAX_BACKOFF_MS,
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
    let sample = Uuid::new_v4().as_u128() as u64;
    Duration::from_millis(full_jitter_delay_ms(retry_count, sample))
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
    // HTTP-date's current wire form is IMF-fixdate, e.g.
    // "Wed, 21 Oct 2015 07:28:00 GMT". Invalid or obsolete forms are
    // ignored and fall back to the bounded local exponential delay.
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || !parts[5].eq_ignore_ascii_case("GMT") {
        return None;
    }
    let day = parts[1].parse::<u32>().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts[3].parse::<i64>().ok()?;
    let time = parts[4];
    let mut clock = time.split(':');
    let hour = clock.next()?.parse::<u64>().ok()?;
    let minute = clock.next()?.parse::<u64>().ok()?;
    let second = clock.next()?.parse::<u64>().ok()?;
    if clock.next().is_some() || day == 0 || day > 31 || hour >= 24 || minute >= 60 || second >= 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let unix_seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::try_from(hour * 3_600 + minute * 60 + second).ok()?)?;
    if unix_seconds < 0 {
        return Some(Duration::ZERO);
    }
    let target = UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(unix_seconds).ok()?))?;
    let remaining = target.duration_since(SystemTime::now()).unwrap_or_default();
    Some(remaining.min(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS)))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let year = year.checked_sub(if month <= 2 { 1 } else { 0 })?;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

pub(super) fn wait_provider_backoff(
    runtime: &ProviderRuntime,
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
        let poll = remaining.min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        runtime.block_on(async {
            tokio::time::sleep(poll).await;
        });
    }
}
