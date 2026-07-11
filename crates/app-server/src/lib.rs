#![forbid(unsafe_code)]

mod evaluation;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use singularity_agent::{
    AgentContextItem, AgentLoop, AgentLoopCapability, AgentLoopInput, AgentRunStatus, AgentStatus,
    ApprovalGrant, PendingToolCall,
};
use singularity_core::{
    CancellationToken, ErrorCode, ProjectInstructionError, contains_sensitive_text,
    load_project_instructions_from_cwd,
};
use singularity_model::{Provider, ProviderConfigSnapshot};
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionDecisionOutcome,
    PermissionOperation, PermissionProfile, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{
    AgentCapabilityResult, AppEvent, ApprovalCenterResult, ApprovalListResult, ArtifactFetchParams,
    ArtifactFetchResult, ConversationMessage, ConversationRole, EvalRunParams,
    EventSubscribeParams, EventSubscribeResult, InitializeParams, InitializeResult, Item,
    JsonRpcMessage, Method, ProviderReadiness, ServerCapabilitiesResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadReadParams, ThreadReadResult, ThreadResult, ThreadStartParams, ThreadStartResult,
    TraceEvent, TraceListParams, TraceListResult, TraceShowParams, TraceTailParams,
    TransportCapability, Turn, TurnIdParams, TurnInterruptResult, TurnResult, TurnStartParams,
    TurnStartResult, TurnStatus,
};
use singularity_store::{CommittedTurnOutcome, SessionStore, StoreError};
use singularity_tools::{
    CommandToolInput, SandboxBackend, ToolBroker, ToolRegistry, ToolSpec, WindowsSandboxBackend,
    WorkspaceTools, command_scope_digest,
};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const THREAD_ARCHIVED: &str = "Thread is archived; resume it before starting a turn";
const THREAD_ARCHIVED_CONTINUATION: &str =
    "Thread is archived; resume it before continuing the turn";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_REQUEST_INTERNAL_ONLY: &str =
    "approval/request is internal to the AgentLoop approval ledger";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const TOOL_READ: &str = "builtin.read";
const TOOL_LIST: &str = "builtin.list";
const TOOL_GREP: &str = "builtin.grep";
const TOOL_EDIT: &str = "builtin.edit";
const TOOL_PATCH: &str = "builtin.patch";
const TOOL_COMMAND: &str = "builtin.command";
const DEFAULT_THREAD_HISTORY_TURN_LIMIT: usize = 64;
const MAX_THREAD_HISTORY_TURN_LIMIT: usize = 256;
const TURN_CANCELLATION_POLL_MS: u64 = 25;

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("project instructions error: {0}")]
    ProjectInstructions(#[from] ProjectInstructionError),
    #[error("workspace error: {0}")]
    Workspace(String),
}

pub type AppServerResult<T> = Result<T, AppServerError>;

pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
    event_filter: Option<Vec<String>>,
    shutdown_requested: bool,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

struct ActiveTurnGuard {
    turn_id: String,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    monitor_stop: Arc<AtomicBool>,
    monitor: Option<JoinHandle<()>>,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        self.monitor_stop.store(true, Ordering::SeqCst);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
    }
}

