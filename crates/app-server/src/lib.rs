#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use singularity_agent::{
    AgentHostStatus, AgentLoop, AgentLoopCapability, AgentLoopInput, AgentLoopStatusBridge,
    PythonSidecarClient, PythonSidecarConfig, PythonSidecarStatus, sidecar_trace_summary,
};
use singularity_core::{ErrorCode, contains_sensitive_text};
use singularity_model::OpenAiProvider;
use singularity_policy::{
    ApprovalDecision, ApprovalRequest, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{
    AgentCapabilityResult, AgentHost, AppEvent, ApprovalCenterResult, ApprovalListResult,
    ArtifactFetchParams, ArtifactFetchResult, EventSubscribeParams, EventSubscribeResult,
    InitializeParams, InitializeResult, JsonRpcMessage, Method, ServerCapabilitiesResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadResult, ThreadStartParams, ThreadStartResult, TraceEvent, TraceListParams,
    TraceListResult, TraceShowParams, TraceTailParams, TransportCapability, Turn, TurnIdParams,
    TurnInterruptResult, TurnResult, TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_store::{ActiveSidecarRun, SessionStore, StoreError};
use singularity_tools::{ToolBroker, ToolRegistry, ToolSpec, WorkspaceTools};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_ALREADY_EXISTS: &str = "Approval already exists";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const TOOL_READ: &str = "builtin.read";
const TOOL_LIST: &str = "builtin.list";
const TOOL_GREP: &str = "builtin.grep";
const TOOL_EDIT: &str = "builtin.edit";
const TOOL_PATCH: &str = "builtin.patch";
const NATIVE_AGENT_LOOP_NOT_READY: &str = "Native AgentLoop is not production-ready";
static TRACE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    sidecar_runs: HashMap<String, SidecarRun>,
    shutdown_requested: bool,
}

struct SidecarRun {
    client: PythonSidecarClient,
    run_id: String,
}

impl AppServer {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
            python_sidecar: None,
            event_filter: None,
            sidecar_runs: HashMap::new(),
            shutdown_requested: false,
        }
    }

    pub fn with_python_sidecar(mut self, config: PythonSidecarConfig) -> Self {
        self.python_sidecar = Some(config);
        self
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
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
            Method::AgentCapability => self.agent_capability(message),
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
            Method::ServerShutdown => self.server_shutdown(message),
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
        self.cleanup_thread_sidecar_runs(&params.thread_id)?;
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
        if matches!(params.agent_host, Some(AgentHost::Native)) && !native_agent_loop_ready() {
            return invalid_request_response(message.id, NATIVE_AGENT_LOOP_NOT_READY);
        }
        let payload = serde_json::to_value(&params.input)?;
        let (turn, _item, _trace) = match self.store.create_turn_with_input_and_trace(
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
        if matches!(params.agent_host, Some(AgentHost::Native)) {
            let bridge = self.run_native_agent_loop(&thread, &params, &turn.turn_id)?;
            let turn = self.update_turn_from_bridge(turn, &bridge)?;
            self.append_native_trace(&params.thread_id, &turn.turn_id, &bridge)?;
            messages.extend(self.agent_terminal_item_events(&bridge, &turn)?);
            messages.push(
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TurnStartResult { turn: turn.clone() })?,
                )
                .to_wire_value(),
            );
            return Ok(messages);
        }

        let previous_session_id = self.previous_python_session_id(&params.thread_id);
        let bridge = self.run_python_sidecar_if_enabled(
            &turn.turn_id,
            &params,
            thread.model.as_deref(),
            previous_session_id.as_deref(),
        );
        if is_terminal_sidecar_status(&bridge.status) {
            self.append_sidecar_trace(&params.thread_id, &turn.turn_id, &bridge)?;
        }
        let turn = self.update_turn_from_bridge(turn, &bridge)?;

        messages.extend(self.agent_terminal_item_events(&bridge, &turn)?);
        messages.push(
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(TurnStartResult { turn: turn.clone() })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    fn agent_capability(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            AgentCapabilityResult {
                native_agent_loop: serde_json::to_value(AgentLoopCapability::current())?,
            },
        )
    }

    fn run_native_agent_loop(
        &self,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
    ) -> AppServerResult<AgentLoopStatusBridge> {
        let provider = match OpenAiProvider::from_env(|name| std::env::var(name).ok()) {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(AgentLoopStatusBridge::failed(error.message)
                    .with_status(AgentHostStatus::Failed));
            }
        };
        let registry = native_workspace_registry();
        let workspace_root = thread
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.trim().is_empty())
            .map(Into::into)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| ".".into());
        let workspace_root_display = workspace_root.to_string_lossy().into_owned();
        let policy = native_workspace_policy(workspace_root_display);
        let goal = params
            .input
            .iter()
            .map(|item| match item {
                singularity_protocol::InputItem::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let loop_input = AgentLoopInput::new(&params.thread_id, turn_id, goal)
            .with_model_name(thread.model.clone());
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(WorkspaceTools::new(workspace_root))
            .run(&loop_input);
        for request in &result.approval_requests {
            match self
                .store
                .create_approval_with_trace(request, "approval", "approval requested")
            {
                Ok(_) => {}
                Err(StoreError::AlreadyExists(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(result.bridge(&loop_input))
    }

    fn run_python_sidecar_if_enabled(
        &mut self,
        turn_id: &str,
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
                client
                    .resume_agent(session_id, &goal, model)
                    .map(|result| (client, result))
            } else {
                client
                    .run_agent(&goal, model)
                    .map(|result| (client, result))
            }
        });
        match result {
            Ok((client, result)) => {
                let bridge = AgentLoopStatusBridge::from_sidecar(result);
                if matches!(
                    bridge.status,
                    AgentHostStatus::Running | AgentHostStatus::CancelRequested
                ) && let (Some(run_id), Some(session_id), Some(task_id)) = (
                    bridge.run_id.as_deref(),
                    bridge.session_id.as_deref(),
                    bridge.task_id.as_deref(),
                ) {
                    if let Err(error) = self.store.register_active_sidecar_run(
                        turn_id,
                        run_id,
                        session_id,
                        task_id,
                        bridge.status.as_str(),
                    ) {
                        return AgentLoopStatusBridge::failed(error.to_string());
                    }
                    if let Err(error) = self.append_lifecycle_trace(
                        &params.thread_id,
                        turn_id,
                        run_id,
                        session_id,
                        task_id,
                        "sidecar_started",
                        bridge.status.as_str(),
                    ) {
                        let _ = self
                            .store
                            .clear_active_sidecar_run(turn_id, bridge.status.as_str());
                        return AgentLoopStatusBridge::failed(error.to_string());
                    }
                    self.sidecar_runs.insert(
                        turn_id.to_string(),
                        SidecarRun {
                            client,
                            run_id: run_id.to_string(),
                        },
                    );
                }
                bridge
            }
            Err(error) => AgentLoopStatusBridge::failed(error),
        }
    }

    fn update_turn_from_bridge(
        &self,
        turn: Turn,
        bridge: &AgentLoopStatusBridge,
    ) -> AppServerResult<Turn> {
        let status = match bridge.status {
            singularity_agent::AgentHostStatus::Completed => Some(TurnStatus::Completed),
            singularity_agent::AgentHostStatus::Blocked => Some(TurnStatus::Blocked),
            singularity_agent::AgentHostStatus::Failed => Some(TurnStatus::Failed),
            singularity_agent::AgentHostStatus::CancelRequested => Some(TurnStatus::Interrupted),
            singularity_agent::AgentHostStatus::Cancelled => Some(TurnStatus::Interrupted),
            singularity_agent::AgentHostStatus::Running => Some(TurnStatus::Running),
            singularity_agent::AgentHostStatus::NotMigrated => None,
        };
        if let Some(status) = status {
            return Ok(self.store.update_turn_state(
                &turn.turn_id,
                status,
                bridge.status.as_str(),
            )?);
        }
        Ok(turn)
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
        summary.payload = redact_sidecar_trace_summary(sidecar_trace_summary(bridge));
        self.store.append_trace(&summary)?;
        for event in &bridge.events {
            let mut translated = TraceEvent::new(
                sidecar_event_trace_id(turn_id, event),
                thread_id,
                turn_id,
                "python_sidecar",
                redact_app_server_text(&event.summary),
            );
            translated.event_type = safe_sidecar_event_type(&event.event_type);
            translated.severity = safe_sidecar_event_severity(&event.severity);
            translated.payload = serde_json::json!({
                "component": "python_sidecar",
                "sequence": event.sequence,
            });
            self.store.append_trace(&translated)?;
        }
        Ok(())
    }

    fn append_native_trace(
        &self,
        thread_id: &str,
        turn_id: &str,
        bridge: &AgentLoopStatusBridge,
    ) -> AppServerResult<()> {
        let mut event = TraceEvent::new(
            format!("trace_{turn_id}_native_agent_loop"),
            thread_id,
            turn_id,
            "agent_loop",
            "Rust native AgentLoop result translated",
        );
        event.payload = json!({
            "component": "agent_loop",
            "status": bridge.status.as_str(),
            "run_id": bridge.run_id,
            "session_id": bridge.session_id,
            "task_id": bridge.task_id,
            "model_turns": bridge.model_turns,
            "tool_calls": bridge.tool_calls,
            "approval_count": bridge.approval_count,
            "error": bridge.error.as_deref().map(redact_app_server_text),
        });
        self.store.append_trace(&event)?;
        Ok(())
    }

    fn turn_interrupt(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        if let Ok(active) = self.store.get_active_sidecar_run(&params.turn_id) {
            let Some(run) = self.sidecar_runs.get_mut(&params.turn_id) else {
                let turn = self.store.get_turn(&params.turn_id)?;
                return json_response(message.id, stale_active_run_interrupt_result(turn, &active));
            };
            let status = run.client.cancel(&active.run_id).map(sidecar_status_bridge);
            let status = match status {
                Ok(status) => status,
                Err(error) => {
                    return json_error(
                        message.id,
                        ErrorCode::new(
                            singularity_core::JSON_RPC_INTERNAL_ERROR,
                            redact_app_server_text(&error),
                        ),
                    );
                }
            };
            if !is_cancel_ack_status(&status.status) {
                return json_error(
                    message.id,
                    ErrorCode::new(
                        singularity_core::JSON_RPC_INTERNAL_ERROR,
                        format!(
                            "sidecar cancel was not accepted: {}",
                            status.status.as_str()
                        ),
                    ),
                );
            }
            let _ = self.store.register_active_sidecar_run(
                &params.turn_id,
                active.run_id.as_str(),
                active.session_id.as_str(),
                active.task_id.as_str(),
                status.status.as_str(),
            )?;
            let transition = if matches!(status.status, AgentHostStatus::CancelRequested) {
                "cancel_requested"
            } else {
                "interrupted"
            };
            self.append_lifecycle_trace(
                &active.thread_id,
                &params.turn_id,
                active.run_id.as_str(),
                active.session_id.as_str(),
                active.task_id.as_str(),
                transition,
                status.status.as_str(),
            )?;
            let turn = self.store.update_turn_state(
                &params.turn_id,
                TurnStatus::Interrupted,
                status.status.as_str(),
            )?;
            if is_terminal_sidecar_status(&status.status) {
                self.append_sidecar_trace(&active.thread_id, &params.turn_id, &status)?;
                let _ = self
                    .store
                    .clear_active_sidecar_run(&params.turn_id, status.status.as_str());
                self.sidecar_runs.remove(&params.turn_id);
            }
            return Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TurnInterruptResult {
                        turn_id: turn.turn_id,
                        status: "interrupted".to_string(),
                        agent_loop_status: Some(status.status.as_str().to_string()),
                    })?,
                )
                .to_wire_value(),
            ]);
        }
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) if is_terminal_turn_status(&turn.status) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TurnInterruptResult {
                        status: turn_status_str(&turn.status).to_string(),
                        turn_id: turn.turn_id,
                        agent_loop_status: Some(turn.agent_loop_status),
                    })?,
                )
                .to_wire_value(),
            ]),
            Ok(turn) => {
                let turn = self
                    .store
                    .update_turn_status(&turn.turn_id, TurnStatus::Interrupted)?;
                Ok(vec![
                    JsonRpcMessage::response(
                        message.id,
                        serde_json::to_value(TurnInterruptResult {
                            turn_id: turn.turn_id,
                            status: "interrupted".to_string(),
                            agent_loop_status: Some(turn.agent_loop_status),
                        })?,
                    )
                    .to_wire_value(),
                ])
            }
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn turn_status(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        if let Ok(active) = self.store.get_active_sidecar_run(&params.turn_id) {
            let Some(run) = self.sidecar_runs.get_mut(&params.turn_id) else {
                let turn = self.store.get_turn(&params.turn_id)?;
                return json_response(message.id, stale_active_run_status_result(turn, &active));
            };
            let bridge = match run.client.status(&run.run_id) {
                Ok(status) => sidecar_status_bridge(status),
                Err(error) => {
                    if is_cancelled_or_interrupted(active.status.as_str()) {
                        self.finalize_active_sidecar_run(
                            &params.turn_id,
                            &active,
                            TurnStatus::Interrupted,
                            "cancelled",
                            "interrupted",
                            true,
                        )?;
                        return json_response(
                            message.id,
                            TurnResult {
                                turn: self.store.get_turn(&params.turn_id)?,
                            },
                        );
                    }
                    let mut bridge = AgentLoopStatusBridge::failed(error);
                    bridge.run_id = Some(active.run_id.clone());
                    bridge.session_id = Some(active.session_id.clone());
                    bridge.task_id = Some(active.task_id.clone());
                    bridge
                }
            };
            let current_turn = self.store.get_turn(&params.turn_id)?;
            let current_turn_interrupted = current_turn.status == TurnStatus::Interrupted;
            let terminal_after_interrupt =
                current_turn_interrupted && is_terminal_sidecar_status(&bridge.status);
            let turn = if current_turn_interrupted {
                if terminal_after_interrupt {
                    self.store.update_turn_state(
                        &params.turn_id,
                        TurnStatus::Interrupted,
                        "cancelled",
                    )?
                } else {
                    let active_status = if is_cancelled_or_interrupted(active.status.as_str()) {
                        active.status.as_str()
                    } else {
                        bridge.status.as_str()
                    };
                    let _ = self.store.register_active_sidecar_run(
                        &params.turn_id,
                        active.run_id.as_str(),
                        active.session_id.as_str(),
                        active.task_id.as_str(),
                        active_status,
                    )?;
                    current_turn
                }
            } else {
                self.update_turn_from_bridge(current_turn, &bridge)?
            };
            if is_terminal_sidecar_status(&bridge.status) {
                if terminal_after_interrupt {
                    let cancelled_bridge = cancelled_sidecar_bridge(&active);
                    self.append_sidecar_trace(
                        &active.thread_id,
                        &params.turn_id,
                        &cancelled_bridge,
                    )?;
                } else {
                    self.append_sidecar_trace(&active.thread_id, &params.turn_id, &bridge)?;
                }
                let _ = self
                    .store
                    .clear_active_sidecar_run(&params.turn_id, turn.agent_loop_status.as_str());
                self.sidecar_runs.remove(&params.turn_id);
            } else {
                let active_status = if current_turn_interrupted
                    || is_cancelled_or_interrupted(active.status.as_str())
                {
                    active.status.as_str()
                } else {
                    bridge.status.as_str()
                };
                let _ = self.store.register_active_sidecar_run(
                    &params.turn_id,
                    active.run_id.as_str(),
                    active.session_id.as_str(),
                    active.task_id.as_str(),
                    active_status,
                )?;
            }
            let mut messages =
                self.agent_terminal_item_events(&bridge, &self.store.get_turn(&params.turn_id)?)?;
            messages.push(
                JsonRpcMessage::response(message.id, serde_json::to_value(TurnResult { turn })?)
                    .to_wire_value(),
            );
            return Ok(messages);
        }
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(message.id, TurnResult { turn }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn append_lifecycle_trace(
        &self,
        thread_id: &str,
        turn_id: &str,
        run_id: &str,
        session_id: &str,
        task_id: &str,
        transition: &str,
        status: &str,
    ) -> AppServerResult<()> {
        let mut event = TraceEvent::new(
            format!("trace_{turn_id}_{transition}_{}", short_trace_id()),
            thread_id,
            turn_id,
            "app_server",
            format!("turn lifecycle {transition}"),
        );
        event.task_id = Some(task_id.to_string());
        event.payload = json!({
            "turn_id": turn_id,
            "thread_id": thread_id,
            "run_id": run_id,
            "session_id": session_id,
            "task_id": task_id,
            "status": status,
            "transition": transition,
            "component": "app_server",
        });
        self.store.append_trace(&event)?;
        Ok(())
    }

    fn server_shutdown(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.cleanup_active_sidecar_runs()?;
        self.shutdown_requested = true;
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"shutdown": true})).to_wire_value(),
        ])
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

    fn agent_terminal_item_events(
        &self,
        bridge: &AgentLoopStatusBridge,
        turn: &Turn,
    ) -> AppServerResult<Vec<Value>> {
        let Some(agent_delta) = agent_completed_delta(bridge) else {
            return Ok(Vec::new());
        };
        let agent_item = self.store.append_item(
            &turn.turn_id,
            singularity_protocol::ItemKind::AgentMessage,
            json!({"delta": agent_delta}),
        )?;
        Ok([
            AppEvent::item_started(agent_item.item_id.clone()),
            AppEvent::item_agent_message_delta(agent_item.item_id.clone(), agent_delta),
            AppEvent::item_completed(agent_item.item_id),
        ]
        .into_iter()
        .filter_map(|event| self.event_notification(event))
        .collect())
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

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.cleanup_active_sidecar_runs();
    }
}

