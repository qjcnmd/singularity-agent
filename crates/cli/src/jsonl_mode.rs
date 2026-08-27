//! `--json` 渲染：逐事件 JSONL 行 + 终态 `summary` 行。
//!
//! 事件行形状为 `{"method": <稳定方法名>, "params": <typed payload>}`；
//! 终态行固定为 `{"summary":{"thread":…,"turn":…}}`，turn 携带
//! `status`/`threadId`/`usage`，截断终态额外携带 `truncated: true`，供外部
//! 评估器等机器解析方消费。thread 未解析时整个省略 `summary.thread` 与
//! `turn.threadId` 键，不写入伪造的哨兵值。

use std::io::Write;

use serde_json::{Value, json};
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::TurnStatus;

pub struct JsonlRenderer {
    thread_id: Option<String>,
}

impl JsonlRenderer {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: Some(thread_id.into()),
        }
    }

    /// thread 尚未解析的渲染器：终态 summary 省略 thread 事实。
    pub fn without_thread() -> Self {
        Self { thread_id: None }
    }

    /// 输出一行事件；序列化失败只丢弃该投影，不影响执行。
    pub fn emit(&mut self, event: &TurnEvent) {
        let mut params = match serde_json::to_value(event) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => return,
        };
        if let Some(object) = params.as_object_mut() {
            object.remove("event");
        }
        let line = json!({"method": event.method(), "params": params});
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{line}");
        let _ = lock.flush();
    }

    /// 终态 summary 行。usage 仅在已知时输出；`truncated` 为 true 时额外
    /// 输出 `turn.truncated: true`（仅截断终态出现，加法兼容）。
    pub fn emit_summary(&self, status: TurnStatus, usage: Option<Value>, truncated: bool) {
        let mut turn = json!({
            "status": status.as_str(),
            "usage": usage,
        });
        if let Some(thread_id) = &self.thread_id {
            turn["threadId"] = json!(thread_id);
        }
        if truncated {
            turn["truncated"] = Value::Bool(true);
        }
        let mut summary = json!({
            "turn": turn,
        });
        if let Some(thread_id) = &self.thread_id {
            summary["thread"] = json!({"threadId": thread_id});
        }
        let line = json!({ "summary": summary });
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{line}");
        let _ = lock.flush();
    }
}

/// 进程退出码：completed=0、interrupted=130、failed/其他=1。
pub fn exit_code_for(status: TurnStatus) -> i32 {
    match status {
        TurnStatus::Completed => 0,
        TurnStatus::Interrupted => 130,
        TurnStatus::Running | TurnStatus::Failed => 1,
    }
}
