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
    /// 第一条 stdout 写入故障。存在时跳过后续事件行；终态 summary 仍会
    /// 尝试写出，但最终结果必须将该故障报告给调用方。
    output_error: Option<String>,
}

impl JsonlRenderer {
    /// 生产构造：事件行与 summary 写真实 stdout。`thread_id` 为 `None` 时
    /// 终态 summary 省略 thread 事实，不写伪造哨兵值。
    pub fn stdout(thread_id: Option<String>) -> Self {
        Self::with_writer(thread_id, std::io::stdout())
    }

    /// 测试注入：事件与 summary 写入指定 sink（输出失败路径的确定性验证）。
    pub fn with_writer(thread_id: Option<String>, out: impl Write + 'static) -> Self {
        Self {
            out: Box::new(out),
            thread_id,
            output_error: None,
        }
    }

    /// 输出一行事件；envelope 投影是 protocol 的纯构造，恒不失败。stdout
    /// 写失败置位 broken 标志（后续事件行跳过），终态行写失败由调用方
    /// 显性处理。投影失败不改变执行事实。
    pub fn on_event(&mut self, event: &TurnEvent) {
        if self.output_error.is_some() {
            return;
        }
        let line = turn_event_envelope(event);
        if writeln!(self.out, "{line}")
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
            .is_err()
        {
            self.output_error
                .get_or_insert_with(|| "failed to write JSON event to stdout".to_string());
        }
    }

    /// 终态 summary 行。形状由 protocol 的 [`TerminalSummary`] 单点定义，
    /// 本方法只做 stdout 写入：usage 仅在已知时输出；`truncated` 为 true 时
    /// 额外输出 `turn.truncated: true`（仅截断终态出现）；thread 未解析时
    /// 省略 `summary.thread` 与 `turn.threadId`，不写伪造哨兵值。
    /// stdout 写失败记录到 [`Self::output_failure`]，调用方据此以 Output 类别收敛。
    /// 已记录的事件写入故障不阻止 summary 尝试——机器解析方仍有机会拿到终态行。
    pub fn emit_summary(
        &mut self,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) {
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        let line = summary.to_line();
        if let Err(error) = writeln!(self.out, "{line}")
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
        {
            self.output_error.get_or_insert(error);
        }
    }

    /// Returns the first output-channel failure observed by this renderer.
    pub fn output_failure(&self) -> Option<&str> {
        self.output_error.as_deref()
    }
}
