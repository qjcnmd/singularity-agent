//! 单个 turn 的原子终态提交：`turn_terminal` 的构造、幂等落盘与事件投影。
//!
//! 终态化不再有"terminal 与 usage 两次独立追加"或"降级成另一个状态"的路径：
//! 构造、校验、落盘与投影收敛到此处，一次写入要么完整、要么根本不产生任何
//! 终态事实（fail-stop 由调用方依据 `persist` 结果实施）。

use singularity_agent::session::{
    SessionManager, SessionMetadata, SessionTurnUsage, TurnTerminalStatus,
};
use singularity_model::ModelUsage;
use singularity_protocol::diagnostic_code;

use crate::error::TurnFailure;
use crate::events::{AgentDiagnosticSeverity, TurnEvent, TurnEventSink};
use crate::objects::{ThreadStatus, Turn, TurnStatus, TurnUsage, turn_usage_from_model_usage};

/// wire usage 投影 → JSONL 存储形状（字段一一对应，camelCase 落盘不变）。
fn session_turn_usage(usage: &TurnUsage) -> SessionTurnUsage {
    SessionTurnUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_present: usage.usage_present,
        usage_complete: usage.usage_complete,
    }
}

/// 单个 turn 的原子终态提交：`turn_terminal` 的构造、幂等落盘与事件投影。
pub(crate) struct TerminalCommit {
    turn_id: String,
    status: TurnTerminalStatus,
    usage: TurnUsage,
    usage_complete: bool,
}

impl TerminalCommit {
    /// 从线程状态构造终态提交；`Active`（非终态）返回 `None`。
    pub(crate) fn new(
        turn_id: &str,
        status: ThreadStatus,
        usage: &ModelUsage,
        usage_complete: bool,
    ) -> Option<Self> {
        let status = terminal_status_for_thread_status(status)?;
        Some(Self {
            turn_id: turn_id.to_string(),
            status,
            usage: turn_usage_from_model_usage(usage, usage_complete),
            usage_complete,
        })
    }

    /// 构造终态 metadata 条目（status + usage + usageComplete 单条）。
    fn metadata(&self) -> SessionMetadata {
        SessionMetadata::turn_terminal(
            &self.turn_id,
            self.status,
            session_turn_usage(&self.usage),
            self.usage_complete,
        )
    }

    /// 单条落盘终态 metadata；同内容已存在时幂等跳过。
    pub(crate) fn persist(&self, session: &mut SessionManager) -> Result<(), String> {
        append_terminal_metadata_if_missing(session, &self.turn_id, self.metadata())
    }

    /// 终态事件层 Turn 投影（携带本轮已落盘的 usage）。
    pub(crate) fn turn(&self, thread_id: &str, turn_status: TurnStatus) -> Turn {
        Turn {
            turn_id: self.turn_id.clone(),
            thread_id: thread_id.to_string(),
            status: turn_status,
            usage: Some(self.usage.clone()),
        }
    }
}

/// 终态无法落盘时的 fail-stop 出口：发 `storage_fatal` 诊断，不发布任何
/// 终态事件——磁盘与客户端之间不存在矛盾窗口。
pub(crate) fn fail_stop_terminalization(
    thread_id: &str,
    turn_id: &str,
    failure: &TurnFailure,
    sink: &mut dyn TurnEventSink,
) {
    let message = failure
        .original
        .clone()
        .unwrap_or_else(|| "fatal storage error: failed to persist terminal metadata".to_string());
    sink.emit(TurnEvent::Diagnostic {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        severity: AgentDiagnosticSeverity::Error,
        code: diagnostic_code::STORAGE_FATAL.to_string(),
        message,
    });
}

