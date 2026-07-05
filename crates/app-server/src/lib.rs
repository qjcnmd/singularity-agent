#![forbid(unsafe_code)]

use serde_json::{Value, json};
use singularity_agent::AgentLoopBridge;
use singularity_core::ErrorCode;
use singularity_policy::{ApprovalDecision, ApprovalRequest};
use singularity_protocol::{
    AppEvent, ApprovalListResult, InitializeParams, InitializeResult, JsonRpcMessage, Method,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadResult, ThreadStartParams, ThreadStartResult, TraceListParams, TraceListResult,
    TraceShowParams, TraceTailParams, TurnIdParams, TurnInterruptResult, TurnResult,
    TurnStartParams, TurnStartResult,
};
use singularity_store::{SessionStore, StoreError};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_ALREADY_EXISTS: &str = "Approval already exists";

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
        let Some(method_name) = message.method.as_deref() else {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::invalid_request("Missing method"))
                    .to_wire_value(),
            ]);
        };
        let Some(method) = Method::parse(method_name) else {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::method_not_found(method_name)).to_wire_value(),
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
            Method::ThreadList => self.thread_list(message),
            Method::ThreadRead | Method::ThreadResume => self.thread_read(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadFork => self.thread_fork(message),
            Method::ThreadArchive => self.thread_archive(message),
            Method::ThreadDelete => self.thread_delete(message),
            Method::TurnStart => self.turn_start(message),
            Method::TurnInterrupt => self.turn_interrupt(message),
            Method::TurnStatus => self.turn_status(message),
            Method::ApprovalList => self.approval_list(message),
            Method::ApprovalRequest => self.approval_request(message),
            Method::ApprovalDecision => self.approval_decision(message),
            Method::TraceList => self.trace_list(message),
            Method::TraceShow => self.trace_show(message),
            Method::TraceTail => self.trace_tail(message),
            Method::ServerShutdown => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"shutdown": true})).to_wire_value(),
            ]),
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

    fn thread_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let threads = self.store.list_threads()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    fn thread_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.get_thread(&params.thread_id) {
            Ok(thread) => json_response(message.id, ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = message.params_as()?;
        let (thread, _trace) = self.store.create_thread_with_trace(
            params.model.as_deref(),
            params.cwd.as_deref(),
            "app_server",
            "thread started",
        )?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadStartResult {
                    thread: thread.clone(),
                })?,
            )
            .to_wire_value(),
            AppEvent::thread_started(&thread)
                .to_notification()
                .to_wire_value(),
        ])
    }

    fn thread_fork(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadForkParams = message.params_as()?;
        if let Err(error) = self.store.get_thread(&params.thread_id) {
            return match error {
                StoreError::NotFound(_) => not_found_response(message.id, THREAD_NOT_FOUND),
                other => Err(other.into()),
            };
        }
        let thread = self
            .store
            .create_thread(params.model.as_deref(), params.cwd.as_deref())?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadForkResult {
                    source_thread_id: params.thread_id,
                    thread,
                })?,
            )
            .to_wire_value(),
        ])
    }

    fn thread_archive(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Archived,
        ) {
            Ok(thread) => json_response(message.id, ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn thread_delete(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.delete_thread(&params.thread_id) {
            Ok(()) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(ThreadDeleteResult {
                        thread_id: params.thread_id,
                        deleted: true,
                    })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnStartParams = message.params_as()?;
        let bridge = AgentLoopBridge::not_migrated();
        let payload = serde_json::to_value(&params.input)?;
        let (turn, item, _trace) = match self.store.create_turn_with_input_and_trace(
            &params.thread_id,
            bridge.status.as_str(),
            payload,
            "app_server",
            "turn started without migrated AgentLoop",
        ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };

        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(TurnStartResult { turn: turn.clone() })?,
            )
            .to_wire_value(),
            AppEvent::turn_started(&turn)
                .to_notification()
                .to_wire_value(),
            AppEvent::item_started(item.item_id.clone())
                .to_notification()
                .to_wire_value(),
            AppEvent::item_agent_message_delta(item.item_id.clone(), "input accepted")
                .to_notification()
                .to_wire_value(),
            AppEvent::item_completed(item.item_id)
                .to_notification()
                .to_wire_value(),
        ])
    }

    fn turn_interrupt(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        match self.store.update_turn_status(
            &params.turn_id,
            singularity_protocol::TurnStatus::Interrupted,
        ) {
            Ok(turn) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TurnInterruptResult {
                        turn_id: turn.turn_id,
                        status: "interrupted".to_string(),
                    })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn turn_status(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(message.id, TurnResult { turn }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn approval_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let approvals = self.store.list_pending_approvals()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ApprovalListResult { approvals })?,
            )
            .to_wire_value(),
        ])
    }

    fn approval_request(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let request: ApprovalRequest = message.params_as()?;
        if let Err(error) =
            self.store
                .create_approval_with_trace(&request, "approval", "approval requested")
        {
            return match error {
                StoreError::AlreadyExists(_) => {
                    invalid_request_response(message.id, APPROVAL_ALREADY_EXISTS)
                }
                other => Err(other.into()),
            };
        }
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"approval": request})).to_wire_value(),
        ])
    }

    fn approval_decision(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = message.params_as()?;
        if let Err(error) = self.store.record_approval_decision_with_trace(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            return match error {
                StoreError::NotFound(_) => {
                    not_found_response(message.id, PENDING_APPROVAL_NOT_FOUND)
                }
                other => Err(other.into()),
            };
        }
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"decision": decision})).to_wire_value(),
        ])
    }

    fn trace_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceListParams = message.params_as()?;
        match self
            .store
            .list_trace_page(&params.run_id, params.limit, params.offset)
        {
            Ok(events) => json_response(message.id, TraceListResult { events }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_RUN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_show(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceShowParams = message.params_as()?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"event": event})).to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_EVENT_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_tail(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceTailParams = message.params_as()?;
        match self
            .store
            .tail_trace(&params.run_id, params.limit.unwrap_or(50))
        {
            Ok(events) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TraceListResult { events })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_RUN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }
}

fn json_response<T: serde::Serialize>(id: Option<Value>, result: T) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

fn not_found_response(id: Option<Value>, message: &'static str) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::not_found(message)).to_wire_value(),
    ])
}

fn invalid_request_response(
    id: Option<Value>,
    message: &'static str,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::invalid_request(message)).to_wire_value(),
    ])
}
