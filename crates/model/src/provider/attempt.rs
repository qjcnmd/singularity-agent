//! 单次 provider HTTP attempt 的编排观测：计时状态机与终态事件投影。
//!
//! 与 `telemetry.rs` 的事件类型对齐——`ProviderAttemptInProgress` 把一次
//! attempt 的时序观测（发起、响应头、首文本、终态）折叠为
//! `ProviderAttemptEvent::Started` / `Finished` 事件，供上层归因重试。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{ModelError, ModelErrorKind};
use crate::provider::contract::ProviderApiProtocol;
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
};
use crate::types::ModelUsage;

pub(crate) fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// 一次真实 provider HTTP attempt 的可变计时状态。
pub(crate) struct ProviderAttemptInProgress {
    provider_name: String,
    model_name: String,
    actual_api_protocol: ProviderApiProtocol,
    pub(crate) started_at: Instant,
    started_at_unix_ms: u64,
    request_send_to_headers_ms: Option<u64>,
    time_to_first_text_delta_ms: Option<u64>,
}

impl ProviderAttemptInProgress {
    pub(crate) fn new(
        provider_name: &str,
        model_name: &str,
        actual_api_protocol: ProviderApiProtocol,
    ) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            model_name: model_name.to_string(),
            actual_api_protocol,
            started_at: Instant::now(),
            started_at_unix_ms: unix_timestamp_ms(),
            request_send_to_headers_ms: None,
            time_to_first_text_delta_ms: None,
        }
    }

    pub(crate) fn started_event(&self) -> ProviderAttemptEvent {
        ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            actual_api_protocol: self.actual_api_protocol,
            started_at_unix_ms: self.started_at_unix_ms,
        })
    }

    pub(crate) fn mark_response_headers_received(&mut self) {
        self.request_send_to_headers_ms = Some(duration_millis(self.started_at.elapsed()));
    }

    pub(crate) fn set_time_to_first_text_delta(&mut self, duration_ms: Option<u64>) {
        self.time_to_first_text_delta_ms = duration_ms;
    }

    pub(crate) fn finish(
        self,
        error: Option<&ModelError>,
        usage: Option<ModelUsage>,
    ) -> ProviderAttemptOccurrence {
        let terminal_status = match error.map(|error| &error.kind) {
            None => ProviderAttemptStatus::Ok,
            Some(ModelErrorKind::Cancelled) => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        let ended_at_unix_ms = unix_timestamp_ms().max(self.started_at_unix_ms);
        ProviderAttemptOccurrence {
            provider_name: self.provider_name,
            model_name: self.model_name,
            actual_api_protocol: self.actual_api_protocol,
            terminal_status,
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms,
            attempt_duration_ms: duration_millis(self.started_at.elapsed()),
            request_send_to_headers_ms: self.request_send_to_headers_ms,
            time_to_first_text_delta_ms: self.time_to_first_text_delta_ms,
            error_category: error.map(ModelError::category),
            error_stage: error.and_then(|error| error.stage.clone()),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            usage,
        }
    }
}

/// 记录一次终态 attempt，不改变聚合重试语义。
pub(crate) fn emit_provider_attempt_started(
    occurrence: &ProviderAttemptInProgress,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
) {
    on_attempt(occurrence.started_event());
}

pub(crate) fn record_provider_attempt(
    occurrence: ProviderAttemptInProgress,
    error: Option<&ModelError>,
    usage: Option<ModelUsage>,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
) {
    let occurrence = occurrence.finish(error, usage);
    on_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence)));
}
