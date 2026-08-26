//! `--print` 渲染：stdout 只输出最终 assistant 文本。
//!
//! 工具过程与普通事件不进入 stdout；warning/error 级诊断按合同写入 stderr，
//! 不混入文本结果。

use std::io::Write;

use singularity_runtime::events::{AgentDiagnosticSeverity, TurnEvent};

pub struct PrintRenderer;

impl PrintRenderer {
    pub fn new() -> Self {
        Self
    }

    /// 观察事件：只有 warning/error 诊断投影到 stderr，其余全部丢弃。
    pub fn emit(&mut self, event: &TurnEvent) {
        if let TurnEvent::Diagnostic {
            severity,
            code,
            message,
            ..
        } = event
            && matches!(
                severity,
                AgentDiagnosticSeverity::Warning | AgentDiagnosticSeverity::Error
            )
        {
            let stderr = std::io::stderr();
            let mut lock = stderr.lock();
            let _ = writeln!(lock, "sg: [{severity}] {code}: {message}");
            let _ = lock.flush();
        }
    }

    /// 输出最终 assistant 文本（唯一进入 stdout 的内容）。
    pub fn write_final_text(&self, text: &str) {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, "{text}");
        let _ = lock.flush();
    }

    pub fn warn_truncated(&self) {
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = writeln!(
            lock,
            "sg: [warning] Response was truncated before completion."
        );
        let _ = lock.flush();
    }
}

impl Default for PrintRenderer {
    fn default() -> Self {
        Self::new()
    }
}
