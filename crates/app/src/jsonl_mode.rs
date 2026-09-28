//! --json 渲染：每条事件写一行 JSONL，最后再写一行终态 summary；事件行的
//! {"method", "params"} 信封就是 TurnEvent 自己的 tagged serde 形状，直接编码写出，
//! 不拼中间 JSON 树，终态行的形状只由 protocol 的 TerminalSummary 定义。
//!
//! 输出故障只算投影层的问题：stdout 写失败后，通道已无法确认行边界（write_all 不保证
//! 原子，可能留下半行），所以连 summary 一起停止写入；执行事实由调用方照常落盘，输出
//! 故障另外经 stderr 和退出码报告。

use std::io::Write;

use serde::Serialize;
use singularity_protocol::{TerminalSummary, TurnEvent, TurnModelUsage, TurnStatus};

pub struct JsonlRenderer {
    out: Box<dyn Write + Send>,
    thread_id: Option<String>,
    /// 一旦有值，后续事件行全部跳过。
    output_error: Option<String>,
}

impl JsonlRenderer {
    /// 生产构造：事件行和 summary 写真实 stdout。
    pub fn stdout(thread_id: Option<String>) -> Self {
        Self::with_writer(thread_id, std::io::stdout())
    }

    /// 测试注入：事件和 summary 写进指定的 sink，用来确定性地验证输出失败路径。
    pub fn with_writer(thread_id: Option<String>, out: impl Write + Send + 'static) -> Self {
        Self {
            out: Box::new(out),
            thread_id,
            output_error: None,
        }
    }

    /// 输出一行事件；输出故障记进 Self::output_failure，由调用方汇报。
    pub fn on_event(&mut self, event: &TurnEvent) {
        self.write_line(event);
    }

    /// 终态 summary 行：usage 已知才写，truncated 时多写一条 turn.truncated: true
    /// （只有截断终态才会出现）；thread 没解析出来时，turn.threadId
    /// 省略，不塞假值顶上。已发生 stdout 写故障时不再补写：残留半行会把两条 JSON
    /// 接到同一行；失败原因记在 Self::output_failure 里，调用方读它并按失败退出
    /// （ProcessOutcome::Failed，错误文本带上底层写失败原因）。
    pub fn emit_summary(
        &mut self,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) {
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        self.write_line(&summary.to_line());
    }

    /// 编码并写完一整行；写入失败保留底层原因。
    fn write_line(&mut self, line: &impl Serialize) {
        if self.output_error.is_some() {
            return;
        }
        let mut bytes =
            serde_json::to_vec(line).expect("CLI protocol values are JSON serializable");
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
