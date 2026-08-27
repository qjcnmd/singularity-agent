//! 结构映射的单一 wire 收口：runtime 对象 → 协议线格式。
//!
//! Thread/ThreadStatus、usage 与 provider 状态的投影全部经此模块；runtime
//! 词形与协议词形的对应关系只有一份事实源。

use singularity_protocol::{ProviderConfigurationStatus, Thread, ThreadStatus, TurnModelUsage};
use singularity_runtime::objects::{ProviderStatus, Thread as RuntimeThread, TurnUsage};
use singularity_runtime::store::ThreadSummary;

/// 把 JSONL 只读摘要投影为协议 Thread。
pub fn thread_from_summary(record: &ThreadSummary) -> Thread {
    Thread {
        thread_id: record.thread_id.clone(),
        model: record.model.clone(),
        cwd: record.cwd.clone(),
        last_turn_status: record.status.map(protocol_thread_status),
    }
}

/// 把 runtime 的进程内 Thread 投影为协议 Thread（事件投影路径）。
pub fn thread_from_object(thread: &RuntimeThread) -> Thread {
    Thread {
        thread_id: thread.thread_id.clone(),
        model: thread.model.clone(),
        cwd: thread.cwd.clone(),
        last_turn_status: thread.last_turn_status.map(protocol_thread_status),
    }
}

/// runtime 持久状态 → 协议展示状态的唯一词形映射。
fn protocol_thread_status(status: singularity_runtime::ThreadStatus) -> ThreadStatus {
    match status {
        singularity_runtime::ThreadStatus::Active => ThreadStatus::Active,
        singularity_runtime::ThreadStatus::Completed => ThreadStatus::Completed,
        singularity_runtime::ThreadStatus::Failed => ThreadStatus::Failed,
        singularity_runtime::ThreadStatus::Interrupted => ThreadStatus::Interrupted,
    }
}

/// 把聚合 turn usage 直配为协议线格式（与 runtime 的 `TurnUsage` 字段
/// 一一对应；completeness 语义不变）。
pub fn turn_model_usage(usage: &TurnUsage) -> TurnModelUsage {
    TurnModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_present: usage.usage_present,
        usage_complete: usage.usage_complete,
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
