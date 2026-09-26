use super::*;

/// 同一个穷尽的分类既决定终态原因，也决定是否必须停止执行链。
/// 存储与宿主故障写不出可信终态，因此返回对应的致命诊断码。
pub(super) fn classify_agent_error(error: &AgentError) -> (TurnFailureCause, Option<&'static str>) {
    match error {
        AgentError::Provider(error) => (provider_turn_cause(error.kind), None),
        AgentError::Session(_) | AgentError::InterruptedOutput { .. } => (
            TurnFailureCause::Store,
            Some(diagnostic_code::STORAGE_FATAL),
        ),
        AgentError::HostFailure(_) => (
            TurnFailureCause::Internal,
            Some(diagnostic_code::HOST_FATAL),
        ),
        AgentError::Instructions(_) | AgentError::SkillLoad(_) => {
            (TurnFailureCause::ProjectInstructions, None)
        }
        AgentError::Aborted | AgentError::InvalidSummary(_) => (TurnFailureCause::Internal, None),
    }
}

pub(super) fn turn_error_detail(error: &AgentError) -> TurnErrorDetail {
    TurnErrorDetail {
        cause: classify_agent_error(error).0,
        message: error.to_string(),
    }
}

/// 本轮接受过停止时，未送达的输入不再进入下一轮：在事件流里和正常终态路径一样标记为
/// 已取消；启动失败、执行期致命失败和终态落盘失败共用这条处置规则。
pub(super) fn cancel_undelivered(undelivered: &[ControlRequest], sink: &mut dyn FnMut(TurnEvent)) {
    for request in undelivered {
        sink(TurnEvent::ControlChanged {
            control: request.snapshot(ControlDisposition::Cancelled),
        });
    }
}

/// 校验 thread 的工作目录仍然可用（存在，且能被规范化）；只返回通过与否，不返回另一个路径值。
/// 终态写不下去时的 fail-stop 出口：发出 storage_fatal 诊断，不发布任何终态事件，
/// 客户端不会把没确认写入的结果当成完成。已经发生的执行失败随同一份错误一起报告，
/// 不会被收尾故障覆盖。
pub(super) fn fail_stop_terminalization(
    thread_id: &str,
    turn_id: &str,
    execution: Option<&TurnErrorDetail>,
    storage_error: String,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    publish_fatal(
        thread_id,
        turn_id,
        diagnostic_code::STORAGE_FATAL,
        &storage_error,
        sink,
    );
    TurnRunError::Terminalization {
        execution: execution.cloned(),
        storage: Some(storage_error),
    }
}

/// 执行期存储/宿主故障的 fail-stop 出口：不写终态记录，也不发布终态事件。
pub(super) fn fail_stop_execution(
    thread_id: &str,
    turn_id: &str,
    error: &AgentError,
    code: &str,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    let detail = turn_error_detail(error);
    publish_fatal(thread_id, turn_id, code, &detail.message, sink);
    TurnRunError::Terminalization {
        execution: Some(detail),
        storage: None,
    }
}

pub(super) fn publish_fatal(
    thread_id: &str,
    turn_id: &str,
    code: &str,
    message: &str,
    sink: &mut dyn FnMut(TurnEvent),
) {
    sink(TurnEvent::Diagnostic {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        severity: DiagnosticSeverity::Error,
        code: code.to_string(),
        message: message.to_string(),
    });
}
