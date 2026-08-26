//! 协议投影适配器：把 runtime 的 [`TurnEvent`] 一一映射为 JSON-RPC 通知，
//! JSONL 由 runtime 先落盘；这里不维护第二份会话状态。
//!
//! 事件配对（item/started 与终端、turn 开始与终态）由 runtime 保证；这里
//! 不做任何执行侧状态机，只做纯投影。

use std::sync::atomic::Ordering;

use singularity_protocol::{
    DiagnosticSeverity, JsonRpcId, JsonRpcMessage,
    ProviderAttemptStatus as ProtocolProviderAttemptStatus,
    TurnFailureCause as ProtocolFailureCause, TurnFailureStage as ProtocolFailureStage,
    TurnStartResult,
};
use singularity_runtime::events::{
    AgentDiagnosticSeverity, ProviderAttemptStatus as RuntimeProviderAttemptStatus, TurnEvent,
    TurnEventSink,
};
use singularity_runtime::objects::{ThreadStatus as RuntimeThreadStatus, Turn as RuntimeTurn};
use singularity_runtime::{
    Conversation, ProviderFailureKind, TurnFailureCause as RuntimeFailureCause,
    TurnFailureStage as RuntimeFailureStage, TurnOutcome, TurnRunError,
};

use super::*;

/// 把一个 runtime turn 投影为协议线格式。
pub(crate) fn wire_turn(turn: &RuntimeTurn) -> Turn {
    Turn {
        turn_id: turn.turn_id.clone(),
        thread_id: turn.thread_id.clone(),
        status: match turn.status {
            singularity_runtime::objects::TurnStatus::Running => TurnStatus::Running,
            singularity_runtime::objects::TurnStatus::Completed => TurnStatus::Completed,
            singularity_runtime::objects::TurnStatus::Failed => TurnStatus::Failed,
            singularity_runtime::objects::TurnStatus::Interrupted => TurnStatus::Interrupted,
        },
        model_usage: turn.usage.as_ref().map(|usage| {
            usage_to_wire_with_completeness(&usage.to_model_usage(), usage.usage_complete)
        }),
    }
}

/// 单次 run_turn 调用的协议投影。
///
/// 所有投影失败都通过 [`Self::poisoned`] 暂存并在 run 返回后以错误返回，
/// 不再发布后续事件；JSONL 事实已在 runtime 侧先行落盘。
///
/// 边界（窗口极窄，仅锁中毒等异常触发）：投影 poisoned 后 transport 向
/// 客户端发送 error response，但 turn 继续执行并写 JSONL——客户端视角的
/// "失败"与执行事实可能短暂背离；turn 自身的终态仍由 runtime 收敛。
pub(crate) struct TurnProjection<'a> {
    server: &'a AppServer,
    conversation: Arc<Conversation>,
    request_id: JsonRpcId,
    emit: &'a mut dyn FnMut(Value),
    response_sent: bool,
    poisoned: Option<AppServerError>,
}

impl<'a> TurnProjection<'a> {
    pub fn new(
        server: &'a AppServer,
        conversation: Arc<Conversation>,
        request_id: JsonRpcId,
        emit: &'a mut dyn FnMut(Value),
    ) -> Self {
        Self {
            server,
            conversation,
            request_id,
            emit,
            response_sent: false,
            poisoned: None,
        }
    }

    /// 取走暂存的投影失败（run 返回后由调用方决定错误响应形态）。
    pub fn take_poisoned(&mut self) -> Option<AppServerError> {
        self.poisoned.take()
    }

    fn emit_value(&mut self, value: Value) {
        if self.poisoned.is_none() {
            (self.emit)(value);
        }
    }

    fn emit_notification(&mut self, event: AppEvent) {
        match self.server.event_notification(event) {
            Ok(value) => self.emit_value(value),
            Err(error) => self.poison_or(error),
        }
    }

    fn poison_or(&mut self, error: AppServerError) {
        if self.poisoned.is_none() {
            self.poisoned = Some(error);
        }
    }

