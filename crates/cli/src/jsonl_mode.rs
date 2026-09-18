//! --json 渲染：逐事件 JSONL 行 + 终态 summary 行。
//!
//! 事件行的 {"method", "params"} 信封就是 TurnEvent 自身的 tagged serde
//! 形状，此处直接编码写入，不再构造中间 JSON 树；终态行的形状由 protocol
//! 单点拥有（TerminalSummary）。本模块只做写入：thread 未解析时 summary
//! 省略 thread 事实，不写入伪造的哨兵值。
//!
//! 输出故障只属于投影层：stdout 写入失败后该通道不再能确认行边界（write_all
//! 不保证原子，可能已留下半行），本模块就此停止后续写入，包括 summary；执行
//! 事实由调用方照常持久化，输出故障另行经 stderr 与退出状态报告。

use std::io::Write;

use serde::Serialize;
use singularity_protocol::{TerminalSummary, TurnEvent, TurnModelUsage, TurnStatus};

pub struct JsonlRenderer {
    out: Box<dyn Write>,
    thread_id: Option<String>,
    /// 第一条输出故障（行编码或 stdout 写入）。存在时跳过后续事件行。
    output_error: Option<String>,
    /// stdout 写入或刷新是否已经失败。失败后该通道不再能确认行边界，后续任何
    /// 行（含 summary）都不再写入；纯编码失败不置位——输出流本身仍然完好。
    stream_failed: bool,
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
            stream_failed: false,
        }
    }

    /// 输出一行事件；事件自身的 tagged serde 形状就是 JSONL 信封，直接
    /// 编码写入。stdout 写失败置位输出通道故障（后续行一律跳过），终态行
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
    /// 已发生 stdout 写故障时不再尝试写出：流上可能残留半行，补写只会把两个
    /// JSON 接在同一行；失败原因已记录在 Self::output_failure，调用方读取它并以
    /// 失败退出（ProcessOutcome::Failed，错误文本带上底层写失败原因）。
    pub fn emit_summary(
        &mut self,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) {
        if self.stream_failed {
            return;
        }
        let summary = TerminalSummary::new(self.thread_id.as_deref(), status, usage, truncated);
        self.write_line(&summary.to_line());
    }

    /// 编码并写完一整行：整行一次 write_all 提交。行不可编码只影响该行（输出流
    /// 仍然完好）；stdout 写入或刷新失败则同时置位通道故障标志，因为失败前可能
    /// 已写出部分字节。两种失败都保留底层原因。
    fn write_line(&mut self, line: &impl Serialize) {
        let bytes = match serde_json::to_vec(line) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                bytes
            }
            Err(error) => return self.record_output_failure(error.to_string(), false),
        };
        if let Err(error) = self.out.write_all(&bytes).and_then(|()| self.out.flush()) {
            self.record_output_failure(error.to_string(), true);
        }
    }

    /// 记录第一条输出故障；stdout 写故障无论先后都置位通道标志。
    fn record_output_failure(&mut self, error: String, stream_failed: bool) {
        self.stream_failed |= stream_failed;
        self.output_error.get_or_insert(error);
    }

    /// 返回此渲染器观察到的第一个输出故障（行编码或 stdout 写入）。
    pub fn output_failure(&self) -> Option<&str> {
        self.output_error.as_deref()
    }
}
