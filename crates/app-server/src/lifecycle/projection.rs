//! runtime 事件的 JSON-RPC 投影。

use singularity_protocol::{
    ExecutionTurn, JsonRpcId, JsonRpcMessage, Turn, TurnStartResult, turn_event_notification,
};
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::{TurnFailureStage, TurnOutcome, TurnRunError};

use super::*;

pub(crate) fn wire_turn(turn: &ExecutionTurn) -> Turn {
    Turn::from(turn)
}

/// 单次 run_turn 调用的协议投影。
pub(crate) struct TurnProjection<'a> {
    request_id: JsonRpcId,
    emit: &'a mut dyn FnMut(Value),
    response_sent: bool,
    poisoned: Option<AppServerError>,
}

impl<'a> TurnProjection<'a> {
    pub fn new(request_id: JsonRpcId, emit: &'a mut dyn FnMut(Value)) -> Self {
        Self {
            request_id,
            emit,
            response_sent: false,
            poisoned: None,
        }
    }

    fn emit_value(&mut self, value: Value) {
        if self.poisoned.is_none() {
            (self.emit)(value);
        }
    }

    fn poison_or(&mut self, error: AppServerError) {
        if self.poisoned.is_none() {
            self.poisoned = Some(error);
        }
    }

    /// `turn/started` 之后紧随回写 turn/start 的响应：stdio 单连接下客户端
    /// 把该 response 之前的 notification 关联到本次请求（每个投影只回一次，
    /// 链式 followUp 的后续 turn 不再产生响应）。
    fn send_start_response(&mut self, turn: &ExecutionTurn) {
        if self.response_sent {
            return;
        }
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
        self.emit_value(JsonRpcMessage::response(self.request_id, value).to_wire_value());
    }
}

impl TurnEventSink for TurnProjection<'_> {
    fn emit(&mut self, event: TurnEvent) {
        self.emit_value(turn_event_notification(&event).to_wire_value());
        if let TurnEvent::TurnStarted { turn } = &event {
            self.send_start_response(turn);
        }
    }
}

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
