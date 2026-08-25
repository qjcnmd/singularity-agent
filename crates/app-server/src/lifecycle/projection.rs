//! 协议投影适配器：把 runtime 的 [`TurnEvent`] 一一映射为 JSON-RPC 通知，
//! 并维持「JSONL 先落盘 → 索引投影 → 客户端事件」的既有顺序。
//!
//! 事件配对（item/started 与终端、turn 开始与终态）由 runtime 保证；这里
//! 不做任何执行侧状态机，只做纯投影与索引同步。

use std::sync::atomic::Ordering;

use singularity_protocol::{JsonRpcId, JsonRpcMessage, TurnStartResult};
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::objects::{
    ThreadStatus as RuntimeThreadStatus, Turn as RuntimeTurn, TurnStatus as RuntimeTurnStatus,
};
use singularity_runtime::{Conversation, TurnOutcome, TurnRunError};

use super::*;
use crate::state::LiveTurnGuard;

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

fn session_status(status: &RuntimeTurnStatus) -> SessionStatus {
    match status {
        RuntimeTurnStatus::Completed => SessionStatus::Completed,
        RuntimeTurnStatus::Interrupted => SessionStatus::Interrupted,
        _ => SessionStatus::Failed,
    }
}

/// provider 分类的裸术语表：runtime 输出裸词，协议线格式带 `provider_` 前缀。
const PROVIDER_CAUSE_WORDS: [&str; 9] = [
    "rate_limited",
    "network",
    "timeout",
    "auth",
    "validation",
    "overloaded",
    "cancelled",
    "context_overflow",
    "unknown",
];

/// 终端化错误 cause 的 wire 词形归一。
pub(crate) fn wire_error_cause(cause: &str) -> String {
    if PROVIDER_CAUSE_WORDS.contains(&cause) {
        format!("provider_{cause}")
    } else {
        cause.to_string()
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
    thread_id: String,
    title: Option<String>,
    emit: &'a mut dyn FnMut(Value),
    live_turn: Option<LiveTurnGuard>,
    response_sent: bool,
    started_synced: bool,
    poisoned: Option<AppServerError>,
}

impl<'a> TurnProjection<'a> {
    pub fn new(
        server: &'a AppServer,
        conversation: Arc<Conversation>,
        request_id: JsonRpcId,
        thread_id: &str,
        title: Option<String>,
        emit: &'a mut dyn FnMut(Value),
    ) -> Self {
        Self {
            server,
            conversation,
            request_id,
            thread_id: thread_id.to_string(),
            title,
            emit,
            live_turn: None,
            response_sent: false,
            started_synced: false,
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

    /// turn/started 到达：JSONL 已落盘，先投影索引（Active + 首轮标题），
    /// 随后才允许发布通知与 RPC 响应。执行停止请求在入场时立即取消，
    /// 使被取消的轮快速收敛为 interrupted。
    fn on_turn_started(&mut self, turn: &RuntimeTurn) {
        if self.server.execution_stopped.load(Ordering::SeqCst) {
            self.conversation.interrupt();
        }
        if !self.started_synced {
            self.started_synced = true;
            let title = self.title.take().unwrap_or_default();
            let record = self.server.store().get_session(&self.thread_id);
            let metadata = match record {
                Ok(record) => SessionMetadataUpdate {
                    status: Some(SessionStatus::Active),
                    title: if record.title.is_none() && !title.is_empty() {
                        Some(Some(&title))
                    } else {
                        None
                    },
                    ..SessionMetadataUpdate::default()
                },
                Err(error) => {
                    self.poison_or(AppServerError::Store(error));
                    return;
                }
            };
            if let Err(error) = self
                .server
                .store()
                .update_session(&self.thread_id, metadata)
            {
                self.poison_or(AppServerError::Store(error));
                return;
            }
        }
        self.live_turn = Some(
            self.server
                .register_live_turn(&turn.turn_id, &turn.thread_id),
        );
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

    fn sync_terminal_index(&mut self, turn: &RuntimeTurn) {
        let usage_value = turn
            .usage
            .as_ref()
            .map(|usage| {
                usage_to_wire_with_completeness(&usage.to_model_usage(), usage.usage_complete)
            })
            .unwrap_or_else(|| singularity_protocol::TurnModelUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cached_input_tokens: 0,
                reasoning_tokens: 0,
                usage_present: false,
                usage_complete: false,
            });
        let usage = match serde_json::to_value(usage_value) {
            Ok(usage) => usage,
            Err(error) => {
                self.poison_or(AppServerError::InvalidJson(error));
                return;
            }
        };
        let metadata = SessionMetadataUpdate {
            status: Some(session_status(&turn.status)),
            token_usage: Some(&usage),
            ..SessionMetadataUpdate::default()
        };
        if let Err(error) = self
            .server
            .store()
            .update_session(&self.thread_id, metadata)
        {
            self.poison_or(AppServerError::Store(error));
        }
    }
}

impl TurnEventSink for TurnProjection<'_> {
    fn emit(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::TurnStarted { turn } => self.on_turn_started(&turn),
            TurnEvent::TurnCompleted { turn } => {
                // durable 终态已由 runtime 落盘；先同步索引再发布终态通知。
                self.sync_terminal_index(&turn);
                self.live_turn = None;
                self.emit_notification(AppEvent::turn_completed(&wire_turn(&turn)));
            }
            TurnEvent::TurnFailed { turn, error } => {
                self.sync_terminal_index(&turn);
                self.live_turn = None;
                self.emit_notification(AppEvent::turn_error(
                    &turn.turn_id,
                    &turn.thread_id,
                    &error.stage,
                    &wire_error_cause(&error.cause),
                    &error.message,
                ));
            }
            TurnEvent::ThreadStarted { thread } => {
                self.emit_notification(AppEvent::thread_started(&protocol_thread(&thread)));
            }
            // 待生效设置已在可信终态后持久化：JSONL 先落盘，此处同步索引
            // model 投影，随后客户端读到的 thread/list 与 thread/read 一致。
            TurnEvent::ThreadSettingsApplied { thread } => {
                let metadata = SessionMetadataUpdate {
                    model: Some(thread.model.as_deref()),
                    ..SessionMetadataUpdate::default()
                };
                if let Err(error) = self
                    .server
                    .store()
                    .update_session(&thread.thread_id, metadata)
                {
                    self.poison_or(AppServerError::Store(error));
                }
            }
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
                thread_id, turn_id, severity, code, message,
            )),
            TurnEvent::ProviderAttempt {
                thread_id,
                turn_id,
                model_turn_ordinal,
                operation_phase,
                provider,
                model,
                protocol,
                attempt_index,
                status,
                attempt_duration_ms,
                retry_scheduled,
                retry_backoff_ms,
                error_category,
                diagnostic_code,
            } => self.emit_notification(AppEvent::provider_attempt(
                thread_id,
                turn_id,
                model_turn_ordinal,
                operation_phase,
                provider,
                model,
                protocol,
                attempt_index,
                status,
                attempt_duration_ms,
                retry_scheduled,
                retry_backoff_ms,
                error_category,
                diagnostic_code,
            )),
        }
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
            cause: cause.into(),
            original: Some(message),
        }),
        Err(TurnRunError::Execution(_)) => Ok(()),
        Err(TurnRunError::Terminalization(failure)) => Err(AppServerError::TurnTerminalization {
            stage: failure.stage.into(),
            cause: failure.cause.into(),
            failure: TurnTerminalizationFailure::Store,
            original: failure.original,
        }),
    }
}
