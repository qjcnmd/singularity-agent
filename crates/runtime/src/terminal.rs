//! 单个 turn 的终态提交：operation_finished 记录的构造、落盘与事件投影。
//!
//! status/usage/truncated 保存在同一条记录中，成功追加后才发布终态事件。
//! 写入失败由调用方 fail-stop；部分写入的尾部由下一次写打开修复。

use singularity_agent::session::{LedgerRecord, SessionManager};
use singularity_model::ModelUsage;
use singularity_protocol::diagnostic_code;

use crate::error::{TurnFailureCause, TurnFailureStage, TurnRunError};
use singularity_agent::session::turn_usage_from_model_usage;
use singularity_protocol::{DiagnosticSeverity, TurnEvent};
use singularity_protocol::{Turn, TurnModelUsage, TurnStatus};

/// 单个 turn 的终态提交：operation_finished 的构造、落盘与事件投影。
///
/// TurnStatus 是终态的唯一事实：落盘形状与事件形状共用同一枚举，
/// 构造时一次判定（Running 非终态，不产生提交）。
pub(crate) struct TerminalCommit {
    operation_id: String,
    turn_id: String,
    status: TurnStatus,
    usage: TurnModelUsage,
    truncated: bool,
}

impl TerminalCommit {
    /// 从 turn 终态构造提交；非终态（Running）返回 None。
    pub(crate) fn new(
        operation_id: &str,
        turn_id: &str,
        status: TurnStatus,
        usage: &ModelUsage,
        usage_complete: bool,
        truncated: bool,
    ) -> Option<Self> {
        if status == TurnStatus::Running {
            return None;
        }
        Some(Self {
            operation_id: operation_id.to_string(),
            turn_id: turn_id.to_string(),
            status,
            usage: turn_usage_from_model_usage(usage, usage_complete),
            truncated,
        })
    }

    /// 构造终态 ledger 记录（status + usage + truncated 单条）。
    fn record(&self, user_stopped: bool) -> LedgerRecord {
        LedgerRecord::OperationFinished {
            operation_id: self.operation_id.clone(),
            turn_id: Some(self.turn_id.clone()),
            outcome: self.status,
            usage: Some(self.usage.clone()),
            truncated: self.truncated,
            user_stopped,
        }
    }

    /// 单条落盘终态记录（一次 commit 恰好一次 persist；turn id 每轮
    /// 新生，不存在重复提交路径）。
    pub(crate) fn persist(
        &self,
        session: &mut SessionManager,
        user_stopped: bool,
    ) -> Result<(), String> {
        session
            .append_record(self.record(user_stopped))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// 终态事件层 Turn 投影（携带本轮已落盘的 usage 与同一终态）。
    pub(crate) fn turn(&self, thread_id: &str) -> Turn {
        Turn {
            turn_id: self.turn_id.clone(),
            thread_id: thread_id.to_string(),
            status: self.status,
            usage: Some(self.usage.clone()),
        }
    }

    /// 终态 usage 投影：终态事件与 TurnOutcome 共享同一份已落盘 usage。
    pub(crate) fn usage(&self) -> &TurnModelUsage {
        &self.usage
    }
}

/// 终态无法落盘时的 fail-stop 出口：发 storage_fatal 诊断，不发布任何
/// 终态事件；客户端不会把未确认写入的结果当作完成。
pub(crate) fn fail_stop_terminalization(
    thread_id: &str,
    turn_id: &str,
    storage_error: String,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    let failure = singularity_protocol::TurnErrorDetail {
        stage: TurnFailureStage::TerminalOutcome,
        cause: TurnFailureCause::Store,
        message: storage_error.clone(),
    };
    sink(TurnEvent::Diagnostic {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        severity: DiagnosticSeverity::Error,
        code: diagnostic_code::STORAGE_FATAL.to_string(),
        message: storage_error,
    });
    TurnRunError::Terminalization(failure)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    /// 终态无法落盘 → fail-stop：只发 storage_fatal 诊断，不发布任何终态事件。
    #[test]
    fn terminal_persist_failure_emits_no_terminal_events() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session =
            SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
        let mut permissions = std::fs::metadata(session.path())
            .expect("session metadata")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(session.path(), permissions).expect("make session read-only");
        let usage = ModelUsage::default();
        let commit =
            TerminalCommit::new("op-1", "turn-1", TurnStatus::Completed, &usage, true, false)
                .expect("terminal");
        assert!(
            commit.persist(&mut session, false).is_err(),
            "append must fail"
        );

        let mut events = Vec::new();
        let error = fail_stop_terminalization(
            "thread-1",
            "turn-1",
            "injected storage failure".to_string(),
            &mut |event| events.push(event),
        );
        assert!(matches!(error, TurnRunError::Terminalization(_)));
        assert!(
            matches!(
                events.as_slice(),
                [TurnEvent::Diagnostic {
                    code,
                    severity: DiagnosticSeverity::Error,
                    ..
                }] if code == diagnostic_code::STORAGE_FATAL
            ),
            "fail-stop must emit exactly one storage_fatal diagnostic: {events:?}"
        );
    }
}