impl AppServer {
    fn cleanup_thread_sidecar_runs(&mut self, thread_id: &str) -> AppServerResult<()> {
        let turn_ids = self
            .store
            .list_active_sidecar_runs()?
            .into_iter()
            .filter(|active| active.thread_id == thread_id)
            .map(|active| active.turn_id)
            .collect::<Vec<_>>();
        for turn_id in turn_ids {
            if let Ok(active) = self.store.get_active_sidecar_run(&turn_id) {
                let (turn_status, agent_loop_status) = self.cleanup_status(&active);
                let transition = if turn_status == TurnStatus::Interrupted {
                    "interrupted"
                } else {
                    "cleanup"
                };
                self.finalize_active_sidecar_run(
                    &turn_id,
                    &active,
                    turn_status,
                    agent_loop_status,
                    transition,
                    false,
                )?;
            }
        }
        Ok(())
    }

    fn cleanup_active_sidecar_runs(&mut self) -> AppServerResult<()> {
        let turn_ids = self.sidecar_runs.keys().cloned().collect::<Vec<_>>();
        for turn_id in turn_ids {
            let Ok(active) = self.store.get_active_sidecar_run(&turn_id) else {
                continue;
            };
            let (turn_status, agent_loop_status) = self.cleanup_status(&active);
            let transition = if turn_status == TurnStatus::Interrupted {
                "interrupted"
            } else {
                "cleanup"
            };
            self.finalize_active_sidecar_run(
                &turn_id,
                &active,
                turn_status,
                agent_loop_status,
                transition,
                false,
            )?;
        }
        Ok(())
    }

