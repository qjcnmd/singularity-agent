#![forbid(unsafe_code)]

use serde_json::{Value, json};
use singularity_agent::AgentLoopBridge;
use singularity_core::ErrorCode;
use singularity_policy::{ApprovalDecision, ApprovalRequest};
use singularity_protocol::{
    AppEvent, InitializeParams, InitializeResult, ItemKind, JsonRpcMessage, Method,
    ThreadStartParams, ThreadStartResult, TraceEvent, TraceListParams, TraceShowParams,
    TurnStartParams, TurnStartResult,
};
use singularity_store::{SessionStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

pub type AppServerResult<T> = Result<T, AppServerError>;

pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
}

impl AppServer {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
        }
    }

    pub fn handle_json(&mut self, line: &str) -> AppServerResult<Vec<Value>> {
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        self.handle(message)
    }

    pub fn handle(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let id = message.id.clone();
        let Some(method) = message.method() else {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::invalid_request("Missing method"))
                    .to_wire_value(),
            ]);
        };

        if matches!(method, Method::Initialized) && !self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::not_initialized()).to_wire_value(),
            ]);
        }
        if !matches!(method, Method::Initialize | Method::Initialized)
            && !self.initialized_acknowledged
        {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::not_initialized()).to_wire_value(),
            ]);
        }

        match method {
            Method::Initialize => self.initialize(message),
            Method::Initialized => {
                self.initialized_acknowledged = true;
                Ok(Vec::new())
            }
            Method::ThreadStart => self.thread_start(message),
            Method::TurnStart => self.turn_start(message),
            Method::ApprovalRequest => self.approval_request(message),
            Method::ApprovalDecision => self.approval_decision(message),
            Method::TraceList => self.trace_list(message),
            Method::TraceShow => self.trace_show(message),
        }
    }

    fn initialize(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        if self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(message.id, ErrorCode::already_initialized()).to_wire_value(),
            ]);
        }
        let _params: InitializeParams = message.params_as()?;
        self.initialized = true;
        Ok(vec![
            JsonRpcMessage::response(message.id, serde_json::to_value(InitializeResult::local())?)
                .to_wire_value(),
        ])
    }

    fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = message.params_as()?;
        let thread = self
            .store
            .create_thread(params.model.as_deref(), params.cwd.as_deref())?;
        let trace = TraceEvent::new(
            format!("trace_{}", thread.thread_id),
            thread.thread_id.clone(),
            thread.thread_id.clone(),
            "app_server",
            "thread started",
        );
        self.store.append_trace(&trace)?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadStartResult { thread })?,
            )
            .to_wire_value(),
        ])
    }

    fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnStartParams = message.params_as()?;
        let bridge = AgentLoopBridge::not_migrated();
        let turn = self
            .store
            .create_turn(&params.thread_id, bridge.status.as_str())?;
        let payload = serde_json::to_value(&params.input)?;
        let item = self
            .store
            .append_item(&turn.turn_id, ItemKind::InputMessage, payload)?;
        let trace = TraceEvent::new(
            format!("trace_{}", turn.turn_id),
            params.thread_id,
            turn.turn_id.clone(),
            "app_server",
            "turn started without migrated AgentLoop",
        );
        self.store.append_trace(&trace)?;

        Ok(vec![
            JsonRpcMessage::response(message.id, serde_json::to_value(TurnStartResult { turn })?)
                .to_wire_value(),
            AppEvent::item_started(item.item_id.clone())
                .to_notification()
                .to_wire_value(),
            AppEvent::item_delta(item.item_id.clone(), "input accepted")
                .to_notification()
                .to_wire_value(),
            AppEvent::item_completed(item.item_id)
                .to_notification()
                .to_wire_value(),
        ])
    }

    fn approval_request(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let request: ApprovalRequest = message.params_as()?;
        self.store.create_approval(&request)?;
        let trace = TraceEvent::new(
            format!("trace_{}", request.request_id),
            request.request_id.clone(),
            request.session_id.clone(),
            "approval",
            "approval requested",
        );
        self.store.append_trace(&trace)?;
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"approval": request})).to_wire_value(),
        ])
    }

    fn approval_decision(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = message.params_as()?;
        if let Err(error) = self.store.record_approval_decision(
            &decision.request_id,
            decision.outcome.clone(),
            &decision.reason,
        ) {
            return match error {
                StoreError::NotFound(_) => Ok(vec![
                    JsonRpcMessage::error(
                        message.id,
                        ErrorCode::not_found("Pending approval not found"),
                    )
                    .to_wire_value(),
                ]),
                other => Err(other.into()),
            };
        }
        let trace = TraceEvent::new(
            format!("trace_{}", decision.decision_id),
            decision.request_id.clone(),
            decision.request_id.clone(),
            "approval",
            "approval decision recorded",
        );
        self.store.append_trace(&trace)?;
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"decision": decision})).to_wire_value(),
        ])
    }

    fn trace_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceListParams = message.params_as()?;
        match self.store.list_trace(&params.run_id) {
            Ok(events) => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"events": events})).to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => Ok(vec![
                JsonRpcMessage::error(message.id, ErrorCode::not_found("Trace run not found"))
                    .to_wire_value(),
            ]),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_show(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceShowParams = message.params_as()?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"event": event})).to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => Ok(vec![
                JsonRpcMessage::error(message.id, ErrorCode::not_found("Trace event not found"))
                    .to_wire_value(),
            ]),
            Err(error) => Err(error.into()),
        }
    }
}
