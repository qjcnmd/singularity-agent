#![forbid(unsafe_code)]

use serde_json::{Value, json};
use singularity_agent::{
    AgentLoopStatusBridge, PythonSidecarClient, PythonSidecarConfig, sidecar_trace_summary,
};
use singularity_core::ErrorCode;
use singularity_policy::{ApprovalDecision, ApprovalRequest};
use singularity_protocol::{
    AppEvent, ApprovalCenterResult, ApprovalListResult, ArtifactFetchParams, ArtifactFetchResult,
    EventSubscribeParams, EventSubscribeResult, InitializeParams, InitializeResult, JsonRpcMessage,
    Method, ServerCapabilitiesResult, ThreadDeleteResult, ThreadForkParams, ThreadForkResult,
    ThreadIdParams, ThreadListResult, ThreadResult, ThreadStartParams, ThreadStartResult,
    TraceEvent, TraceListParams, TraceListResult, TraceShowParams, TraceTailParams,
    TransportCapability, Turn, TurnIdParams, TurnInterruptResult, TurnResult, TurnStartParams,
    TurnStartResult,
};
use singularity_store::{SessionStore, StoreError};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_ALREADY_EXISTS: &str = "Approval already exists";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";

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
    python_sidecar: Option<PythonSidecarConfig>,
    event_filter: Option<Vec<String>>,
}