    fn cleanup_status(&self, active: &ActiveSidecarRun) -> (TurnStatus, &'static str) {
        let interrupted = is_cancelled_or_interrupted(active.status.as_str())
            || self
                .store
                .get_turn(active.turn_id.as_str())
                .is_ok_and(|turn| turn.status == TurnStatus::Interrupted);
        if interrupted {
            (TurnStatus::Interrupted, "cancelled")
        } else {
            (TurnStatus::Failed, "failed")
        }
    }

    fn finalize_active_sidecar_run(
        &mut self,
        turn_id: &str,
        active: &ActiveSidecarRun,
        turn_status: TurnStatus,
        agent_loop_status: &str,
        transition: &str,
        append_sidecar_trace_event: bool,
    ) -> AppServerResult<()> {
        self.append_lifecycle_trace(
            &active.thread_id,
            turn_id,
            active.run_id.as_str(),
            active.session_id.as_str(),
            active.task_id.as_str(),
            transition,
            agent_loop_status,
        )?;
        self.store
            .update_turn_state(turn_id, turn_status, agent_loop_status)?;
        if append_sidecar_trace_event {
            let bridge = AgentLoopStatusBridge {
                status: AgentHostStatus::Cancelled,
                completed: false,
                final_answer: None,
                run_id: Some(active.run_id.clone()),
                session_id: Some(active.session_id.clone()),
                task_id: Some(active.task_id.clone()),
                model_turns: 0,
                tool_calls: 0,
                approval_count: 0,
                events: Vec::new(),
                trace_path: None,
                error: None,
            };
            self.append_sidecar_trace(&active.thread_id, turn_id, &bridge)?;
        }
        let _ = self
            .store
            .clear_active_sidecar_run(turn_id, agent_loop_status);
        self.sidecar_runs.remove(turn_id);
        Ok(())
    }
}

