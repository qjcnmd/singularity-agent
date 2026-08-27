//! Turn 执行事件出口。

pub use singularity_protocol::{
    DiagnosticSeverity as AgentDiagnosticSeverity, ProviderAttemptStatus, TurnErrorDetail,
    TurnEvent,
};

/// 事件观察端。投影失败不得影响 Agent 执行：实现方自行吞掉并记录自身的
/// 投影错误，runtime 只保证按合同顺序调用。
pub trait TurnEventSink {
    fn emit(&mut self, event: TurnEvent);
}

impl<F> TurnEventSink for F
where
    F: FnMut(TurnEvent),
{
    fn emit(&mut self, event: TurnEvent) {
        self(event)
    }
}
