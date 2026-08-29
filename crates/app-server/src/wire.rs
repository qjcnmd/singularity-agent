//! 结构映射的单一 wire 收口：runtime 对象 → 协议线格式。
//!
//! Thread 摘要的 wire 投影经此模块；runtime 词形与协议词形同源，
//! provider 状态直接使用协议的 `ProviderConfigurationStatus`，无映射层。

use singularity_protocol::Thread;
use singularity_runtime::ThreadSummary;

/// 把 JSONL 只读摘要投影为协议 Thread。
pub fn thread_from_summary(record: &ThreadSummary) -> Thread {
    Thread {
        thread_id: record.thread_id.clone(),
        model: record.model.clone(),
        cwd: record.cwd.clone(),
        last_turn_status: record.status,
    }
}
