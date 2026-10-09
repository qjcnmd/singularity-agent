//! --json 渲染：每条事件写一行 JSONL，最后写终态 summary；事件行直接编码 TurnEvent 的
//! tagged serde 形状，终态行形状只由 protocol 的 TerminalSummary 定义。stdout 写失败后
//! 行边界无法确认，summary 一并停写；执行事实照常落盘，故障经 stderr 和退出码报告。

use std::io::Write;

use serde::Serialize;
use singularity_protocol::{TerminalSummary, TurnEvent, TurnModelUsage, TurnStatus};

pub struct JsonlRenderer {
    out: std::io::Stdout,
    thread_id: Option<String>,
    /// 一旦有值，后续事件行全部跳过。
    output_error: Option<String>,
}

impl JsonlRenderer {
    /// 生产构造：事件行和 summary 写真实 stdout。
    pub fn stdout(thread_id: Option<String>) -> Self {
        Self {
            out: std::io::stdout(),
            thread_id,
            output_error: None,
        }
    }

    /// 输出一行事件；输出故障记进 Self::output_failure，由调用方汇报。
    pub fn on_event(&mut self, event: &TurnEvent) {
        self.write_line(event);
    }

    /// 写入终态 summary；字段的省略与空值规则由协议类型决定。
    /// 已发生输出故障时不再追加，避免与残留的半行拼接。
    pub fn emit_summary(&mut self, status: TurnStatus, usage: Option<TurnModelUsage>, truncated: bool) {
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        self.write_line(&summary.to_line());
    }

    fn write_line(&mut self, line: &impl Serialize) {
        if self.output_error.is_some() {
            return;
        }
        let mut bytes = serde_json::to_vec(line).expect("CLI protocol values are JSON serializable");
        bytes.push(b'\n');
        if let Err(error) = self.out.write_all(&bytes).and_then(|()| self.out.flush()) {
            self.output_error = Some(error.to_string());
        }
    }

    /// 本渲染器遇到的第一条 stdout 输出故障。
    pub fn output_failure(&self) -> Option<&str> {
        self.output_error.as_deref()
    }
}
