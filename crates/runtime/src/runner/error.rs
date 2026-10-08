use super::*;

/// 这套穷尽的分类同时决定终态原因，以及是否必须停止执行链。存储故障写不出可信终态，
/// 执行器按 Store 分类停止执行链。
pub(super) fn classify_agent_error(error: &AgentError) -> TurnFailureCause {
    match error {
        AgentError::Provider(error) => provider_turn_cause(error.kind),
        AgentError::Session(_) | AgentError::FailureRecording { .. } => TurnFailureCause::Store,
        AgentError::Instructions(_) => TurnFailureCause::ProjectInstructions,
        AgentError::Aborted | AgentError::InvalidSummary(_) => TurnFailureCause::Internal,
    }
}

/// 终态写不下去时的 fail-stop 出口：发出 storage_fatal 诊断，不发布终态事件，客户端不会
/// 把没落盘的结果当成完成。已发生的执行失败随同一份错误报告，不被收尾故障覆盖。
pub(super) fn fail_stop_terminalization(
    thread_id: &str,
    turn_id: &str,
    execution: Option<&TurnErrorDetail>,
    storage_error: String,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    publish_storage_fatal(thread_id, turn_id, &storage_error, sink);
    TurnRunError::Terminalization {
        execution: execution.cloned(),
        storage: storage_error,
    }
}

/// 执行期存储故障的 fail-stop 出口：不写终态记录，也不发布终态事件。
pub(super) fn fail_stop_execution(
    thread_id: &str,
    turn_id: &str,
    detail: TurnErrorDetail,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    publish_storage_fatal(thread_id, turn_id, &detail.message, sink);
    TurnRunError::Execution(detail)
}

fn publish_storage_fatal(thread_id: &str, turn_id: &str, message: &str, sink: &mut dyn FnMut(TurnEvent)) {
    sink(TurnEvent::Diagnostic {
        thread_id: thread_id.to_string(),
        turn_id: Some(turn_id.to_string()),
        severity: DiagnosticSeverity::Error,
        code: diagnostic_code::STORAGE_FATAL.to_string(),
        message: message.to_string(),
    });
}
