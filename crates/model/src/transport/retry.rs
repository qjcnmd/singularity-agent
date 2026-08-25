use reqwest::header::HeaderMap;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use crate::MAX_RETRY_AFTER_MS;

pub(crate) fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|milliseconds| Duration::from_millis(milliseconds.min(MAX_RETRY_AFTER_MS)))
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
        return Some(Duration::from_secs(seconds).min(Duration::from_millis(MAX_RETRY_AFTER_MS)));
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
    Some(remaining.min(Duration::from_millis(MAX_RETRY_AFTER_MS)))
}
