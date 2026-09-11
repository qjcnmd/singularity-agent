//! 单次 provider HTTP attempt 的编排观测：计时状态机与终态事件投影。
//!
//! 与 telemetry.rs 的事件类型对齐——ProviderAttemptInProgress 保存一次
//! HTTP 请求的开始时间，并在结束时投影 Started / Finished 事件与总耗时。
//! 重试序号由发起重试的 Agent 账本绑定，不属于 Provider。

use std::time::{Duration, Instant};

use crate::error::{ModelErrorKind, ProviderError};
use crate::provider::contract::ProviderApiProtocol;
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
};
use crate::types::ModelUsage;

/// Duration 的毫秒投影：饱和到 u64，全仓时长→毫秒换算唯一实现。
pub fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// 一次真实 provider HTTP attempt 的可变计时状态。
pub(crate) struct ProviderAttemptInProgress {
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
            provider_name: provider_name.to_string(),
            model_name: model_name.to_string(),
            actual_api_protocol,
            started_at: Instant::now(),
        }
    }

    pub(crate) fn started_event(&self) -> ProviderAttemptEvent {
        ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            actual_api_protocol: self.actual_api_protocol,
        })
    }

    pub(crate) fn finish(
        self,
        error: Option<&ProviderError>,
        usage: Option<ModelUsage>,
        retry_after_ms: Option<u64>,
    ) -> ProviderAttemptOccurrence {
        let terminal_status = match error.map(|error| &error.kind) {
            None => ProviderAttemptStatus::Ok,
            Some(ModelErrorKind::Cancelled) => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        ProviderAttemptOccurrence {
            provider_name: self.provider_name,
            model_name: self.model_name,
            actual_api_protocol: self.actual_api_protocol,
            terminal_status,
            attempt_duration_ms: duration_millis(self.started_at.elapsed()),
            error_category: error.map(ProviderError::category),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            retry_after_ms,
            retry_after_source: retry_after_ms
                .map(|_| singularity_protocol::RetryAfterSource::ProviderHeader),
            usage,
        }
    }
}

pub(crate) fn record_provider_attempt(
    occurrence: ProviderAttemptInProgress,
    error: Option<&ProviderError>,
    usage: Option<ModelUsage>,
    retry_after_ms: Option<u64>,
    record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let occurrence = occurrence.finish(error, usage, retry_after_ms);
    record_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence)))
}