fn append_terminal_metadata_if_missing(
    session: &mut SessionManager,
    turn_id: &str,
    metadata: SessionMetadata,
) -> Result<(), String> {
    // 幂等按完整终态内容判定：同一 turn 已存在相同终态即视为已写，不重复追加。
    let already_terminal = session
        .metadata_entries()
        .iter()
        .any(|entry| entry.turn_id() == Some(turn_id) && entry == &metadata);
    if !already_terminal {
        session
            .append_metadata(metadata)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn terminal_status_for_thread_status(status: ThreadStatus) -> Option<TurnTerminalStatus> {
    match status {
        ThreadStatus::Completed => Some(TurnTerminalStatus::Completed),
        ThreadStatus::Failed => Some(TurnTerminalStatus::Failed),
        ThreadStatus::Interrupted => Some(TurnTerminalStatus::Interrupted),
        ThreadStatus::Active => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use singularity_agent::session::SessionMetadataKind;

    use crate::error::{TurnFailureCause, TurnFailureStage};

    /// 终态+用量单条原子写入：一次 persist 恰好一条 `turn_terminal`，内容完整；
    /// 相同内容重复 persist 幂等跳过。
    #[test]
    fn terminal_commit_is_single_entry_and_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session =
            SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
        let usage = ModelUsage {
            input_tokens: 100,
            total_tokens: 150,
            ..Default::default()
        };
        let commit =
            TerminalCommit::new("turn-1", ThreadStatus::Completed, &usage, true).expect("terminal");
        commit.persist(&mut session).expect("first persist");
        commit.persist(&mut session).expect("idempotent persist");

        let terminals: Vec<SessionMetadata> = session
            .metadata_entries()
            .into_iter()
            .filter(|entry| entry.kind() == SessionMetadataKind::TurnTerminal)
            .collect();
        assert_eq!(terminals.len(), 1, "single atomic terminal entry");
        assert_eq!(terminals[0].turn_id(), Some("turn-1"));
        assert_eq!(
            terminals[0].terminal_status(),
            Some(TurnTerminalStatus::Completed)
        );
        let SessionMetadata::TurnTerminal {
            usage: persisted,
            usage_complete,
            ..
        } = &terminals[0]
        else {
            unreachable!("filtered to TurnTerminal");
        };
        assert!(*usage_complete, "usageComplete persisted");
        assert_eq!(persisted.input_tokens, 100);
        assert_eq!(persisted.total_tokens, 150);
    }

    /// 幂等按完整终态内容判定：同 turn 不同终态是新内容，如实追加（不跳过）。
    #[test]
    fn terminal_commit_distinguishes_different_terminal_content() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session =
            SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
        let usage = ModelUsage::default();
        let completed =
            TerminalCommit::new("turn-1", ThreadStatus::Completed, &usage, true).expect("terminal");
        completed.persist(&mut session).expect("completed persist");
        let failed =
            TerminalCommit::new("turn-1", ThreadStatus::Failed, &usage, true).expect("terminal");
        failed.persist(&mut session).expect("failed persist");

        let terminals: Vec<TurnTerminalStatus> = session
            .metadata_entries()
            .iter()
            .filter_map(singularity_agent::session::SessionMetadata::terminal_status)
            .collect();
        assert_eq!(
            terminals,
            vec![TurnTerminalStatus::Completed, TurnTerminalStatus::Failed]
        );
    }

    /// 终态无法落盘 → fail-stop：只发 `storage_fatal` 诊断，不发布任何终态事件。
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
            TerminalCommit::new("turn-1", ThreadStatus::Completed, &usage, true).expect("terminal");
        assert!(commit.persist(&mut session).is_err(), "append must fail");

        let mut events = Vec::new();
        let failure = TurnFailure {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
            original: Some("injected storage failure".to_string()),
        };
        fail_stop_terminalization("thread-1", "turn-1", &failure, &mut |event| {
            events.push(event)
        });
        assert!(
            matches!(
                events.as_slice(),
                [TurnEvent::Diagnostic {
                    code,
                    severity: AgentDiagnosticSeverity::Error,
                    ..
                }] if code == diagnostic_code::STORAGE_FATAL
            ),
            "fail-stop must emit exactly one storage_fatal diagnostic: {events:?}"
        );
    }
}
