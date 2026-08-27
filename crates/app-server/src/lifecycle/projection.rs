//! runtime 事件的 JSON-RPC 投影。

use singularity_protocol::{
    AppEvent, ExecutionTurn, JsonRpcId, JsonRpcMessage, Turn, TurnStartResult,
};
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::{TurnFailureStage, TurnOutcome, TurnRunError};

use super::*;

pub(crate) fn wire_turn(turn: &ExecutionTurn) -> Turn {
    Turn::from(turn)
}

/// 单次 run_turn 调用的协议投影。
pub(crate) struct TurnProjection<'a> {
    server: &'a AppServer,
    request_id: JsonRpcId,
    emit: &'a mut dyn FnMut(Value),
    response_sent: bool,
    poisoned: Option<AppServerError>,
}

impl<'a> TurnProjection<'a> {
    pub fn new(
        server: &'a AppServer,
        request_id: JsonRpcId,
        emit: &'a mut dyn FnMut(Value),
    ) -> Self {
        Self {
            server,
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

    fn on_turn_started(&mut self, turn: &ExecutionTurn) {
        self.emit_notification(AppEvent::from_turn_event(&TurnEvent::TurnStarted {
            turn: turn.clone(),
        }));
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
            self.emit_value(JsonRpcMessage::response(self.request_id, value).to_wire_value());
        }
    }
}

impl TurnEventSink for TurnProjection<'_> {
    fn emit(&mut self, event: TurnEvent) {
        if let TurnEvent::TurnStarted { turn } = &event {
            self.on_turn_started(turn);
        } else {
            self.emit_notification(AppEvent::from_turn_event(&event));
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