fn json_response<T: serde::Serialize>(id: Option<Value>, result: T) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

fn native_workspace_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for spec in native_workspace_tool_specs() {
        registry
            .register(spec)
            .expect("valid native workspace tool");
    }
    registry
}

fn native_agent_loop_ready() -> bool {
    let capability = AgentLoopCapability::current();
    capability.available && capability.missing_boundaries.is_empty()
}

fn native_workspace_policy(workspace_root: String) -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write(workspace_root))
        .with_rule(native_read_tool_rule())
}

fn native_read_tool_rule() -> PermissionRule {
    PermissionRule::new(
        "allow_native_read_tools",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Read)
}

fn native_workspace_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            TOOL_READ,
            "Read a workspace file through the native Rust tool boundary",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_chars": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_LIST,
            "List workspace directory entries through the native Rust tool boundary",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_entries": {"type": "integer", "minimum": 1}
                },
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_GREP,
            "Search workspace text files through the native Rust tool boundary",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"},
                    "max_matches": {"type": "integer", "minimum": 1}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_EDIT,
            "Replace expected text in a workspace file through the native Rust tool boundary",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected": {"type": "string"},
                    "replacement": {"type": "string"}
                },
                "required": ["path", "expected", "replacement"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_PATCH,
            "Apply explicit workspace file changes through the native Rust tool boundary",
            json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "expected": {"type": ["string", "null"]},
                                "replacement": {"type": "string"}
                            },
                            "required": ["path", "replacement"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["changes"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn json_error(id: Option<Value>, error: ErrorCode) -> AppServerResult<Vec<Value>> {
    Ok(vec![JsonRpcMessage::error(id, error).to_wire_value()])
}

fn stale_active_run_interrupt_result(turn: Turn, active: &ActiveSidecarRun) -> TurnInterruptResult {
    TurnInterruptResult {
        turn_id: turn.turn_id,
        status: turn_status_str(&turn.status).to_string(),
        agent_loop_status: Some(active.status.clone()),
    }
}

fn stale_active_run_status_result(mut turn: Turn, active: &ActiveSidecarRun) -> TurnResult {
    turn.agent_loop_status = active.status.clone();
    TurnResult { turn }
}

fn cancelled_sidecar_bridge(active: &ActiveSidecarRun) -> AgentLoopStatusBridge {
    AgentLoopStatusBridge {
        status: AgentHostStatus::Cancelled,
        completed: false,
        final_answer: None,
        run_id: Some(active.run_id.clone()),
        session_id: Some(active.session_id.clone()),
        task_id: Some(active.task_id.clone()),
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        events: Vec::new(),
        trace_path: None,
        error: None,
    }
}

fn sidecar_status_bridge(status: PythonSidecarStatus) -> AgentLoopStatusBridge {
    let host_status = AgentHostStatus::from(status.status.as_str());
    AgentLoopStatusBridge {
        status: host_status,
        completed: status.status == "completed",
        final_answer: status.final_answer,
        run_id: Some(status.run_id),
        session_id: status.session_id,
        task_id: status.task_id,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        events: status.events,
        trace_path: status.trace_path,
        error: None,
    }
}

fn is_terminal_sidecar_status(status: &AgentHostStatus) -> bool {
    matches!(
        status,
        AgentHostStatus::Completed
            | AgentHostStatus::Blocked
            | AgentHostStatus::Cancelled
            | AgentHostStatus::Failed
    )
}

fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    !matches!(status, TurnStatus::Running)
}

fn turn_status_str(status: &TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Blocked => "blocked",
        TurnStatus::Failed => "failed",
        TurnStatus::Interrupted => "interrupted",
    }
}

