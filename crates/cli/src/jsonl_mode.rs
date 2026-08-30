//! `--json` 渲染：逐事件 JSONL 行 + 终态 `summary` 行。
//!
//! 事件行形状为 `{"method": <稳定方法名>, "params": <typed payload>}`；
//! 终态行固定为 `{"summary":{"thread":…,"turn":…}}`，turn 携带
//! `status`/`threadId`/`usage`，截断终态额外携带 `truncated: true`，供外部
//! 评估器等机器解析方消费。thread 未解析时整个省略 `summary.thread` 与
//! `turn.threadId` 键，不写入伪造的哨兵值。

use std::io::Write;

use serde_json::{Value, json};
use singularity_runtime::events::{TurnEvent, turn_event_params};
use singularity_runtime::objects::TurnStatus;

pub struct JsonlRenderer {
    thread_id: Option<String>,
    /// stdout 已写入失败：置位后跳过后续事件行，避免每行重试；终态
    /// summary 仍会尝试写出并把真实结果报告给调用方。
    stdout_broken: bool,
}

impl JsonlRenderer {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: Some(thread_id.into()),
            stdout_broken: false,
        }
    }

    /// thread 尚未解析的渲染器：终态 summary 省略 thread 事实。
    pub fn without_thread() -> Self {
        Self {
            thread_id: None,
            stdout_broken: false,
        }
    }

    /// 输出一行事件；投影是恒不失败的纯构造，stdout 写失败置位 broken
    /// 标志（后续事件行跳过），终态行写失败由调用方显性处理。
    pub fn emit(&mut self, event: &TurnEvent) {
        if self.stdout_broken {
            return;
        }
        let line = json!({"method": event.method(), "params": turn_event_params(event)});
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if writeln!(lock, "{line}")
            .and_then(|()| lock.flush())
            .is_err()
        {
            self.stdout_broken = true;
        }
    }

    /// 终态 summary 行。usage 仅在已知时输出；`truncated` 为 true 时额外
    /// 输出 `turn.truncated: true`（仅截断终态出现，加法兼容）。
    /// stdout 写失败以 `Err` 返回，调用方据此以非零退出码收敛。
    pub fn emit_summary(
        &self,
        status: TurnStatus,
        usage: Option<Value>,
        truncated: bool,
    ) -> std::io::Result<()> {
        let mut turn = json!({
            "status": status,
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
        writeln!(lock, "{line}")?;
        lock.flush()
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
