//! 结构映射的单一 wire 收口：runtime 对象 → 协议线格式。
//!
//! Thread/ThreadStatus、usage 与 provider 状态的投影全部经此模块；runtime
//! 词形与协议词形的对应关系只有一份事实源。

use singularity_protocol::{ProviderConfigurationStatus, Thread};
use singularity_runtime::ThreadSummary;
use singularity_runtime::objects::ProviderStatus;

/// 把 JSONL 只读摘要投影为协议 Thread。
pub fn thread_from_summary(record: &ThreadSummary) -> Thread {
    Thread {
        thread_id: record.thread_id.clone(),
        model: record.model.clone(),
        cwd: record.cwd.clone(),
        last_turn_status: record.status,
    }
}

/// 把 runtime 的 provider 状态投影为协议 `provider/status` 形状。
pub fn provider_configuration(status: &ProviderStatus) -> ProviderConfigurationStatus {
    ProviderConfigurationStatus {
        source: status.source.clone(),
        snapshot_id: status.snapshot_id.clone(),
        configured: status.configured,
        configuration_blocker: status.configuration_blocker.clone(),
        api_key_present: status.api_key_present,
        base_url_present: status.base_url_present,
        model_present: status.model_present,
    }
}