impl AppServer {
    pub fn new(store: SessionStore, provider_snapshot: ProviderConfigSnapshot) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
            event_filter: None,
            shutdown_requested: false,
            sandbox_backend: Arc::new(WindowsSandboxBackend::new()),
            provider_snapshot,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_sandbox_backend(
        mut self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.sandbox_backend = Arc::new(sandbox_backend);
        self
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn ready_for_turn_worker(&self) -> bool {
        self.initialized_acknowledged
    }

    pub fn cancel_active_turns(&self) -> AppServerResult<()> {
        for cancellation in self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace("active turn registry poisoned".into()))?
            .values()
        {
            cancellation.cancel();
        }
        Ok(())
    }

    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: SessionStore::open(&self.store.descriptor().path)?,
            initialized: true,
            initialized_acknowledged: true,
            event_filter: self.event_filter.clone(),
            shutdown_requested: false,
            sandbox_backend: Arc::clone(&self.sandbox_backend),
            provider_snapshot: self.provider_snapshot.clone(),
            active_turns: Arc::clone(&self.active_turns),
        })
    }

    fn activate_turn(
        &self,
        turn_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let cancellation = CancellationToken::new();
        {
            let mut active_turns = self
                .active_turns
                .lock()
                .map_err(|_| AppServerError::Workspace("active turn registry poisoned".into()))?;
            if active_turns.contains_key(turn_id) {
                return Err(AppServerError::Workspace(format!(
                    "turn {turn_id} is already active"
                )));
            }
            active_turns.insert(turn_id.to_string(), cancellation.clone());
        }
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor = cancellation_monitor(
            &self.store.descriptor().path,
            turn_id,
            cancellation.clone(),
            Arc::clone(&monitor_stop),
        );
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
            monitor_stop,
            monitor,
        };
        Ok((cancellation, guard))
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

        let result = match method {
            Method::Initialize => self.initialize(message),
            Method::Initialized => {
                self.initialized_acknowledged = true;
                Ok(Vec::new())
            }
            Method::ServerCapabilities => self.server_capabilities(message),
            Method::ThreadList => self.thread_list(message),
            Method::ThreadRead => self.thread_read(message),
            Method::ThreadResume => self.thread_resume(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadFork => self.thread_fork(message),
            Method::ThreadArchive => self.thread_archive(message),
            Method::ThreadDelete => self.thread_delete(message),
            Method::TurnStart => self.turn_start(message),
            Method::EvalRun => self.eval_run(message),
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
        };
        match result {
            Err(AppServerError::InvalidParams(error)) => {
                json_error(id, ErrorCode::invalid_params(error))
            }
            result => result,
        }
    }

    fn initialize(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        if self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(message.id, ErrorCode::already_initialized()).to_wire_value(),
            ]);
        }
        let _params: InitializeParams = parse_params(&message)?;
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
        let params: ThreadReadParams = parse_params(&message)?;
        let turn_limit = match history_turn_limit(params.limit) {
            Ok(limit) => limit,
            Err(error) => return invalid_request_response(message.id, error),
        };
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        match self.store.read_thread_history(
            &params.thread_id,
            params.before_turn_sequence,
            turn_limit,
        ) {
            Ok(history) => json_response(
                message.id,
                ThreadReadResult {
                    thread,
                    messages: history.messages,
                    next_before_turn_sequence: history.next_before_turn_sequence,
                },
            ),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn thread_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = workspace_root(&thread) {
            return invalid_request_response(message.id, error);
        }
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Active,
        ) {
            Ok(thread) => json_response(message.id, ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }
    fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = parse_params(&message)?;
        let cwd = match canonical_thread_cwd(params.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => return invalid_request_response(message.id, error),
        };
        let (thread, _trace) = self.store.create_thread_with_trace(
            params.model.as_deref(),
            Some(&cwd),
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
        let params: ThreadForkParams = parse_params(&message)?;
        let source = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let source_cwd = match params.cwd.as_deref().or(source.cwd.as_deref()) {
            Some(cwd) => cwd,
            None => {
                return invalid_request_response(
                    message.id,
                    "source thread does not have an absolute workspace",
                );
            }
        };
        let cwd = match canonical_thread_cwd(Some(source_cwd)) {
            Ok(cwd) => cwd,
            Err(error) => return invalid_request_response(message.id, error),
        };
        let thread = self.store.create_thread(
            params.model.as_deref().or(source.model.as_deref()),
            Some(&cwd),
        )?;
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
        let params: ThreadIdParams = parse_params(&message)?;
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
        let params: ThreadIdParams = parse_params(&message)?;
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
        let mut messages = Vec::new();
        self.handle_turn_start_streaming(message, |message| messages.push(message))?;
        Ok(messages)
    }

    pub fn handle_turn_start_streaming(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        if message.method.as_deref() != Some(Method::TurnStart.as_str()) {
            return Err(AppServerError::InvalidParams(
                "streaming handler requires turn/start".to_string(),
            ));
        }
        let params: TurnStartParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                emit_messages(&mut emit, not_found_response(message.id, THREAD_NOT_FOUND)?);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if thread.status != singularity_protocol::ThreadStatus::Active {
            emit_messages(
                &mut emit,
                invalid_request_response(message.id, THREAD_ARCHIVED)?,
            );
            return Ok(());
        }
        if let Err(error) = workspace_root(&thread) {
            emit_messages(&mut emit, invalid_request_response(message.id, error)?);
            return Ok(());
        }
        let capability = AgentLoopCapability::current();
        if !agent_loop_capability_ready(&capability) {
            emit_messages(
                &mut emit,
                invalid_request_response(message.id, agent_loop_unavailable_message(&capability))?,
            );
            return Ok(());
        }
        let payload = serde_json::to_value(&params.input)?;
        let started = match self.store.create_turn_with_input_trace_and_history(
            &params.thread_id,
            AgentStatus::Running.as_str(),
            payload,
            "app_server",
            "turn started",
            DEFAULT_THREAD_HISTORY_TURN_LIMIT,
        ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                emit_messages(&mut emit, not_found_response(message.id, THREAD_NOT_FOUND)?);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let turn = started.turn;
        let (cancellation, _active_turn) = self.activate_turn(&turn.turn_id)?;
        if let Some(event) = self.event_notification(AppEvent::turn_started(&turn)) {
            emit(event);
        }
        let status = self.run_agent_loop(
            &thread,
            &params,
            &turn.turn_id,
            &started.history.messages,
            &cancellation,
        )?;
        let committed = self.commit_turn_run_status(turn, &status, &cancellation)?;
        let turn = committed.turn;
        emit_messages(
            &mut emit,
            self.agent_terminal_item_events(committed.assistant_item.as_ref())?,
        );
        if let Some(event) = self.event_notification(AppEvent::turn_completed(&turn)) {
            emit(event);
        }
        emit(
            JsonRpcMessage::response(message.id, serde_json::to_value(TurnStartResult { turn })?)
                .to_wire_value(),
        );
        Ok(())
    }

    fn agent_capability(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            AgentCapabilityResult {
                agent_loop: serde_json::to_value(AgentLoopCapability::current())?,
                provider_readiness: provider_readiness(&self.provider_snapshot),
            },
        )
    }

    fn eval_run(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EvalRunParams = parse_params(&message)?;
        match evaluation::run_evaluation(
            &params,
            Arc::clone(&self.sandbox_backend),
            &self.provider_snapshot,
        ) {
            Ok(result) => json_response(message.id, result),
            Err(error) => json_error(message.id, ErrorCode::invalid_request(error)),
        }
    }

    fn run_agent_loop(
        &self,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        history: &[ConversationMessage],
        cancellation: &CancellationToken,
    ) -> AppServerResult<AgentRunStatus> {
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(AgentRunStatus::failed(error.message).with_status(AgentStatus::Failed));
            }
        };
        match self.run_agent_loop_with_provider(
            provider,
            thread,
            params,
            turn_id,
            history,
            cancellation,
        ) {
            Err(AppServerError::ProjectInstructions(error)) => {
                Ok(AgentRunStatus::failed(error.to_string()).with_status(AgentStatus::Failed))
            }
            Err(AppServerError::Workspace(error)) => {
                Ok(AgentRunStatus::failed(error).with_status(AgentStatus::Failed))
            }
            result => result,
        }
    }

    fn resume_agent_loop(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        cancellation: &CancellationToken,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        if !agent_loop_ready() {
            return Ok(None);
        }
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        if pending_tool_call.is_none() {
            return Ok(None);
        }
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let turn_id = &request.turn_id;
                let turn = self.store.get_turn(turn_id)?;
                let run_status = approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    pending_tool_call.as_ref(),
                    AgentStatus::Failed,
                    "unavailable",
                    error.message,
                );
                return Ok(Some((turn, run_status)));
            }
        };
        self.resume_agent_loop_after_gate(
            request,
            decision,
            pending_tool_call,
            provider,
            cancellation,
        )
    }

    fn resume_agent_loop_after_gate<P>(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        provider: P,
        cancellation: &CancellationToken,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>>
    where
        P: Provider,
    {
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        let turn_id = &request.turn_id;
        let turn = self.store.get_turn(turn_id)?;
        if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            return Ok(None);
        }
        let Some(pending_tool_call) = pending_tool_call else {
            return Ok(None);
        };
        let pending = match serde_json::from_value::<PendingToolCall>(pending_tool_call.clone()) {
            Ok(pending) => pending,
            Err(error) => {
                let run_status = approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    format!("invalid pending tool call: {error}"),
                );
                return Ok(Some((turn, run_status)));
            }
        };
        if pending.request_id != request.request_id {
            let run_status = approval_terminal_status(
                &turn,
                request,
                decision,
                Some(&pending_tool_call),
                AgentStatus::Failed,
                "unavailable",
                "pending tool call request mismatch",
            );
            return Ok(Some((turn, run_status)));
        }
        let thread = self.store.get_thread(&turn.thread_id)?;
        if thread.status != singularity_protocol::ThreadStatus::Active {
            return Ok(None);
        }
        let workspace_root = match workspace_root(&thread) {
            Ok(workspace_root) => workspace_root,
            Err(error) => {
                let run_status = approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    error,
                );
                return Ok(Some((turn, run_status)));
            }
        };
        let user_input = self.store.get_turn_user_input(turn_id)?;
        let params = TurnStartParams {
            thread_id: turn.thread_id.clone(),
            input: serde_json::from_value(user_input)?,
        };
        let grant = ApprovalGrant::allow(
            request.request_id.clone(),
            request.action.clone(),
            request.resources.clone(),
        );
        let history = self.store.read_thread_history_before_turn(
            &thread.thread_id,
            turn_id,
            DEFAULT_THREAD_HISTORY_TURN_LIMIT,
        )?;
        let registry = workspace_tool_registry();
        let workspace_root_display = workspace_root.to_string_lossy().into_owned();
        let policy = workspace_policy(workspace_root_display);
        let loop_input = match agent_loop_input(
            &thread,
            &params,
            turn_id,
            &workspace_root,
            &history.messages,
        ) {
            Ok(input) => input.with_approval_grant(grant),
            Err(error) => {
                let run_status = approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    error.to_string(),
                );
                return Ok(Some((turn, run_status)));
            }
        };
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools(
                workspace_root,
                Arc::clone(&self.sandbox_backend),
            ))
            .with_cancellation_token(cancellation.clone())
            .resume_pending_tool_call(&loop_input, &pending);
        self.persist_agent_approval_requests(
            &result.approval_requests,
            &result.pending_tool_calls,
        )?;
        let mut run_status = result.to_run_status(&loop_input);
        if run_status.audit_events.is_empty() && pending.tool_name == TOOL_COMMAND {
            let audit_status = approval_terminal_status(
                &turn,
                request,
                decision,
                Some(&pending_tool_call),
                run_status.status.clone(),
                "unavailable",
                run_status
                    .error
                    .clone()
                    .unwrap_or_else(|| "approval resume did not execute command".to_string()),
            );
            run_status.audit_events = audit_status.audit_events;
        }
        Ok(Some((turn, run_status)))
    }

    fn run_agent_loop_with_provider<P>(
        &self,
        provider: P,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        history: &[ConversationMessage],
        cancellation: &CancellationToken,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider,
    {
        let registry = workspace_tool_registry();
        let workspace_root = workspace_root(thread).map_err(AppServerError::Workspace)?;
        let workspace_root_display = workspace_root.to_string_lossy().into_owned();
        let policy = workspace_policy(workspace_root_display);
        let loop_input = agent_loop_input(thread, params, turn_id, &workspace_root, history)?;
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools(
                workspace_root,
                Arc::clone(&self.sandbox_backend),
            ))
            .with_cancellation_token(cancellation.clone())
            .run(&loop_input);
        self.persist_agent_approval_requests(
            &result.approval_requests,
            &result.pending_tool_calls,
        )?;
        Ok(result.to_run_status(&loop_input))
    }

    fn persist_agent_approval_requests(
        &self,
        approval_requests: &[ApprovalRequest],
        pending_tool_calls: &[PendingToolCall],
    ) -> AppServerResult<()> {
        let pending_by_request = pending_tool_calls
            .iter()
            .map(|pending| (pending.request_id.as_str(), pending))
            .collect::<HashMap<_, _>>();
        for request in approval_requests {
            let pending_tool_call = pending_by_request
                .get(request.request_id.as_str())
                .map(|pending| serde_json::to_value(*pending))
                .transpose()?;
            match self.store.create_approval_with_pending_tool_call_and_trace(
                request,
                pending_tool_call,
                "approval",
                "approval requested",
            ) {
                Ok(_) => {}
                Err(StoreError::AlreadyExists(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn commit_turn_run_status(
        &self,
        turn: Turn,
        run_status: &AgentRunStatus,
        cancellation: &CancellationToken,
    ) -> AppServerResult<CommittedTurnOutcome> {
        let mut effective_status = run_status.clone();
        if cancellation.is_cancelled() {
            mark_run_cancelled(&mut effective_status);
        }
        match self.commit_effective_turn_status(&turn, &effective_status) {
            Ok(committed) => Ok(committed),
            Err(StoreError::InvalidState(message))
                if message == "cancel-requested turn can only finalize as interrupted" =>
            {
                mark_run_cancelled(&mut effective_status);
                self.commit_effective_turn_status(&turn, &effective_status)
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn commit_effective_turn_status(
        &self,
        turn: &Turn,
        run_status: &AgentRunStatus,
    ) -> Result<CommittedTurnOutcome, StoreError> {
        let assistant_delta = agent_completed_delta(run_status);
        let event = agent_loop_trace(turn, run_status);
        self.store.commit_turn_outcome(
            &turn.turn_id,
            turn_status_for_agent(&run_status.status),
            run_status.status.as_str(),
            assistant_delta.as_deref(),
            &event,
        )
    }

    fn turn_interrupt(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
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
                let thread_id = turn.thread_id.clone();
                if let Some(cancellation) = self
                    .active_turns
                    .lock()
                    .map_err(|_| AppServerError::Workspace("active turn registry poisoned".into()))?
                    .get(&turn.turn_id)
                    .cloned()
                {
                    cancellation.cancel();
                }
                let trace = TraceEvent {
                    payload: json!({
                        "turn_id": turn.turn_id,
                        "agent_loop_status": AgentStatus::CancelRequested.as_str(),
                    }),
                    ..TraceEvent::new(
                        format!("trace_{}_interrupt_requested", turn.turn_id),
                        thread_id,
                        turn.turn_id.clone(),
                        "app_server",
                        "turn interrupt requested",
                    )
                };
                let turn = self
                    .store
                    .request_turn_cancellation(&turn.turn_id, &trace)?;
                let status = if turn.agent_loop_status == AgentStatus::CancelRequested.as_str() {
                    AgentStatus::CancelRequested.as_str()
                } else {
                    turn_status_str(&turn.status)
                };
                Ok(vec![
                    JsonRpcMessage::response(
                        message.id,
                        serde_json::to_value(TurnInterruptResult {
                            turn_id: turn.turn_id,
                            status: status.to_string(),
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
        let params: TurnIdParams = parse_params(&message)?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(message.id, TurnResult { turn }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn server_shutdown(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.shutdown_requested = true;
        self.cancel_active_turns()?;
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
        let _request: ApprovalRequest = parse_params(&message)?;
        invalid_request_response(message.id, APPROVAL_REQUEST_INTERNAL_ONLY)
    }

    fn approval_decision(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = parse_params(&message)?;
        let pending_request = match self.store.get_pending_approval(&decision.request_id) {
            Ok(request) => request,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, PENDING_APPROVAL_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let pending_thread = self.store.get_thread(&pending_request.thread_id)?;
        if self
            .store
            .has_pending_tool_call(&pending_request.request_id)?
        {
            if pending_thread.status != singularity_protocol::ThreadStatus::Active {
                return invalid_request_response(message.id, THREAD_ARCHIVED_CONTINUATION);
            }
            if let Err(error) = workspace_root(&pending_thread) {
                return invalid_request_response(message.id, error);
            }
        }
        let recorded = match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(recorded) => recorded,
            Err(error) => {
                return match error {
                    StoreError::NotFound(_) => {
                        not_found_response(message.id, PENDING_APPROVAL_NOT_FOUND)
                    }
                    other => Err(other.into()),
                };
            }
        };
        let pending_tool_call = recorded.pending_tool_call.clone();
        let active_turn =
            if matches!(decision.outcome, ApprovalOutcome::Allow) && pending_tool_call.is_some() {
                Some(self.activate_turn(&recorded.request.turn_id)?)
            } else {
                None
            };
        let cancellation = active_turn
            .as_ref()
            .map(|(cancellation, _guard)| cancellation.clone())
            .unwrap_or_default();
        let resumed = self.resume_agent_loop(
            &recorded.request,
            &decision,
            pending_tool_call.clone(),
            &cancellation,
        )?;
        let mut messages = Vec::new();
        let terminal = if let Some(resumed) = resumed {
            Some(resumed)
        } else {
            self.approval_no_resume_status(
                &recorded.request,
                &decision,
                pending_tool_call.as_ref(),
            )?
        };
        if let Some((turn, run_status)) = terminal {
            let committed = self.commit_turn_run_status(turn, &run_status, &cancellation)?;
            messages.extend(self.agent_terminal_item_events(committed.assistant_item.as_ref())?);
            messages.extend(self.event_notification(AppEvent::turn_completed(&committed.turn)));
        }
        messages.push(
            JsonRpcMessage::response(message.id, json!({"decision": decision})).to_wire_value(),
        );
        Ok(messages)
    }

    fn approval_no_resume_status(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<&Value>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        let Ok(turn) = self.store.get_turn(&request.turn_id) else {
            return Ok(None);
        };
        if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            return Ok(None);
        }
        if pending_tool_call.is_none() {
            return Ok(None);
        }
        let (status, audit_decision, message) = match decision.outcome {
            ApprovalOutcome::Allow if pending_tool_call.is_some() => (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but agent loop turn could not resume",
            ),
            ApprovalOutcome::Allow => (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but pending tool call is unavailable",
            ),
            ApprovalOutcome::Deny => (AgentStatus::Failed, "denied", "approval denied"),
            ApprovalOutcome::Defer => (AgentStatus::Blocked, "deferred", "approval deferred"),
        };
        let run_status = approval_terminal_status(
            &turn,
            request,
            decision,
            pending_tool_call,
            status,
            audit_decision,
            message,
        );
        Ok(Some((turn, run_status)))
    }

    fn event_subscribe(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EventSubscribeParams = parse_params(&message)?;
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
        let params: ArtifactFetchParams = parse_params(&message)?;
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
        assistant_item: Option<&Item>,
    ) -> AppServerResult<Vec<Value>> {
        let Some(agent_item) = assistant_item else {
            return Ok(Vec::new());
        };
        let agent_delta = agent_item
            .payload
            .get("delta")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                StoreError::InvalidState(
                    "committed assistant item is missing its string delta".to_string(),
                )
            })?;
        Ok([
            AppEvent::item_started(agent_item.item_id.clone()),
            AppEvent::item_agent_message_delta(agent_item.item_id.clone(), agent_delta),
            AppEvent::item_completed(agent_item.item_id.clone()),
        ]
        .into_iter()
        .filter_map(|event| self.event_notification(event))
        .collect())
    }

    fn trace_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceListParams = parse_params(&message)?;
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
        let params: TraceShowParams = parse_params(&message)?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"event": event})).to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_EVENT_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_tail(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceTailParams = parse_params(&message)?;
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

fn emit_messages(emit: &mut impl FnMut(Value), messages: Vec<Value>) {
    for message in messages {
        emit(message);
    }
}

fn cancellation_monitor(
    store_path: &str,
    turn_id: &str,
    cancellation: CancellationToken,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    if store_path == ":memory:" {
        return None;
    }
    let store_path = store_path.to_string();
    let turn_id = turn_id.to_string();
    Some(std::thread::spawn(move || {
        let store = match SessionStore::open(store_path) {
            Ok(store) => store,
            Err(_) => {
                cancellation.cancel();
                return;
            }
        };
        while !stop.load(Ordering::SeqCst) && !cancellation.is_cancelled() {
            match store.get_turn(&turn_id) {
                Ok(turn) if turn.agent_loop_status == AgentStatus::CancelRequested.as_str() => {
                    cancellation.cancel();
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    cancellation.cancel();
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(TURN_CANCELLATION_POLL_MS));
        }
    }))
}

fn approval_terminal_status(
    turn: &Turn,
    request: &ApprovalRequest,
    decision: &ApprovalDecision,
    pending_tool_call: Option<&Value>,
    status: AgentStatus,
    audit_decision: &str,
    message: impl Into<String>,
) -> AgentRunStatus {
    let mut run_status = AgentRunStatus::failed(message).with_status(status);
    run_status.run_id = Some(turn.turn_id.clone());
    run_status.session_id = Some(request.thread_id.clone());
    run_status.task_id = Some(turn.turn_id.clone());
    run_status.approval_count = 1;
    let mut audit_event = json!({
        "approval_policy": ApprovalPolicy::OnRequest,
        "approval_decision": audit_decision,
        "approval_request_id": decision.request_id,
        "approval_decision_id": decision.decision_id,
        "command_provenance": "agent_requested",
    });
    if let Some(command_audit) = pending_command_audit_metadata(pending_tool_call) {
        merge_json_object(&mut audit_event, command_audit);
    }
    run_status.audit_events.push(audit_event);
    run_status
}

fn pending_command_audit_metadata(pending_tool_call: Option<&Value>) -> Option<Value> {
    let pending_value = pending_tool_call?;
    let (tool_name, raw_arguments) =
        match serde_json::from_value::<PendingToolCall>(pending_value.clone()) {
            Ok(pending) => (pending.tool_name, Some(pending.raw_arguments)),
            Err(_error) => (
                pending_value
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                pending_value
                    .get("raw_arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
        };
    if tool_name != TOOL_COMMAND {
        return None;
    }
    let mut audit = json!({
        "sandbox_backend": "not_executed",
        "sandbox_enforcement": "not_executed",
    });
    let Some(raw_arguments) = raw_arguments else {
        audit["cwd"] = json!("unknown");
        audit["timeout_seconds"] = json!("unknown");
        audit["sandbox_mode"] = json!("unknown");
        audit["network_access"] = json!("unknown");
        audit["command_scope_digest"] = json!("unavailable");
        return Some(audit);
    };
    let Ok(input) = serde_json::from_str::<CommandToolInput>(&raw_arguments) else {
        audit["cwd"] = json!("unknown");
        audit["timeout_seconds"] = json!("unknown");
        audit["sandbox_mode"] = json!("unknown");
        audit["network_access"] = json!("unknown");
        audit["command_scope_digest"] = json!("unavailable");
        return Some(audit);
    };
    let sandbox_mode = input.sandbox_mode();
    let network_access = input.network_access();
    merge_json_object(
        &mut audit,
        json!({
            "cwd": input.effective_cwd(),
            "timeout_seconds": input.effective_timeout_seconds(),
            "sandbox_mode": sandbox_mode,
            "network_access": network_access,
            "command_scope_digest": command_scope_digest(
                &input.argv,
                input.effective_cwd(),
                input.effective_timeout_seconds(),
                &sandbox_mode,
                &network_access,
            ),
        }),
    );
    Some(audit)
}

fn merge_json_object(target: &mut Value, source: Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn workspace_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for spec in workspace_tool_specs() {
        registry.register(spec).expect("valid workspace tool");
    }
    registry
}

fn history_turn_limit(limit: Option<u32>) -> Result<usize, String> {
    let limit = limit.unwrap_or(DEFAULT_THREAD_HISTORY_TURN_LIMIT as u32);
    if limit == 0 || limit > MAX_THREAD_HISTORY_TURN_LIMIT as u32 {
        return Err(format!(
            "thread history limit must be between 1 and {MAX_THREAD_HISTORY_TURN_LIMIT}"
        ));
    }
    usize::try_from(limit).map_err(|_| "thread history limit is unsupported".to_string())
}
fn canonical_thread_cwd(cwd: Option<&str>) -> Result<String, String> {
    let path = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => Path::new(cwd).to_path_buf(),
        Some(_) => return Err("thread cwd must not be empty".to_string()),
        None => std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    };
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize thread cwd: {error}"))?;
    if !canonical.is_dir() {
        return Err("thread cwd must be an existing directory".to_string());
    }
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())
}
fn workspace_root(thread: &Thread) -> Result<PathBuf, String> {
    let cwd = thread
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| "thread does not have an absolute workspace".to_string())?;
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err("thread does not have an absolute workspace".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize thread workspace: {error}"))?;
    if !canonical.is_dir() {
        return Err("thread workspace must be an existing directory".to_string());
    }
    Ok(canonical)
}

fn workspace_tools(
    workspace_root: PathBuf,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
) -> WorkspaceTools {
    WorkspaceTools::new(workspace_root).with_shared_sandbox_backend(sandbox_backend)
}

fn agent_loop_input(
    thread: &Thread,
    params: &TurnStartParams,
    turn_id: &str,
    cwd: &std::path::Path,
    history: &[ConversationMessage],
) -> Result<AgentLoopInput, ProjectInstructionError> {
    let goal = params
        .input
        .iter()
        .map(|item| match item {
            singularity_protocol::InputItem::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let history = history.iter().map(|message| match message.role {
        ConversationRole::User => {
            AgentContextItem::history_user(&message.item_id, &message.content)
        }
        ConversationRole::Assistant => {
            AgentContextItem::history_assistant(&message.item_id, &message.content)
        }
    });
    let mut input = AgentLoopInput::new(&params.thread_id, turn_id, goal)
        .with_history(history)
        .with_model_name(thread.model.clone());
    if let Some(instructions) = load_project_instructions_from_cwd(cwd)? {
        input = input.with_project_instructions(instructions.content);
    }
    Ok(input)
}

fn agent_loop_ready() -> bool {
    let capability = AgentLoopCapability::current();
    agent_loop_capability_ready(&capability)
}

fn provider_readiness(snapshot: &ProviderConfigSnapshot) -> ProviderReadiness {
    let config = snapshot.redacted_config();
    let readiness = snapshot.readiness();
    ProviderReadiness {
        source: snapshot.source().map(|source| source.as_str().to_string()),
        snapshot_id: snapshot.snapshot_id().to_string(),
        ready: readiness.ready,
        blocker: readiness
            .blocker
            .as_ref()
            .map(|blocker| blocker.code().to_string()),
        api_key_present: config.api_key_present,
        base_url_present: config.base_url_present,
        model_present: config.model_name.is_some(),
    }
}

fn agent_loop_capability_ready(capability: &AgentLoopCapability) -> bool {
    capability.available
        && capability.blockers.is_empty()
        && capability.status == AgentStatus::Completed
}

fn agent_loop_unavailable_message(capability: &AgentLoopCapability) -> String {
    let blockers = if capability.blockers.is_empty() {
        "none".to_string()
    } else {
        capability.blockers.join(",")
    };
    format!(
        "AgentLoop is not available: status={}; blockers={blockers}",
        capability.status.as_str()
    )
}
fn workspace_policy(workspace_root: String) -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write(workspace_root))
        .with_rule(workspace_read_tool_rule())
        .with_rule(sandbox_command_rule())
}

fn workspace_read_tool_rule() -> PermissionRule {
    PermissionRule::new(
        "allow_workspace_read_tools",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Read)
}

fn sandbox_command_rule() -> PermissionRule {
    PermissionRule::new(
        "allow_sandbox_commands",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Execute)
}

fn workspace_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            TOOL_READ,
            "Read a workspace file",
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
            "List workspace directory entries",
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
            "Search workspace text files",
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
            "Replace expected text in a workspace file",
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
            "Apply explicit workspace file changes",
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
        ToolSpec::new(
            TOOL_COMMAND,
            "Run a sandboxed command",
            json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1
                    },
                    "cwd": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1}
                },
                "required": ["argv"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn json_error(id: Option<Value>, error: ErrorCode) -> AppServerResult<Vec<Value>> {
    Ok(vec![JsonRpcMessage::error(id, error).to_wire_value()])
}

fn parse_params<T>(message: &JsonRpcMessage) -> Result<T, AppServerError>
where
    T: serde::de::DeserializeOwned,
{
    message
        .params_as()
        .map_err(|error| AppServerError::InvalidParams(error.to_string()))
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

fn turn_status_for_agent(status: &AgentStatus) -> TurnStatus {
    match status {
        AgentStatus::Completed => TurnStatus::Completed,
        AgentStatus::Blocked => TurnStatus::Blocked,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => TurnStatus::Interrupted,
        AgentStatus::Running => TurnStatus::Running,
        AgentStatus::Failed => TurnStatus::Failed,
    }
}

fn mark_run_cancelled(status: &mut AgentRunStatus) {
    status.status = AgentStatus::Cancelled;
    status.completed = false;
    status.final_answer = None;
    status.error = None;
}

fn agent_loop_trace(turn: &Turn, status: &AgentRunStatus) -> TraceEvent {
    let mut event = TraceEvent::new(
        format!("trace_{}_agent_loop", turn.turn_id),
        &turn.thread_id,
        &turn.turn_id,
        "agent_loop",
        "AgentLoop result translated",
    );
    event.payload = json!({
        "component": "agent_loop",
        "status": status.status.as_str(),
        "run_id": &status.run_id,
        "session_id": &status.session_id,
        "task_id": &status.task_id,
        "model_turns": status.model_turns,
        "tool_calls": status.tool_calls,
        "approval_count": status.approval_count,
        "audit_events": &status.audit_events,
        "verification": &status.verification,
        "error": status.error.as_deref().map(redact_app_server_text),
    });
    event
}

fn agent_completed_delta(run_status: &AgentRunStatus) -> Option<String> {
    if run_status.status == AgentStatus::Completed {
        run_status
            .final_answer
            .as_deref()
            .filter(|answer| !answer.trim().is_empty())
            .map(redact_app_server_text)
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

fn not_found_response(id: Option<Value>, message: &'static str) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::not_found(message)).to_wire_value(),
    ])
}

fn invalid_request_response(
    id: Option<Value>,
    message: impl Into<String>,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::invalid_request(message)).to_wire_value(),
    ])
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use singularity_agent::PendingToolCall;
    use singularity_model::{
        ModelCapabilities, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
        ModelTurnResponse, Provider, ProviderError,
    };
    use singularity_protocol::ItemKind;
    use singularity_tools::{CommandRequest, CommandResult};

    use super::*;

    fn app_server(store: SessionStore) -> AppServer {
        AppServer::new(store, ProviderConfigSnapshot::capture(|_| None))
    }

    #[test]
    fn duplicate_turn_activation_keeps_the_original_cancellation_token_registered() {
        let server = app_server(SessionStore::open(":memory:").expect("store"));
        let (original, _guard) = server.activate_turn("turn_1").expect("activate turn");

        let duplicate = server.activate_turn("turn_1");

        assert!(matches!(duplicate, Err(AppServerError::Workspace(_))));
        assert!(!original.is_cancelled());
        server.cancel_active_turns().expect("cancel active turn");
        assert!(original.is_cancelled());
    }

    struct StaticProvider {
        responses: Vec<ModelTurnResponse>,
        seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    }

    impl Provider for StaticProvider {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &singularity_core::CancellationToken,
        ) -> Result<ModelTurnResponse, ProviderError> {
            let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
            let response_index = seen_requests.len();
            seen_requests.push(request.clone());
            let mut response = self
                .responses
                .get(response_index)
                .unwrap_or_else(|| self.responses.last().expect("static provider response"))
                .clone();
            response.request_id = request.request_id.clone();
            Ok(response)
        }
    }

    #[test]
    fn agent_loop_loads_hierarchical_agents_md_from_thread_cwd() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp
            .path()
            .join("SINGULARITY_API_KEY=must-not-leak")
            .join("workspace");
        let cwd = workspace.join("crates").join("agent");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        std::fs::create_dir_all(&cwd).expect("nested cwd");
        std::fs::write(workspace.join("AGENTS.md"), "root instructions").expect("root agents");
        std::fs::write(
            workspace.join("crates").join("AGENTS.md"),
            "crate instructions",
        )
        .expect("crate agents");
        std::fs::write(cwd.join("AGENTS.md"), "agent instructions").expect("agent agents");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&cwd.to_string_lossy()))
            .expect("thread");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "user goal".to_string(),
            }],
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![ModelTurnResponse::completed(
                "model_request_turn_1_0",
                "response_1",
                "done",
            )],
            seen_requests: Arc::clone(&seen_requests),
        };
        let server = app_server(store);

        let status = server
            .run_agent_loop_with_provider(
                provider,
                &thread,
                &params,
                "turn_1",
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");

        assert_eq!(status.status, AgentStatus::Completed);
        let requests = seen_requests.lock().expect("seen requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.len(), 2);
        assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
        assert_eq!(requests[0].messages[1].role, ModelRole::User);
        let developer = requests[0].messages[0].content[0]
            .text
            .as_deref()
            .expect("developer instructions");
        assert!(developer.starts_with("You are a coding agent working in the current workspace."));
        assert!(developer.ends_with(
            "Project instructions:\nroot instructions\n\ncrate instructions\n\nagent instructions"
        ));
        assert_eq!(
            requests[0].messages[1].content[0].text.as_deref(),
            Some("user goal")
        );
        let hidden_workspace_marker = workspace.to_string_lossy();
        assert!(!requests[0].tools.iter().any(|tool| {
            serde_json::to_string(tool)
                .expect("serialize tool")
                .contains(hidden_workspace_marker.as_ref())
        }));
        assert!(
            !requests[0]
                .trace_metadata
                .to_string()
                .contains(hidden_workspace_marker.as_ref())
        );
        assert!(
            !serde_json::to_string(&status)
                .expect("serialize status")
                .contains(hidden_workspace_marker.as_ref())
        );
    }

    #[test]
    fn agent_loop_replays_only_completed_store_history_in_order() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        std::fs::write(workspace.join("AGENTS.md"), "project instructions")
            .expect("agents instructions");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");

        let (prior, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "previous user"}]),
                "app_server",
                "prior turn",
            )
            .expect("prior turn");
        store
            .append_item(
                &prior.turn_id,
                ItemKind::AgentMessage,
                json!({"delta": "previous assistant"}),
            )
            .expect("prior assistant");
        store
            .append_item(
                &prior.turn_id,
                ItemKind::Reasoning,
                json!({"summary": "private tool metadata must not replay"}),
            )
            .expect("private prior item");
        store
            .update_turn_state(
                &prior.turn_id,
                TurnStatus::Completed,
                AgentStatus::Completed.as_str(),
            )
            .expect("complete prior turn");

        let (failed, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "failed user must not replay"}]),
                "app_server",
                "failed turn",
            )
            .expect("failed turn");
        store
            .append_item(
                &failed.turn_id,
                ItemKind::AgentMessage,
                json!({"delta": "failed assistant must not replay"}),
            )
            .expect("failed assistant");
        store
            .update_turn_state(
                &failed.turn_id,
                TurnStatus::Failed,
                AgentStatus::Failed.as_str(),
            )
            .expect("fail turn");

        let (blocked, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "blocked user must not replay"}]),
                "app_server",
                "blocked turn",
            )
            .expect("blocked turn");
        store
            .update_turn_state(
                &blocked.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("block turn");

        let started = store
            .create_turn_with_input_trace_and_history(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "current user"}]),
                "app_server",
                "current turn",
                DEFAULT_THREAD_HISTORY_TURN_LIMIT,
            )
            .expect("current turn");
        assert_eq!(
            started
                .history
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["previous user", "previous assistant"]
        );

        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "current user".to_string(),
            }],
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![ModelTurnResponse::completed(
                "model_request_turn_1_0",
                "response",
                "done",
            )],
            seen_requests: Arc::clone(&seen_requests),
        };
        let server = app_server(store);

        let status = server
            .run_agent_loop_with_provider(
                provider,
                &thread,
                &params,
                &started.turn.turn_id,
                &started.history.messages,
                &CancellationToken::new(),
            )
            .expect("agent loop");

        assert_eq!(status.status, AgentStatus::Completed);
        let requests = seen_requests.lock().expect("seen requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .map(|message| message.role.clone())
                .collect::<Vec<_>>(),
            vec![
                ModelRole::Developer,
                ModelRole::User,
                ModelRole::Assistant,
                ModelRole::User,
            ]
        );
        let developer = requests[0].messages[0].content[0]
            .text
            .as_deref()
            .expect("developer instructions");
        assert!(developer.starts_with("You are a coding agent working in the current workspace."));
        assert!(developer.ends_with("Project instructions:\nproject instructions"));
        assert_eq!(
            requests[0].messages[1..]
                .iter()
                .map(|message| message.content[0].text.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("previous user"),
                Some("previous assistant"),
                Some("current user"),
            ]
        );
        let request_json = serde_json::to_string(&requests[0]).expect("request json");
        for forbidden in [
            "private tool metadata must not replay",
            "failed user must not replay",
            "failed assistant must not replay",
            "blocked user must not replay",
        ] {
            assert!(!request_json.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn sandbox_command_schema_does_not_expose_permission_expansion_fields() {
        let command = workspace_tool_specs()
            .into_iter()
            .find(|spec| spec.name == TOOL_COMMAND)
            .expect("command tool spec");
        let properties = command
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("command properties");

        assert!(!properties.contains_key("sandbox_mode"));
        assert!(!properties.contains_key("network_access"));
    }

    #[cfg(windows)]
    #[test]
    fn agent_loop_approval_resume_without_pending_tool_call_fails_closed_after_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let server = app_server(store);
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            TOOL_EDIT,
        )
        .with_tool_call_id("call_1")
        .with_resources(["README.md"]);
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![final_response],
            seen_requests: Arc::clone(&seen_requests),
        };

        let resumed = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                None,
                provider,
                &CancellationToken::new(),
            )
            .expect("resume");

        assert!(resumed.is_none());
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
        assert!(seen_requests.lock().expect("seen requests").is_empty());
    }

    #[test]
    fn agent_loop_approval_no_resume_status_records_session_and_command_audit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "run command"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            TOOL_COMMAND,
        )
        .with_tool_call_id("call_1");
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: json!({
                "argv": ["test-program", "success"],
                "sandbox_mode": "workspace_write",
                "network_access": "allowed"
            })
            .to_string(),
            resources: Vec::new(),
        };
        let pending_payload = serde_json::to_value(&pending).expect("pending payload");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let server = app_server(store);

        let (_turn, run_status) = server
            .approval_no_resume_status(&request, &decision, Some(&pending_payload))
            .expect("status")
            .expect("terminal status");

        assert_eq!(
            run_status.session_id.as_deref(),
            Some(thread.thread_id.as_str())
        );
        assert_eq!(
            run_status.audit_events[0]["sandbox_mode"],
            "workspace_write"
        );
        assert_eq!(run_status.audit_events[0]["network_access"], "allowed");
        assert_eq!(
            run_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert_eq!(
            run_status.audit_events[0]["sandbox_enforcement"],
            "not_executed"
        );
        assert!(
            run_status.audit_events[0]["command_scope_digest"]
                .as_str()
                .expect("command scope digest")
                .starts_with("sha256:")
        );
        assert_eq!(
            run_status.audit_events[0]["approval_decision"],
            "unavailable"
        );
    }

    #[test]
    fn agent_loop_approval_resume_failures_record_command_audit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "run command"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            TOOL_COMMAND,
        )
        .with_tool_call_id("call_1");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let valid_command = json!({
            "argv": ["test-program", "success"],
            "sandbox_mode": "workspace_write",
            "network_access": "allowed"
        })
        .to_string();
        let mismatched_pending = PendingToolCall {
            request_id: "approval_other_call_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: valid_command.clone(),
            resources: Vec::new(),
        };
        let invalid_args_pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: "{not-json".to_string(),
            resources: Vec::new(),
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let server = app_server(store);

        let (_turn, mismatch_status) = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&mismatched_pending).expect("pending payload")),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "done",
                    )],
                    seen_requests: Arc::clone(&seen_requests),
                },
                &CancellationToken::new(),
            )
            .expect("resume")
            .expect("terminal status");
        assert_eq!(mismatch_status.status, AgentStatus::Failed);
        assert_eq!(
            mismatch_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert!(
            mismatch_status.audit_events[0]["command_scope_digest"]
                .as_str()
                .expect("command scope digest")
                .starts_with("sha256:")
        );

        let (_turn, invalid_args_status) = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&invalid_args_pending).expect("pending payload")),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "done",
                    )],
                    seen_requests: Arc::clone(&seen_requests),
                },
                &CancellationToken::new(),
            )
            .expect("resume")
            .expect("terminal status");
        assert_eq!(invalid_args_status.status, AgentStatus::Failed);
        assert_eq!(
            invalid_args_status.audit_events[0]["approval_decision"],
            "unavailable"
        );
        assert_eq!(
            invalid_args_status.audit_events[0]["sandbox_enforcement"],
            "not_executed"
        );
        assert_eq!(
            invalid_args_status.audit_events[0]["command_scope_digest"],
            "unavailable"
        );
        assert!(seen_requests.lock().expect("seen requests").is_empty());
    }

    #[test]
    fn agent_loop_approval_resume_uses_stored_pending_tool_call_after_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (prior, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "previous approval user"}]),
                "app_server",
                "prior turn",
            )
            .expect("prior turn");
        store
            .append_item(
                &prior.turn_id,
                ItemKind::AgentMessage,
                json!({"delta": "previous approval assistant"}),
            )
            .expect("prior assistant");
        store
            .update_turn_state(
                &prior.turn_id,
                TurnStatus::Completed,
                AgentStatus::Completed.as_str(),
            )
            .expect("complete prior turn");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let (future, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "future user must not replay"}]),
                "app_server",
                "future turn",
            )
            .expect("future turn");
        store
            .append_item(
                &future.turn_id,
                ItemKind::AgentMessage,
                json!({"delta": "future assistant must not replay"}),
            )
            .expect("future assistant");
        store
            .update_turn_state(
                &future.turn_id,
                TurnStatus::Completed,
                AgentStatus::Completed.as_str(),
            )
            .expect("complete future turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            TOOL_EDIT,
        )
        .with_tool_call_id("call_1")
        .with_resources(["README.md"]);
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_EDIT.to_string(),
            raw_arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            })
            .to_string(),
            resources: vec!["README.md".to_string()],
        };
        let pending_payload = serde_json::to_value(&pending).expect("pending payload");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let mut verification_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_2", "");
        verification_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_verify".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            arguments: json!({
                "argv": ["cmd.exe", "/C", "echo verified"],
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "argv": ["cmd.exe", "/C", "echo verified"],
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_3", "done");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![verification_response, final_response],
            seen_requests: Arc::clone(&seen_requests),
        };
        let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

        let resumed = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(pending_payload),
                provider,
                &CancellationToken::new(),
            )
            .expect("resume")
            .expect("resumed");

        assert_eq!(resumed.0.turn_id, turn.turn_id);
        assert_eq!(resumed.1.status, AgentStatus::Completed);
        assert_eq!(resumed.1.final_answer.as_deref(), Some("done"));
        assert!(resumed.1.verification.required);
        assert!(resumed.1.verification.passed);
        assert_eq!(resumed.1.verification.successful_command_count, 1);
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "after"
        );
        let requests = seen_requests.lock().expect("seen requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
        assert_eq!(requests[0].messages[1].role, ModelRole::User);
        assert_eq!(
            requests[0].messages[1].content[0].text.as_deref(),
            Some("previous approval user")
        );
        assert_eq!(requests[0].messages[2].role, ModelRole::Assistant);
        assert_eq!(
            requests[0].messages[2].content[0].text.as_deref(),
            Some("previous approval assistant")
        );
        assert_eq!(requests[0].messages[3].role, ModelRole::User);
        assert_eq!(
            requests[0].messages[3].content[0].text.as_deref(),
            Some("edit readme")
        );
        let request_json = serde_json::to_string(&requests[0]).expect("request json");
        assert!(!request_json.contains("future user must not replay"));
        assert!(!request_json.contains("future assistant must not replay"));
    }

    #[test]
    fn archived_thread_cannot_resume_a_pending_approval_tool() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        store
            .update_thread_status(
                &thread.thread_id,
                singularity_protocol::ThreadStatus::Archived,
            )
            .expect("archive thread");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            TOOL_EDIT,
        )
        .with_tool_call_id("call_1")
        .with_resources(["README.md"]);
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_EDIT.to_string(),
            raw_arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            })
            .to_string(),
            resources: vec!["README.md".to_string()],
        };
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![ModelTurnResponse::completed("request", "response", "done")],
            seen_requests: Arc::clone(&seen_requests),
        };
        let server = app_server(store);

        let resumed = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(pending).expect("pending payload")),
                provider,
                &CancellationToken::new(),
            )
            .expect("resume check");

        assert!(resumed.is_none());
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
        assert!(seen_requests.lock().expect("seen requests").is_empty());
    }

    struct CompletedSandboxBackend;

    impl SandboxBackend for CompletedSandboxBackend {
        fn name(&self) -> &'static str {
            "completed_test"
        }

        fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
            singularity_tools::SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }
    }

    #[test]
    fn agent_loop_command_uses_bound_sandbox_backend_without_approval() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "run command".to_string(),
            }],
        };
        let mut command_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
        command_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: "builtin.command".to_string(),
            arguments: json!({
                "argv": ["cmd.exe", "/C", "echo app-server-sandbox-ok"],
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "argv": ["cmd.exe", "/C", "echo app-server-sandbox-ok"],
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
        let provider = StaticProvider {
            responses: vec![command_response, final_response],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        };
        let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

        let status = server
            .run_agent_loop_with_provider(
                provider,
                &thread,
                &params,
                "turn_1",
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");

        assert_eq!(status.status, AgentStatus::Completed);
        assert_eq!(status.final_answer.as_deref(), Some("done"));
        assert_eq!(status.tool_calls, 1);
        assert_eq!(status.approval_count, 0);
    }
}