    /// turn/started 到达时 JSONL 已落盘。执行停止请求在入场时立即取消，
    /// 使被取消的轮快速收敛为 interrupted。
    fn on_turn_started(&mut self, turn: &RuntimeTurn) {
        if self.server.execution_stopped.load(Ordering::SeqCst) {
            self.conversation.interrupt();
        }
        self.emit_notification(AppEvent::turn_started(&wire_turn(turn)));
        if !self.response_sent {
            self.response_sent = true;
            let value = match serde_json::to_value(TurnStartResult {
                turn: wire_turn(turn),
            }) {
                Ok(value) => value,
                Err(error) => {
                    self.poison_or(AppServerError::InvalidJson(error));
                    return;
                }
            };
            self.emit_value(
                JsonRpcMessage::response(self.request_id.clone(), value).to_wire_value(),
            );
        }
    }
}

impl TurnEventSink for TurnProjection<'_> {
    fn emit(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::TurnStarted { turn } => self.on_turn_started(&turn),
            TurnEvent::TurnCompleted { turn } => {
                self.emit_notification(AppEvent::turn_completed(&wire_turn(&turn)));
            }
            TurnEvent::TurnFailed { turn, error } => {
                self.emit_notification(AppEvent::turn_error(
                    &turn.turn_id,
                    &turn.thread_id,
                    protocol_failure_stage(error.stage),
                    protocol_failure_cause(error.cause),
                    &error.message,
                ));
            }
            TurnEvent::ThreadStarted { thread } => {
                self.emit_notification(AppEvent::thread_started(&protocol_thread(&thread)));
            }
            TurnEvent::ThreadSettingsApplied { .. } => {}
            TurnEvent::ItemStarted {
                thread_id,
                turn_id,
                item_id,
            } => self.emit_notification(AppEvent::item_started(thread_id, turn_id, item_id)),
            TurnEvent::AssistantDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            } => self.emit_notification(AppEvent::item_agent_message_delta(
                thread_id, turn_id, item_id, delta,
            )),
            TurnEvent::ItemCompleted {
                thread_id,
                turn_id,
                item_id,
            } => self.emit_notification(AppEvent::item_completed(thread_id, turn_id, item_id)),
            TurnEvent::ItemFailed {
                thread_id,
                turn_id,
                item_id,
                error,
            } => self.emit_notification(AppEvent::item_failed(thread_id, turn_id, item_id, error)),
            TurnEvent::ToolExecutionStart {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
            } => self.emit_notification(AppEvent::tool_execution_start(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
            )),
            TurnEvent::ToolExecutionUpdate {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => self.emit_notification(AppEvent::tool_execution_update(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
                partial_result,
            )),
            TurnEvent::ToolExecutionEnd {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => self.emit_notification(AppEvent::tool_execution_end(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                result,
                is_error,
            )),
            TurnEvent::Diagnostic {
                thread_id,
                turn_id,
                severity,
                code,
                message,
            } => self.emit_notification(AppEvent::agent_diagnostic(
                thread_id,
                turn_id,
                protocol_diagnostic_severity(severity),
                code,
                message,
            )),
            TurnEvent::ProviderAttempt {
                thread_id,
                turn_id,
                model_turn_ordinal,
                provider,
                model,
                protocol,
                status,
                attempt_duration_ms,
                error_category,
                diagnostic_code,
            } => self.emit_notification(AppEvent::provider_attempt(
                thread_id,
                turn_id,
                model_turn_ordinal,
                provider,
                model,
                protocol,
                protocol_provider_attempt_status(status),
                attempt_duration_ms,
                error_category,
                diagnostic_code,
            )),
        }
    }
}

fn protocol_diagnostic_severity(severity: AgentDiagnosticSeverity) -> DiagnosticSeverity {
    match severity {
        AgentDiagnosticSeverity::Info => DiagnosticSeverity::Info,
        AgentDiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
        AgentDiagnosticSeverity::Error => DiagnosticSeverity::Error,
    }
}