fn is_cancelled_or_interrupted(status: &str) -> bool {
    matches!(status, "cancel_requested" | "cancelled" | "canceled")
}

fn is_cancel_ack_status(status: &AgentHostStatus) -> bool {
    matches!(
        status,
        AgentHostStatus::CancelRequested | AgentHostStatus::Cancelled
    )
}

fn agent_completed_delta(bridge: &AgentLoopStatusBridge) -> Option<String> {
    if bridge.status == AgentHostStatus::Completed {
        bridge.final_answer.as_deref().map(redact_app_server_text)
    } else {
        None
    }
}

fn redact_app_server_text(text: &str) -> String {
    if contains_sensitive_text(text) {
        "[redacted sensitive app-server output]".to_string()
    } else {
        text.to_string()
    }
}

fn redact_sidecar_trace_summary(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        if let Some(Value::String(trace_path)) = object.get_mut("trace_path") {
            *trace_path = redact_app_server_text(trace_path);
        }
    }
    payload
}

fn sidecar_event_trace_id(turn_id: &str, event: &singularity_agent::SidecarRunEvent) -> String {
    let event_id = safe_trace_id_segment(&event.event_id);
    if event_id.is_empty() || contains_sensitive_text(&event.event_id) {
        format!("trace_{turn_id}_python_sidecar_{}", event.sequence)
    } else {
        format!("trace_{turn_id}_{event_id}")
    }
}

fn safe_trace_id_segment(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .collect()
}

fn safe_sidecar_event_type(event_type: &str) -> String {
    if event_type.trim().is_empty() || contains_sensitive_text(event_type) {
        "python_sidecar.event".to_string()
    } else {
        event_type.to_string()
    }
}

fn safe_sidecar_event_severity(severity: &str) -> String {
    match severity.to_ascii_lowercase().as_str() {
        "debug" | "info" | "warning" | "error" => severity.to_ascii_lowercase(),
        _ => "info".to_string(),
    }
}

fn short_trace_id() -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or_default();
    let sequence = TRACE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{micros:x}_{sequence:x}")
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
