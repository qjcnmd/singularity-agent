//! --json 渲染：逐事件 JSONL 行 + 终态 summary 行。
//!
//! 事件行的 {"method", "params"} 信封就是 TurnEvent 自身的 tagged serde
//! 形状，此处直接编码写入，不再构造中间 JSON 树；终态行的形状由 protocol
//! 单点拥有（TerminalSummary）。本模块只做写入：thread 未解析时 summary
//! 省略 thread 事实，不写入伪造的哨兵值。

use std::io::Write;

use serde::Serialize;
use singularity_protocol::{TerminalSummary, TurnEvent, TurnModelUsage, TurnStatus};

pub struct JsonlRenderer {
    out: Box<dyn Write>,
    thread_id: Option<String>,
    /// 第一条 stdout 写入故障。存在时跳过后续事件行；终态 summary 仍会
    /// 尝试写出，但最终结果必须将该故障报告给调用方。
    output_error: Option<String>,
}

impl JsonlRenderer {
    /// 生产构造：事件行与 summary 写真实 stdout。thread_id 为 None 时
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

    /// 输出一行事件；事件自身的 tagged serde 形状就是 JSONL 信封，直接
    /// 编码写入。stdout 写失败置位 broken 标志（后续事件行跳过），终态行
    /// 写失败由调用方显性处理。投影失败不改变执行事实。
    pub fn on_event(&mut self, event: &TurnEvent) {
        if self.output_error.is_some() {
            return;
        }
        self.write_line(event);
    }

    /// 终态 summary 行。形状由 protocol 的 TerminalSummary 单点定义，
    /// 本方法只做 stdout 写入：usage 仅在已知时输出；truncated 为 true 时
    /// 额外输出 turn.truncated: true（仅截断终态出现）；thread 未解析时
    /// 省略 summary.thread 与 turn.threadId，不写伪造哨兵值。
    /// stdout 写失败记录到 Self::output_failure，调用方读取它并以失败退出
    /// （ProcessOutcome::Failed，错误文本带上底层写失败原因）。
    /// 已记录的事件写入故障不阻止 summary 尝试——机器解析方仍有机会拿到终态行。
    pub fn emit_summary(
        &mut self,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) {
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        self.write_line(&summary.to_line());
    }

    /// 编码并写完一整行：整行一次 write_all 提交，任何失败都不向输出流残留
    /// 半行破损数据。行不可编码（值无法表示为 JSON）与 stdout 写失败同样
    /// 属于输出故障，错误文本保留底层原因。
    fn write_line(&mut self, line: &impl Serialize) {
        let encoded = serde_json::to_vec(line).map(|mut bytes| {
            bytes.push(b'\n');
            bytes
        });
        let written = encoded
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                self.out
                    .write_all(&bytes)
                    .map_err(|error| error.to_string())
            })
            .and_then(|()| self.out.flush().map_err(|error| error.to_string()));
        if let Err(error) = written {
            self.output_error.get_or_insert(error);
        }
    }

    /// 返回此渲染器观察到的第一个输出故障（行编码或 stdout 写入）。
    pub fn output_failure(&self) -> Option<&str> {
        self.output_error.as_deref()
    }
}
