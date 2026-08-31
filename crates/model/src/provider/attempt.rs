//! 单次 provider HTTP attempt 的编排观测：计时状态机与终态事件投影。
//!
//! 与 `telemetry.rs` 的事件类型对齐——`ProviderAttemptInProgress` 把一次
//! attempt 的时序观测（发起、响应头、首文本、终态）折叠为
//! `ProviderAttemptEvent::Started` / `Finished` 事件，供上层归因重试。

use std::time::{Duration, Instant};

use crate::error::{ModelError, ModelErrorKind};
use crate::provider::contract::ProviderApiProtocol;
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
};
use crate::types::ModelUsage;

pub(crate) fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// 一次真实 provider HTTP attempt 的可变计时状态。
pub(crate) struct ProviderAttemptInProgress {
    attempt: u32,
    provider_name: String,
    model_name: String,
    actual_api_protocol: ProviderApiProtocol,
    pub(crate) started_at: Instant,
}

impl ProviderAttemptInProgress {
    pub(crate) fn new(
        provider_name: &str,
        model_name: &str,
        actual_api_protocol: ProviderApiProtocol,
    ) -> Self {
        Self {
            attempt: 1,
            provider_name: provider_name.to_string(),
            model_name: model_name.to_string(),
            actual_api_protocol,
            started_at: Instant::now(),
        }
    }

    pub(crate) fn started_event(&self) -> ProviderAttemptEvent {
        ProviderAttemptEvent::Started(ProviderAttemptStarted {
            attempt: self.attempt,
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            actual_api_protocol: self.actual_api_protocol,
        })
    }

    pub(crate) fn finish(
        self,
        error: Option<&ModelError>,
        usage: Option<ModelUsage>,
        retry_after_ms: Option<u64>,
    ) -> ProviderAttemptOccurrence {
        let terminal_status = match error.map(|error| &error.kind) {
            None => ProviderAttemptStatus::Ok,
            Some(ModelErrorKind::Cancelled) => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        ProviderAttemptOccurrence {
            attempt: self.attempt,
            provider_name: self.provider_name,
            model_name: self.model_name,
            actual_api_protocol: self.actual_api_protocol,
            terminal_status,
            attempt_duration_ms: duration_millis(self.started_at.elapsed()),
            error_category: error.map(ModelError::category),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            retry_after_ms,
            retry_after_source: retry_after_ms
                .map(|_| singularity_protocol::RetryAfterSource::ProviderHeader),
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
    retry_after_ms: Option<u64>,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
) {
    let occurrence = occurrence.finish(error, usage, retry_after_ms);
    on_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence)));
}
