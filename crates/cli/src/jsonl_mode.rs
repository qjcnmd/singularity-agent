//! `--json` 渲染：逐事件 JSONL 行 + 终态 `summary` 行。
//!
//! 事件行的 `{"method", "params"}` envelope 与终态行的形状都由 protocol
//! 单点拥有（`turn_event_envelope`、`TerminalSummary`）；本模块只做写入：
//! thread 未解析时 summary 省略 thread 事实，不写入伪造的哨兵值。

use std::io::Write;

use singularity_runtime::events::{TurnEvent, turn_event_envelope};
use singularity_runtime::objects::{TerminalSummary, TurnModelUsage, TurnStatus};

pub struct JsonlRenderer {
    out: Box<dyn Write>,
    thread_id: Option<String>,
    /// stdout 已写入失败：置位后跳过后续事件行，避免每行重试；终态
    /// summary 仍会尝试写出，但最终结果必须将该输出故障报告给调用方。
    stdout_broken: bool,
    output_error: Option<String>,
}

impl JsonlRenderer {
    /// 生产构造：事件行与 summary 写真实 stdout。
    pub fn stdout(thread_id: Option<String>) -> Self {
        Self::with_writer(thread_id, std::io::stdout())
    }

    /// thread 已知（会话准备已完成）的构造。
    pub fn with_thread(thread_id: &str) -> Self {
        Self::stdout(Some(thread_id.to_string()))
    }

    /// thread 尚未解析的构造：终态 summary 省略 thread 事实。
    pub fn without_thread() -> Self {
        Self {
            out: Box::new(std::io::stdout()),
            thread_id: None,
            stdout_broken: false,
            output_error: None,
        }
    }

    /// 测试注入：事件与 summary 写入指定 sink（输出失败路径的确定性验证）。
    pub fn with_writer(thread_id: Option<String>, out: impl Write + 'static) -> Self {
        Self {
            out: Box::new(out),
            thread_id,
            stdout_broken: false,
            output_error: None,
        }
    }

    /// 输出一行事件；envelope 投影是 protocol 的纯构造，恒不失败。stdout
    /// 写失败置位 broken 标志（后续事件行跳过），终态行写失败由调用方
    /// 显性处理。投影失败不改变执行事实。
    pub fn on_event(&mut self, event: &TurnEvent) {
        if self.stdout_broken {
            return;
        }
        let line = turn_event_envelope(event);
        if writeln!(self.out, "{line}")
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
            .is_err()
        {
            self.stdout_broken = true;
            if self.output_error.is_none() {
                self.output_error = Some("failed to write JSON event to stdout".to_string());
            }
        }
    }

    /// 终态 summary 行。形状由 protocol 的 [`TerminalSummary`] 单点定义，
    /// 本方法只做 stdout 写入：usage 仅在已知时输出；`truncated` 为 true 时
    /// 额外输出 `turn.truncated: true`（仅截断终态出现）；thread 未解析时
    /// 省略 `summary.thread` 与 `turn.threadId`，不写伪造哨兵值。
    /// stdout 写失败以 `Err` 返回，调用方据此以 Output 类别收敛。
    /// broken 标志不阻止 summary 尝试——机器解析方仍有机会拿到终态行。
    pub fn emit_summary(
        &mut self,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) -> Result<(), String> {
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        let line = summary.to_line();
        let result = writeln!(self.out, "{line}")
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string());
        if let Err(error) = &result {
            self.stdout_broken = true;
            if self.output_error.is_none() {
                self.output_error = Some(error.clone());
            }
        }
        result
    }

    /// Returns the first output-channel failure observed by this renderer.
    pub fn output_failure(&self) -> Option<&str> {
        self.output_error.as_deref()
    }
}