impl AppServer {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
            python_sidecar: None,
            event_filter: None,
        }
    }

    pub fn with_python_sidecar(mut self, config: PythonSidecarConfig) -> Self {
        self.python_sidecar = Some(config);
        self
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
            Method::ServerCapabilities => self.server_capabilities(message),
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
            Method::ApprovalCenter => self.approval_center(message),
            Method::ApprovalRequest => self.approval_request(message),
            Method::ApprovalDecision => self.approval_decision(message),
            Method::EventSubscribe => self.event_subscribe(message),
            Method::ArtifactFetch => self.artifact_fetch(message),
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

    fn server_capabilities(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            ServerCapabilitiesResult {
                transports: vec![
                    TransportCapability {
                        transport: "stdio".to_string(),
                        available: true,
                        auth_token_required: false,
                    },
                    TransportCapability {
                        transport: "websocket".to_string(),
                        available: false,
                        auth_token_required: true,
                    },
                ],
            },
        )
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
        let mut messages = Vec::new();
        messages.extend(self.event_notification(AppEvent::thread_started(&thread)));
        messages.push(
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadStartResult {
                    thread: thread.clone(),
                })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
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
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let payload = serde_json::to_value(&params.input)?;
        let (turn, item, _trace) = match self.store.create_turn_with_input_and_trace(
            &params.thread_id,
            singularity_agent::AgentHostStatus::NotMigrated.as_str(),
            payload,
            "app_server",
            "turn started",
        ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };

        let mut messages = Vec::new();
        messages.extend(self.event_notification(AppEvent::turn_started(&turn)));

        let previous_session_id = self.previous_python_session_id(&params.thread_id);
        let bridge = self.run_python_sidecar_if_enabled(
            &params,
            thread.model.as_deref(),
            previous_session_id.as_deref(),
        );
        self.append_sidecar_trace(&params.thread_id, &turn.turn_id, &bridge)?;
        let turn = self.update_turn_from_bridge(turn, &bridge)?;

        if let Some(agent_delta) = bridge.final_answer.as_deref().or(bridge.error.as_deref()) {
            for event in [
                AppEvent::item_started(item.item_id.clone()),
                AppEvent::item_agent_message_delta(item.item_id.clone(), agent_delta),
                AppEvent::item_completed(item.item_id.clone()),
            ] {
                messages.extend(self.event_notification(event));
            }
        }
        messages.push(
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(TurnStartResult { turn: turn.clone() })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    fn run_python_sidecar_if_enabled(
        &self,
        params: &TurnStartParams,
        model: Option<&str>,
        previous_session_id: Option<&str>,
    ) -> AgentLoopStatusBridge {
        let Some(config) = &self.python_sidecar else {
            return AgentLoopStatusBridge::not_migrated();
        };
        let goal = params
            .input
            .iter()
            .map(|item| match item {
                singularity_protocol::InputItem::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let result = PythonSidecarClient::spawn(config).and_then(|mut client| {
            if let Some(session_id) = previous_session_id {
                client.resume_agent(session_id, &goal, model)
            } else {
                client.run_agent(&goal, model)
            }
        });
        match result {
            Ok(result) => AgentLoopStatusBridge::from_sidecar(result),
            Err(error) => AgentLoopStatusBridge::failed(error),
        }
    }

    fn update_turn_from_bridge(
        &self,
        turn: Turn,
        bridge: &AgentLoopStatusBridge,
    ) -> AppServerResult<Turn> {
        let status = match bridge.status {
            singularity_agent::AgentHostStatus::Completed => {
                Some(singularity_protocol::TurnStatus::Completed)
            }
            singularity_agent::AgentHostStatus::Blocked => {
                Some(singularity_protocol::TurnStatus::Blocked)
            }
            singularity_agent::AgentHostStatus::Failed => {
                Some(singularity_protocol::TurnStatus::Failed)
            }
            singularity_agent::AgentHostStatus::Cancelled => {
                Some(singularity_protocol::TurnStatus::Interrupted)
            }
            singularity_agent::AgentHostStatus::Running => {
                Some(singularity_protocol::TurnStatus::Running)
            }
            singularity_agent::AgentHostStatus::NotMigrated => None,
        };
        let Some(status) = status else {
            return Ok(turn);
        };
        self.store
            .update_turn_state(&turn.turn_id, status, bridge.status.as_str())
            .map_err(Into::into)
    }

    fn previous_python_session_id(&self, thread_id: &str) -> Option<String> {
        self.store
            .list_trace(thread_id)
            .ok()?
            .into_iter()
            .rev()
            .find_map(|event| {
                if event.component != "python_sidecar" {
                    return None;
                }
                event
                    .payload
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|session_id| !session_id.is_empty())
                    .map(str::to_string)
            })
    }

    fn append_sidecar_trace(
        &self,
        thread_id: &str,
        turn_id: &str,
        bridge: &AgentLoopStatusBridge,
    ) -> AppServerResult<()> {
        if matches!(
            bridge.status,
            singularity_agent::AgentHostStatus::NotMigrated
        ) {
            return Ok(());
        }
        let mut summary = TraceEvent::new(
            format!("trace_{turn_id}_python_sidecar"),
            thread_id,
            turn_id,
            "python_sidecar",
            "Python sidecar result translated",
        );
        summary.payload = sidecar_trace_summary(bridge);
        self.store.append_trace(&summary)?;
        for event in &bridge.events {
            let mut translated = TraceEvent::new(
                format!("trace_{turn_id}_{}", event.event_id),
                thread_id,
                turn_id,
                "python_sidecar",
                event.summary.clone(),
            );
            translated.event_type = event.event_type.clone();
            translated.severity = event.severity.clone();
            translated.payload = serde_json::json!({
                "component": event.component,
                "sequence": event.sequence,
            });
            self.store.append_trace(&translated)?;
        }
        Ok(())
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

    fn approval_center(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            ApprovalCenterResult {
                pending_approvals: self.store.list_pending_approvals()?,
                decisions: self.store.list_approval_decisions()?,
            },
        )
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
        if let Err(error) =
            self.store
                .record_approval_decision(&decision, "approval", "approval decision recorded")
        {
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

    fn event_subscribe(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EventSubscribeParams = message.params_as()?;
        self.event_filter = Some(params.event_types.clone());
        json_response(
            message.id,
            EventSubscribeResult {
                subscription_id: EVENT_SUBSCRIPTION_ID.to_string(),
                event_types: params.event_types,
            },
        )
    }

    fn artifact_fetch(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ArtifactFetchParams = message.params_as()?;
        match self.store.get_artifact_ref(&params.artifact_id) {
            Ok(artifact) => json_response(message.id, ArtifactFetchResult { artifact }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, ARTIFACT_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn event_notification(&self, event: AppEvent) -> Option<Value> {
        if self
            .event_filter
            .as_ref()
            .is_some_and(|event_types| !event_types.iter().any(|method| method == event.method()))
        {
            return None;
        }
        Some(event.to_notification().to_wire_value())
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
            .tail_trace(&params.run_id, params.limit.unwrap_or(50), params.offset)
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