fn protocol_provider_attempt_status(
    status: RuntimeProviderAttemptStatus,
) -> ProtocolProviderAttemptStatus {
    match status {
        RuntimeProviderAttemptStatus::Started => ProtocolProviderAttemptStatus::Started,
        RuntimeProviderAttemptStatus::Ok => ProtocolProviderAttemptStatus::Ok,
        RuntimeProviderAttemptStatus::Error => ProtocolProviderAttemptStatus::Error,
        RuntimeProviderAttemptStatus::Cancelled => ProtocolProviderAttemptStatus::Cancelled,
    }
}

fn protocol_failure_stage(stage: RuntimeFailureStage) -> ProtocolFailureStage {
    match stage {
        RuntimeFailureStage::AgentLoop => ProtocolFailureStage::AgentLoop,
        RuntimeFailureStage::TerminalOutcome => ProtocolFailureStage::TerminalOutcome,
    }
}

fn protocol_failure_cause(cause: RuntimeFailureCause) -> ProtocolFailureCause {
    match cause {
        RuntimeFailureCause::Store => ProtocolFailureCause::Store,
        RuntimeFailureCause::ProjectInstructions => ProtocolFailureCause::ProjectInstructions,
        RuntimeFailureCause::Workspace => ProtocolFailureCause::Workspace,
        RuntimeFailureCause::Provider(ProviderFailureKind::RateLimited) => {
            ProtocolFailureCause::ProviderRateLimited
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Network) => {
            ProtocolFailureCause::ProviderNetwork
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Timeout) => {
            ProtocolFailureCause::ProviderTimeout
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Auth) => {
            ProtocolFailureCause::ProviderAuth
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Validation) => {
            ProtocolFailureCause::ProviderValidation
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Overloaded) => {
            ProtocolFailureCause::ProviderOverloaded
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Cancelled) => {
            ProtocolFailureCause::ProviderCancelled
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::ContextOverflow) => {
            ProtocolFailureCause::ProviderContextOverflow
        }
        RuntimeFailureCause::Provider(ProviderFailureKind::Unknown) => {
            ProtocolFailureCause::ProviderUnknown
        }
        RuntimeFailureCause::Serialization => ProtocolFailureCause::Serialization,
        RuntimeFailureCause::Internal => ProtocolFailureCause::Internal,
    }
}

fn protocol_thread(thread: &singularity_runtime::objects::Thread) -> Thread {
    Thread {
        thread_id: thread.thread_id.clone(),
        cwd: Some(thread.cwd.clone()),
        model: thread.model.clone(),
        last_turn_status: thread.last_turn_status.map(|status| match status {
            RuntimeThreadStatus::Active => ThreadStatus::Active,
            RuntimeThreadStatus::Completed => ThreadStatus::Completed,
            RuntimeThreadStatus::Failed => ThreadStatus::Failed,
            RuntimeThreadStatus::Interrupted => ThreadStatus::Interrupted,
        }),
    }
}

/// 把 run_turn 的返回收敛为 RPC 边界结果。
///
/// - 执行失败（`Execution`）：终态事件已投影，返回 Ok；
/// - 准备失败：返回 typed TurnExecution（无任何 turn 语义）；
/// - 终态化失败：返回 typed TurnTerminalization（不发布虚假终态）。
pub(crate) fn classify_run_result(
    result: Result<TurnOutcome, TurnRunError>,
) -> AppServerResult<()> {
    match result {
        Ok(_) => Ok(()),
        Err(TurnRunError::Preparation { cause, message }) => Err(AppServerError::TurnExecution {
            stage: TurnFailureStage::AgentLoop,
            cause,
            original: Some(message),
        }),
        Err(TurnRunError::Execution(_)) => Ok(()),
        Err(TurnRunError::Terminalization(failure)) => Err(AppServerError::TurnTerminalization {
            stage: failure.stage,
            cause: failure.cause,
            failure: TurnTerminalizationFailure::Store,
            original: failure.original,
        }),
    }
}
