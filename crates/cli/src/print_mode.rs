//! `--print` 渲染：stdout 只输出最终 assistant 文本。
//!
//! 工具过程与普通事件不进入 stdout；warning/error 级诊断按合同写入 stderr，
//! 不混入文本结果。stdout/stderr 写入 sink 可注入，输出失败路径由调用方
//! 收敛到精确进程结果。

use std::io::Write;

use singularity_runtime::events::{DiagnosticSeverity, TurnEvent};

/// `--print` 渲染器：只观察事件并按合同写 stdout/stderr。
pub struct PrintRenderer {
    out: Box<dyn Write>,
    err: Box<dyn Write>,
}

impl PrintRenderer {
    /// 生产构造：文本写真实 stdout，诊断写真实 stderr。
    pub fn stdout() -> Self {
        Self::with_writers(std::io::stdout(), std::io::stderr())
    }

    /// 测试注入：两个 sink 独立可替换（输出失败与 stderr 投影的确定性验证）。
    pub fn with_writers(out: impl Write + 'static, err: impl Write + 'static) -> Self {
        Self {
            out: Box::new(out),
            err: Box::new(err),
        }
    }

    /// 观察事件：只有 warning/error 诊断投影到 stderr，其余全部丢弃。
    pub fn on_event(&mut self, event: &TurnEvent) {
        if let TurnEvent::Diagnostic {
            severity,
            code,
            message,
            ..
        } = event
            && matches!(
                severity,
                DiagnosticSeverity::Warning | DiagnosticSeverity::Error
            )
        {
            let _ = writeln!(self.err, "sg: [{severity}] {code}: {message}");
            let _ = self.err.flush();
        }
    }

    /// 输出最终 assistant 文本（唯一进入 stdout 的内容）。
    /// stdout 写失败以 `Err` 返回，调用方据此以 Output 类别收敛。
    pub fn write_final_text(&mut self, text: &str) -> Result<(), String> {
        writeln!(self.out, "{text}")
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
    }

    pub fn warn_truncated(&mut self) {
        let _ = writeln!(
            self.err,
            "sg: [warning] Response was truncated before completion."
        );
        let _ = self.err.flush();
    }
}
