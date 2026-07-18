#![forbid(unsafe_code)]

//! 在进程边界负责 turn 准入、`AgentLoop` 执行、持久化和取消的 JSON-RPC 应用服务。
//!
//! 服务将协议处理与工作线程执行分离，并通过 `SessionStore` 提交终态后再发出对应事件。

mod evaluation;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{Builder as ThreadBuilder, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use singularity_agent::{
    AgentContextItem, AgentLoop, AgentLoopCapability, AgentLoopInput, AgentLoopResult,
    AgentRunStatus, AgentStatus, ApprovalGrant, PendingToolCall, agent_control_tool_entries,
    project_audit_event,
};
use singularity_core::{
    CancellationToken, ErrorCode, ProjectInstructionError, contains_sensitive_text,
    load_project_instructions,
};
use singularity_model::{Provider, ProviderConfigSnapshot};
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionDecisionOutcome,
    PermissionOperation, PermissionProfile, PermissionProfileName, PermissionResource,
    PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{
    AgentCapabilityResult, AgentLoopCapabilityStatus, AppEvent, ApprovalCenterResult,
    ApprovalDecisionResult, ApprovalListResult, ArtifactFetchParams, ArtifactFetchResult,
    ConversationMessage, ConversationRole, EvalRunParams, EventSubscribeParams,
    EventSubscribeResult, InitializeParams, InitializeResult, Item, JsonRpcId, JsonRpcMessage,
    Method, ProviderConfigurationStatus, ServerCapabilitiesResult, ServerShutdownResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadReadParams, ThreadReadResult, ThreadResult, ThreadStartParams, ThreadStartResult,
    TraceEvent, TraceListParams, TraceListResult, TraceShowParams, TraceShowResult,
    TraceTailParams, TransportCapability, Turn, TurnIdParams, TurnInterruptResult, TurnResult,
    TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_sandbox::{SandboxBackend, SandboxBackendEnforcement, WindowsSandboxBackend};
use singularity_store::{
    CommitTurnOutcomeParams, CommittedTurnOutcome, CreateStartedTurnParams, SessionStore,
    StoreError,
};
use singularity_tools::{
    COMMAND_TOOL as TOOL_COMMAND, EDIT_TOOL as TOOL_EDIT, GREP_TOOL as TOOL_GREP,
    LIST_TOOL as TOOL_LIST, PATCH_TOOL as TOOL_PATCH, READ_TOOL as TOOL_READ, ToolBroker,
    ToolRegistry, WorkspaceTools, workspace_tool_entries,
};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const THREAD_ARCHIVED: &str = "Thread is archived; resume it before starting a turn";
const THREAD_ARCHIVED_CONTINUATION: &str =
    "Thread is archived; resume it before continuing the turn";
const THREAD_EXECUTION_ACTIVE: &str = "Thread already has an active or pending turn";
const WORKSPACE_EXECUTION_ACTIVE: &str = "Workspace already has an active or pending turn";
const EXECUTION_STOPPED: &str = "AppServer is stopping; execution was not started";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_CHECKPOINT_REQUIRED: &str =
    "approval request requires an internal AgentLoop checkpoint";
const APPROVAL_REQUEST_INTERNAL_ONLY: &str =
    "approval/request is internal to the AgentLoop approval history";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const DEFAULT_THREAD_HISTORY_TURN_LIMIT: usize = 64;
const MAX_THREAD_HISTORY_TURN_LIMIT: usize = 256;
const TURN_CANCELLATION_POLL_MS: u64 = 25;
const TURN_MONITOR_SHUTDOWN_WAIT_MS: u64 = 100;
const STRICT_COMMAND_SANDBOX_UNAVAILABLE: &str = "strict_command_sandbox_unavailable";
const APP_ERROR_INVALID_STATE: i64 = -32005;

/// 在应用边界转换为 JSON-RPC 响应的错误。
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

/// `AppServer` 请求处理和生命周期操作使用的结果类型。
pub type AppServerResult<T> = Result<T, AppServerError>;
type ApprovalCheckpoint = (ApprovalRequest, Value);

struct AgentLoopInvocation<'a> {
    thread: &'a Thread,
    params: &'a TurnStartParams,
    turn_id: &'a str,
    history: &'a [ConversationMessage],
    cancellation: &'a CancellationToken,
}

/// 协调线程、turn、approval、追踪和工作线程的有状态 JSON-RPC 服务。
pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
    event_filter: Option<Vec<String>>,
    shutdown_requested: bool,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    execution_stopped: Arc<AtomicBool>,
}

/// 由请求工作线程与标准输入输出传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    execution_stopped: Arc<AtomicBool>,
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并将取消传播到每个活动 turn。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
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
}

struct ActiveTurnGuard {
    turn_id: String,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    monitor: Option<CancellationMonitor>,
}

struct CancellationMonitorControl {
    started: AtomicBool,
    stop: AtomicBool,
    wake: Sender<()>,
}

struct CancellationMonitor {
    control: Arc<CancellationMonitorControl>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl ActiveTurnGuard {
    fn start_monitor(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.control.started.store(true, Ordering::SeqCst);
            let _ = monitor.control.wake.send(());
        }
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Some(mut monitor) = self.monitor.take() {
            monitor.control.stop.store(true, Ordering::SeqCst);
            let _ = monitor.control.wake.send(());
            match monitor
                .done
                .recv_timeout(Duration::from_millis(TURN_MONITOR_SHUTDOWN_WAIT_MS))
            {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    if let Some(thread) = monitor.thread.take() {
                        let _ = thread.join();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // A busy SQLite read may outlive this request. The stop flag
                    // prevents another poll; detaching keeps request teardown bounded.
                }
            }
        }
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
    }
}

impl AppServer {
    /// 使用平台沙箱和已捕获的模型提供方配置快照创建未初始化的服务。
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
            execution_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 替换服务使用的 sandbox backend。
    pub fn with_sandbox_backend(
        mut self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.sandbox_backend = Arc::new(sandbox_backend);
        self
    }

    /// 判断服务是否已收到 shutdown 请求。
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// 判断初始化握手是否允许启动 turn worker。
    pub fn ready_for_turn_worker(&self) -> bool {
        self.initialized_acknowledged
    }

    /// 请求当前进程所有执行停止。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.cancellation_handle().request_execution_stop()
    }

    /// 返回共享的执行取消句柄。
    pub fn cancellation_handle(&self) -> AppServerCancellationHandle {
        AppServerCancellationHandle {
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
        }
    }

    /// 为请求工作线程打开独立的存储连接，同时共享停止状态。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: self.store.trusted_reopen()?,
            initialized: true,
            initialized_acknowledged: true,
            event_filter: self.event_filter.clone(),
            shutdown_requested: false,
            sandbox_backend: Arc::clone(&self.sandbox_backend),
            provider_snapshot: self.provider_snapshot.clone(),
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
        })
    }

    /// 注册一个活动 turn，并为其附加持久化取消监视器。
    fn activate_turn(
        &self,
        turn_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let (cancellation, guard) = self.prepare_turn_activation(turn_id)?;
        guard.start_monitor();
        Ok((cancellation, guard))
    }

    // Establish every fallible runtime resource before a new running Turn is
    // committed. The monitor remains paused until the caller starts it after commit.
    fn prepare_turn_activation(
        &self,
        turn_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let cancellation = CancellationToken::new();
        // Open the fallible monitor connection before publishing the registry entry.
        let monitor_store = if self.store.descriptor().path == ":memory:" {
            None
        } else {
            Some(self.store.trusted_reopen()?)
        };
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace("active turn registry poisoned".into()))?;
        if active_turns.contains_key(turn_id) {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is already active"
            )));
        }
        let monitor = cancellation_monitor(monitor_store, turn_id, cancellation.clone())?;
        if self.execution_stopped.load(Ordering::SeqCst) {
            cancellation.cancel();
        }
        active_turns.insert(turn_id.to_string(), cancellation.clone());
        drop(active_turns);
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
            monitor,
        };
        Ok((cancellation, guard))
    }

    fn refresh_turn_if_unowned(&self, turn: Turn) -> AppServerResult<Turn> {
        if is_terminal_turn_status(&turn.status) {
            return Ok(turn);
        }
        let Some(_execution_guard) = self.store.try_begin_workspace_execution(&turn.thread_id)?
        else {
            return Ok(turn);
        };
        self.store.get_turn(&turn.turn_id).map_err(Into::into)
    }

    /// 解析一行 JSON-RPC，并通过协议状态机进行分发。
    pub fn handle_json(&mut self, line: &str) -> AppServerResult<Vec<Value>> {
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        self.handle(message)
    }

    /// 处理一个已解析的 JSON-RPC 请求，并返回零个或多个协议响应或事件。
    pub fn handle(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let notification = message.is_notification();
        let id = message.id().cloned();
        let Some(method_name) = message.method_name() else {
            return if notification {
                Ok(Vec::new())
            } else {
                Ok(vec![JsonRpcMessage::invalid_request(id).to_wire_value()])
            };
        };
        let Some(method) = Method::parse(method_name) else {
            return if notification {
                Ok(Vec::new())
            } else {
                Ok(vec![JsonRpcMessage::method_not_found(id).to_wire_value()])
            };
        };

        if method
            .spec()
            .validate_params(message.params().cloned().unwrap_or_else(|| json!({})))
            .is_err()
        {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::invalid_params("Invalid params"))
            };
        }

        if matches!(method, Method::Initialized) && !self.initialized {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::not_initialized())
            };
        }
        if !matches!(method, Method::Initialize | Method::Initialized)
            && !self.initialized_acknowledged
        {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::not_initialized())
            };
        }

        let message = if notification {
            message.into_request_with_id(JsonRpcId::Number(0))
        } else {
            message
        };

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
        if notification {
            return Ok(Vec::new());
        }
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
                JsonRpcMessage::error(message.required_id(), ErrorCode::already_initialized())
                    .to_wire_value(),
            ]);
        }
        let _params: InitializeParams = parse_params(&message)?;
        self.initialized = true;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(InitializeResult::local())?,
            )
            .to_wire_value(),
        ])
    }

    fn server_capabilities(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
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
                message.required_id(),
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    fn thread_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadReadParams = parse_params(&message)?;
        let turn_limit = match history_turn_limit(params.limit) {
            Ok(limit) => limit,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        match self.store.read_thread_history(
            &params.thread_id,
            params.before_turn_sequence,
            turn_limit,
        ) {
            Ok(history) => json_response(
                message.required_id(),
                ThreadReadResult {
                    thread,
                    messages: history.messages,
                    next_before_turn_sequence: history.next_before_turn_sequence,
                },
            ),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn thread_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = workspace_tools_for_thread(&thread, Arc::clone(&self.sandbox_backend)) {
            return invalid_state_response(message.required_id(), error);
        }
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Active,
        ) {
            Ok(thread) => json_response(message.required_id(), ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = parse_params(&message)?;
        let cwd = match canonical_thread_cwd(params.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let (thread, _trace) = self.store.create_thread_with_trace_and_policy(
            params.model.as_deref(),
            Some(&cwd),
            params
                .sandbox_mode
                .unwrap_or(PermissionProfileName::WorkspaceWrite),
            params.approval_policy.unwrap_or(ApprovalPolicy::OnRequest),
            "app_server",
            "thread started",
        )?;
        let mut messages = Vec::new();
        messages.extend(self.event_notification(AppEvent::thread_started(&thread)));
        messages.push(
            JsonRpcMessage::response(
                message.required_id(),
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
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let source_cwd = match params.cwd.as_deref().or(source.cwd.as_deref()) {
            Some(cwd) => cwd,
            None => {
                return invalid_state_response(
                    message.required_id(),
                    "source thread does not have an absolute workspace",
                );
            }
        };
        let cwd = match canonical_thread_cwd(Some(source_cwd)) {
            Ok(cwd) => cwd,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let thread = self.store.create_thread_with_policy(
            params.model.as_deref().or(source.model.as_deref()),
            Some(&cwd),
            params.sandbox_mode.unwrap_or(source.sandbox_mode),
            params.approval_policy.unwrap_or(source.approval_policy),
        )?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
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
            Ok(thread) => json_response(message.required_id(), ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(StoreError::ThreadHasNonterminalTurn { .. }) => {
                invalid_state_response(message.required_id(), THREAD_EXECUTION_ACTIVE)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn thread_delete(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        match self.store.delete_thread(&params.thread_id) {
            Ok(()) => Ok(vec![
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(ThreadDeleteResult {
                        thread_id: params.thread_id,
                        deleted: true,
                    })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(StoreError::ThreadHasNonterminalTurn { .. }) => {
                invalid_state_response(message.required_id(), THREAD_EXECUTION_ACTIVE)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        self.handle_turn_start_streaming(message, |message| messages.push(message))?;
        Ok(messages)
    }

    /// 执行 `turn/start`，并在每个持久化阶段完成时发出生命周期事件。
    pub fn handle_turn_start_streaming(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        if message.method_name() != Some(Method::TurnStart.as_str()) {
            return Err(AppServerError::InvalidParams(
                "streaming handler requires turn/start".to_string(),
            ));
        }
        let params: TurnStartParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if thread.status != singularity_protocol::ThreadStatus::Active {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), THREAD_ARCHIVED)?,
            );
            return Ok(());
        }
        let workspace_tools =
            match workspace_tools_for_thread(&thread, Arc::clone(&self.sandbox_backend)) {
                Ok(tools) => tools,
                Err(error) => {
                    emit_messages(
                        &mut emit,
                        invalid_state_response(message.required_id(), error)?,
                    );
                    return Ok(());
                }
            };
        let capability = agent_loop_capability(self.sandbox_backend.as_ref());
        if !agent_loop_capability_ready(&capability) {
            emit_messages(
                &mut emit,
                invalid_state_response(
                    message.required_id(),
                    agent_loop_unavailable_message(&capability),
                )?,
            );
            return Ok(());
        }
        let Some(_execution_guard) = self
            .store
            .try_begin_workspace_execution(&params.thread_id)?
        else {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
            );
            return Ok(());
        };
        let payload = serde_json::to_value(&params.input)?;
        let allocated_turn_id = SessionStore::allocate_turn_id();
        let (cancellation, active_turn) =
            self.prepare_turn_activation(allocated_turn_id.as_str())?;
        let started = match self
            .store
            .create_allocated_turn_with_input_trace_and_history(
                allocated_turn_id,
                CreateStartedTurnParams {
                    thread_id: &params.thread_id,
                    agent_loop_status: AgentStatus::Running.as_str(),
                    input: payload,
                    component: "app_server",
                    summary: "turn started",
                    history_turn_limit: DEFAULT_THREAD_HISTORY_TURN_LIMIT,
                },
            ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(StoreError::WorkspaceHasNonterminalTurn { .. }) => {
                emit_messages(
                    &mut emit,
                    invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let turn = started.turn;
        active_turn.start_monitor();
        if let Some(event) = self.event_notification(AppEvent::turn_started(&turn)) {
            emit(event);
        }
        let status = self.run_agent_loop(
            &thread,
            &params,
            &turn.turn_id,
            &started.history.messages,
            &cancellation,
            workspace_tools,
        )?;
        let committed = self.commit_turn_run_status(turn, &status, &cancellation)?;
        let terminal_events = self.committed_turn_events(&committed)?;
        let turn = committed.turn;
        emit_messages(&mut emit, terminal_events);
        emit(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnStartResult { turn })?,
            )
            .to_wire_value(),
        );
        Ok(())
    }

    fn agent_capability(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let capability = agent_loop_capability(self.sandbox_backend.as_ref());
        json_response(
            message.required_id(),
            AgentCapabilityResult {
                agent_loop: AgentLoopCapabilityStatus {
                    available: capability.available,
                    status: capability.status.as_str().to_string(),
                    reason: capability.reason,
                    blockers: capability.blockers,
                },
                provider_configuration: provider_configuration(&self.provider_snapshot),
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
            Ok(result) => json_response(message.required_id(), result),
            Err(_) => json_error(
                Some(message.required_id()),
                ErrorCode::invalid_params("Invalid params"),
            ),
        }
    }

    /// 根据已捕获的模型提供方、工作区策略和持久化历史构建 `AgentLoop`。
    fn run_agent_loop(
        &self,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        history: &[ConversationMessage],
        cancellation: &CancellationToken,
        workspace_tools: WorkspaceTools,
    ) -> AppServerResult<AgentRunStatus> {
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let category = error.error.category();
                return Ok(
                    AgentRunStatus::failed_with_category(error.message, Some(category))
                        .with_status(AgentStatus::Failed),
                );
            }
        };
        let invocation = AgentLoopInvocation {
            thread,
            params,
            turn_id,
            history,
            cancellation,
        };
        match self.run_agent_loop_with_provider_and_tools(provider, invocation, workspace_tools) {
            Err(AppServerError::ProjectInstructions(error)) => {
                Ok(AgentRunStatus::failed(error.to_string()).with_status(AgentStatus::Failed))
            }
            Err(AppServerError::Workspace(error)) => {
                Ok(AgentRunStatus::failed(error).with_status(AgentStatus::Failed))
            }
            result => result,
        }
    }

    /// 仅当存储与 turn 仍满足其契约时恢复已批准的检查点。
    fn resume_agent_loop(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        cancellation: &CancellationToken,
        prepared_workspace_tools: Option<WorkspaceTools>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<ApprovalCheckpoint>)>> {
        if !agent_loop_ready(self.sandbox_backend.as_ref()) {
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
                let category = error.error.category();
                let turn_id = &request.turn_id;
                let turn = self.store.get_turn(turn_id)?;
                let thread = self.store.get_thread(&request.thread_id)?;
                let mut run_status = approval_terminal_status(
                    &thread,
                    decision,
                    pending_tool_call.as_ref(),
                    AgentStatus::Failed,
                    "unavailable",
                    error.message,
                );
                run_status.error_category = Some(category);
                return Ok(Some((turn, run_status, Vec::new())));
            }
        };
        self.resume_agent_loop_after_gate(
            request,
            decision,
            pending_tool_call,
            provider,
            cancellation,
            prepared_workspace_tools,
        )
    }

    /// 重建规范化的 loop 输入，并执行一个已批准的待执行调用。
    fn resume_agent_loop_after_gate<P>(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        provider: P,
        cancellation: &CancellationToken,
        prepared_workspace_tools: Option<WorkspaceTools>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<ApprovalCheckpoint>)>>
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
        let thread = self.store.get_thread(&turn.thread_id)?;
        let Some(pending_tool_call) = pending_tool_call else {
            return Ok(None);
        };
        let pending = match serde_json::from_value::<PendingToolCall>(pending_tool_call.clone()) {
            Ok(pending) => pending,
            Err(error) => {
                let run_status = approval_terminal_status(
                    &thread,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    format!("invalid pending tool call: {error}"),
                );
                return Ok(Some((turn, run_status, Vec::new())));
            }
        };
        if pending.request_id != request.request_id {
            let run_status = approval_terminal_status(
                &thread,
                decision,
                Some(&pending_tool_call),
                AgentStatus::Failed,
                "unavailable",
                "pending tool call request mismatch",
            );
            return Ok(Some((turn, run_status, Vec::new())));
        }
        if thread.status != singularity_protocol::ThreadStatus::Active {
            return Ok(None);
        }
        let workspace_tools = match prepared_workspace_tools {
            Some(workspace_tools) => workspace_tools,
            None => {
                let run_status = approval_terminal_status(
                    &thread,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    "workspace capability was not prepared",
                );
                return Ok(Some((turn, run_status, Vec::new())));
            }
        };
        let workspace_root = workspace_tools.workspace_root().to_path_buf();
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
        let policy = workspace_policy(thread.sandbox_mode, thread.approval_policy);
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
                    &thread,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    error.to_string(),
                );
                return Ok(Some((turn, run_status, Vec::new())));
            }
        };
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(cancellation.clone())
            .resume_pending_tool_call(&loop_input, &pending, &pending_tool_call);
        let mut run_status = result.to_run_status();
        let next_approvals = match approval_checkpoints(&result) {
            Ok(next_approvals) => next_approvals,
            Err(error) => {
                run_status.status = AgentStatus::Failed;
                run_status.completed = false;
                run_status.final_answer = None;
                run_status.error = Some(format!("approval continuation failed: {error}"));
                Vec::new()
            }
        };
        if run_status.audit_events.is_empty() && pending.tool_name.as_str() == TOOL_COMMAND {
            let audit_status = approval_terminal_status(
                &thread,
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
        Ok(Some((turn, run_status, next_approvals)))
    }

    #[cfg(test)]
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
        let workspace_tools = workspace_tools_for_thread(thread, Arc::clone(&self.sandbox_backend))
            .map_err(AppServerError::Workspace)?;
        let invocation = AgentLoopInvocation {
            thread,
            params,
            turn_id,
            history,
            cancellation,
        };
        self.run_agent_loop_with_provider_and_tools(provider, invocation, workspace_tools)
    }

    fn run_agent_loop_with_provider_and_tools<P>(
        &self,
        provider: P,
        invocation: AgentLoopInvocation<'_>,
        workspace_tools: WorkspaceTools,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider,
    {
        let registry = workspace_tool_registry();
        let workspace_root = workspace_tools.workspace_root().to_path_buf();
        let policy = workspace_policy(
            invocation.thread.sandbox_mode,
            invocation.thread.approval_policy,
        );
        let loop_input = agent_loop_input(
            invocation.thread,
            invocation.params,
            invocation.turn_id,
            &workspace_root,
            invocation.history,
        )?;
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(invocation.cancellation.clone())
            .run(&loop_input);
        let mut run_status = result.to_run_status();
        if invocation.cancellation.is_cancelled() {
            mark_run_cancelled(&mut run_status);
            return Ok(run_status);
        }
        match self.persist_agent_approval_requests(&result) {
            Ok(()) => Ok(run_status),
            Err(AppServerError::Store(StoreError::InvalidState(message)))
                if message == "pending approval requires a running or blocked turn" =>
            {
                let turn = self.store.get_turn(invocation.turn_id)?;
                if turn.agent_loop_status == AgentStatus::CancelRequested.as_str()
                    || turn.status == TurnStatus::Interrupted
                {
                    mark_run_cancelled(&mut run_status);
                    Ok(run_status)
                } else {
                    Err(StoreError::InvalidState(message).into())
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 在向客户端暴露阻塞 turn 前持久化每个 `AgentLoop` 检查点。
    fn persist_agent_approval_requests(&self, result: &AgentLoopResult) -> AppServerResult<()> {
        for (request, pending_tool_call) in approval_checkpoints(result)? {
            match self.store.create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(pending_tool_call),
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

    /// 将运行状态映射为持久化 turn 状态，并在提交时让取消优先。
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
            Err(StoreError::InvalidState(message))
                if message == "terminal turn status cannot be overwritten" =>
            {
                let current = self.store.get_turn(&turn.turn_id)?;
                if current.status == TurnStatus::Interrupted
                    && current.agent_loop_status == AgentStatus::Cancelled.as_str()
                {
                    mark_run_cancelled(&mut effective_status);
                    self.commit_effective_turn_status(&turn, &effective_status)
                        .map_err(Into::into)
                } else {
                    Err(StoreError::InvalidState(message).into())
                }
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
        let plan = run_status
            .plan
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let event = agent_loop_trace(turn, run_status);
        self.store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: turn_status_for_agent(&run_status.status),
                agent_loop_status: run_status.status.as_str(),
                assistant_delta: assistant_delta.as_deref(),
                plan: plan.as_ref(),
                trace: &event,
            },
        )
    }

    /// 在一个存储事务中提交 approval 续行状态及后续检查点（如有）。
    fn commit_effective_turn_status_resolving_approval(
        &self,
        request_id: &str,
        turn: &Turn,
        run_status: &AgentRunStatus,
        next_approvals: &[ApprovalCheckpoint],
    ) -> Result<CommittedTurnOutcome, StoreError> {
        let mut effective_status = run_status.clone();
        let commit = |status: &AgentRunStatus| {
            let assistant_delta = agent_completed_delta(status);
            let plan = status.plan.as_ref().map(serde_json::to_value).transpose()?;
            let event = agent_loop_trace(turn, status);
            let effective_next_approvals = if status.status == AgentStatus::Blocked {
                next_approvals
            } else {
                &[]
            };
            self.store
                .commit_turn_outcome_and_resolve_pending_execution(
                    request_id,
                    CommitTurnOutcomeParams {
                        status: turn_status_for_agent(&status.status),
                        agent_loop_status: status.status.as_str(),
                        assistant_delta: assistant_delta.as_deref(),
                        plan: plan.as_ref(),
                        trace: &event,
                    },
                    effective_next_approvals,
                )
        };
        match commit(&effective_status) {
            Ok(committed) => Ok(committed),
            Err(StoreError::InvalidState(message))
                if message == "cancel-requested turn can only finalize as interrupted" =>
            {
                mark_run_cancelled(&mut effective_status);
                commit(&effective_status)
            }
            Err(error) => Err(error),
        }
    }

    fn turn_interrupt(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let turn = match self.store.get_turn(&params.turn_id) {
            Ok(turn) => self.refresh_turn_if_unowned(turn)?,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if is_terminal_turn_status(&turn.status) {
            return Ok(vec![
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(TurnInterruptResult {
                        status: turn.status.as_storage_text().to_string(),
                        turn_id: turn.turn_id,
                        agent_loop_status: Some(turn.agent_loop_status),
                    })?,
                )
                .to_wire_value(),
            ]);
        }
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
            ..TraceEvent::for_turn(
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
            turn.status.as_storage_text()
        };
        let mut messages = Vec::new();
        if is_terminal_turn_status(&turn.status) {
            messages.extend(self.event_notification(AppEvent::turn_completed(&turn)));
        }
        messages.push(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnInterruptResult {
                    turn_id: turn.turn_id,
                    status: status.to_string(),
                    agent_loop_status: Some(turn.agent_loop_status),
                })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    fn turn_status(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(
                message.required_id(),
                TurnResult {
                    turn: self.refresh_turn_if_unowned(turn)?,
                },
            ),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TURN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn server_shutdown(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.shutdown_requested = true;
        self.request_execution_stop()?;
        json_response(
            message.required_id(),
            ServerShutdownResult { shutdown: true },
        )
    }

    fn approval_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let approvals = self.store.list_pending_approvals()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ApprovalListResult { approvals })?,
            )
            .to_wire_value(),
        ])
    }

    fn approval_center(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            ApprovalCenterResult {
                pending_approvals: self.store.list_pending_approvals()?,
                decisions: self.store.list_approval_decisions()?,
            },
        )
    }

    fn approval_request(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let _request: ApprovalRequest = parse_params(&message)?;
        invalid_state_response(message.required_id(), APPROVAL_REQUEST_INTERNAL_ONLY)
    }

    /// 记录 approval，并保留、失败处理或恢复已认领的检查点。
    fn approval_decision(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = parse_params(&message)?;
        let pending_request = match self.store.get_pending_approval(&decision.request_id) {
            Ok(request) => request,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let is_tool_continuation = pending_request.tool_call_id.is_some();
        if is_tool_continuation
            && !self
                .store
                .has_pending_tool_call(&pending_request.request_id)?
        {
            return not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND);
        }
        let pending_thread = self.store.get_thread(&pending_request.thread_id)?;
        let continues_execution =
            is_tool_continuation && matches!(decision.outcome, ApprovalOutcome::Allow);
        let continuation_workspace = if continues_execution {
            if pending_thread.status != singularity_protocol::ThreadStatus::Active {
                return invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION);
            }
            match workspace_tools_for_thread(&pending_thread, Arc::clone(&self.sandbox_backend)) {
                Ok(tools) => Some(tools),
                Err(error) => return invalid_state_response(message.required_id(), error),
            }
        } else {
            None
        };
        let _execution_guard = if continues_execution {
            let Some(guard) = self
                .store
                .try_begin_workspace_execution(&pending_request.thread_id)?
            else {
                return invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE);
            };
            Some(guard)
        } else {
            None
        };
        let active_turn = if continues_execution {
            let active_turn = self.activate_turn(&pending_request.turn_id)?;
            if active_turn.0.is_cancelled() {
                return invalid_state_response(message.required_id(), EXECUTION_STOPPED);
            }
            Some(active_turn)
        } else {
            None
        };
        let recorded = match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(recorded) => recorded,
            Err(error) => {
                return match error {
                    StoreError::NotFound(_) => {
                        not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)
                    }
                    StoreError::InvalidState(state_message)
                        if state_message == "pending approval allow requires an active thread" =>
                    {
                        invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION)
                    }
                    StoreError::WorkspaceHasNonterminalTurn { .. } => {
                        invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)
                    }
                    other => Err(other.into()),
                };
            }
        };
        if let Some((cancellation, _guard)) = active_turn.as_ref() {
            let turn = self.store.get_turn(&recorded.request.turn_id)?;
            if turn.agent_loop_status == "cancel_requested" {
                cancellation.cancel();
            }
        }
        let pending_tool_call = recorded.pending_tool_call.clone();
        if matches!(decision.outcome, ApprovalOutcome::Defer) {
            return Ok(vec![approval_decision_response(
                message.required_id(),
                &decision,
            )?]);
        }
        if matches!(decision.outcome, ApprovalOutcome::Deny) {
            let mut messages = Vec::new();
            if pending_tool_call.is_some() {
                let turn = self.store.get_turn(&recorded.request.turn_id)?;
                messages.extend(self.event_notification(AppEvent::turn_completed(&turn)));
            }
            messages.push(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(messages);
        }
        let mut messages = Vec::new();
        let continuation = (|| -> AppServerResult<_> {
            let cancellation = active_turn
                .as_ref()
                .map(|(cancellation, _guard)| cancellation.clone())
                .unwrap_or_default();
            let resumed = self.resume_agent_loop(
                &recorded.request,
                &decision,
                pending_tool_call.clone(),
                &cancellation,
                continuation_workspace.clone(),
            )?;
            let terminal = if let Some(resumed) = resumed {
                Some(resumed)
            } else {
                self.approval_no_resume_status(
                    &recorded.request,
                    &decision,
                    pending_tool_call.as_ref(),
                )?
                .map(|(turn, run_status)| (turn, run_status, Vec::new()))
            };
            Ok((terminal, cancellation))
        })();
        let (terminal, cancellation) = match continuation {
            Ok(continuation) => continuation,
            Err(error) => {
                let committed = self.terminalize_claimed_approval_error(
                    &recorded.request,
                    &decision,
                    pending_tool_call.as_ref(),
                    None,
                    &error,
                )?;
                messages.extend(self.committed_turn_events(&committed)?);
                messages.push(approval_decision_response(
                    message.required_id(),
                    &decision,
                )?);
                return Ok(messages);
            }
        };
        if let Some((turn, run_status, next_approvals)) = terminal {
            let mut effective_status = run_status.clone();
            if cancellation.is_cancelled() {
                mark_run_cancelled(&mut effective_status);
            }
            let committed = match self.commit_effective_turn_status_resolving_approval(
                &decision.request_id,
                &turn,
                &effective_status,
                &next_approvals,
            ) {
                Ok(committed) => committed,
                Err(error) => self.terminalize_claimed_approval_error(
                    &recorded.request,
                    &decision,
                    pending_tool_call.as_ref(),
                    Some(&effective_status),
                    &AppServerError::Store(error),
                )?,
            };
            messages.extend(self.committed_turn_events(&committed)?);
        }
        messages.push(approval_decision_response(
            message.required_id(),
            &decision,
        )?);
        Ok(messages)
    }

    fn terminalize_claimed_approval_error(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<&Value>,
        prior_status: Option<&AgentRunStatus>,
        continuation_error: &AppServerError,
    ) -> AppServerResult<CommittedTurnOutcome> {
        let turn = self.store.get_turn(&request.turn_id).map_err(|error| {
            AppServerError::Store(StoreError::InvalidState(format!(
                "approval continuation failed: {continuation_error}; failed to load claimed turn for terminalization: {error}"
            )))
        })?;
        let thread = self.store.get_thread(&turn.thread_id)?;
        let fallback_status = approval_terminal_status(
            &thread,
            decision,
            pending_tool_call,
            AgentStatus::Failed,
            "unavailable",
            format!("approval continuation failed: {continuation_error}"),
        );
        let mut run_status = prior_status
            .cloned()
            .unwrap_or_else(|| fallback_status.clone());
        run_status.status = AgentStatus::Failed;
        run_status.completed = false;
        run_status.final_answer = None;
        run_status.error = fallback_status.error;
        if run_status.audit_events.is_empty() {
            run_status.audit_events = fallback_status.audit_events;
        }
        self.commit_effective_turn_status_resolving_approval(
            &decision.request_id,
            &turn,
            &run_status,
            &[],
        )
        .map_err(|error| {
            AppServerError::Store(StoreError::InvalidState(format!(
                "approval continuation failed: {continuation_error}; failed to persist claimed approval terminal state: {error}"
            )))
        })
    }

    fn approval_no_resume_status(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<&Value>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        let turn = self.store.get_turn(&request.turn_id)?;
        let thread = self.store.get_thread(&turn.thread_id)?;
        if pending_tool_call.is_none() {
            return Ok(None);
        }
        let (status, audit_decision, message) = if turn.agent_loop_status
            == AgentStatus::CancelRequested.as_str()
        {
            (
                AgentStatus::Cancelled,
                "unavailable",
                "approval continuation interrupted before resume",
            )
        } else if is_terminal_turn_status(&turn.status) {
            (
                AgentStatus::from(turn.agent_loop_status.as_str()),
                "unavailable",
                "approval continuation already reached a terminal turn",
            )
        } else if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but turn state changed before agent loop resume",
            )
        } else {
            match decision.outcome {
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
            }
        };
        let run_status = approval_terminal_status(
            &thread,
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
            message.required_id(),
            EventSubscribeResult {
                subscription_id: EVENT_SUBSCRIPTION_ID.to_string(),
                event_types: params.event_types,
            },
        )
    }

    fn artifact_fetch(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ArtifactFetchParams = parse_params(&message)?;
        match self.store.get_artifact_ref(&params.artifact_id) {
            Ok(artifact) => json_response(message.required_id(), ArtifactFetchResult { artifact }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), ARTIFACT_NOT_FOUND)
            }
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

    fn committed_turn_events(
        &self,
        committed: &CommittedTurnOutcome,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = self.agent_terminal_item_events(committed.plan_item.as_ref())?;
        if let Some(plan_item) = committed.plan_item.as_ref()
            && let Some(event) = self.event_notification(AppEvent::turn_plan_updated(
                &committed.turn.turn_id,
                plan_item.payload.clone(),
            ))
        {
            messages.push(event);
        }
        messages.extend(self.agent_terminal_item_events(committed.assistant_item.as_ref())?);
        messages.extend(self.event_notification(AppEvent::turn_completed(&committed.turn)));
        Ok(messages)
    }

    fn agent_terminal_item_events(&self, item: Option<&Item>) -> AppServerResult<Vec<Value>> {
        let Some(agent_item) = item else {
            return Ok(Vec::new());
        };
        let mut events = vec![AppEvent::item_started(agent_item.item_id.clone())];
        match &agent_item.kind {
            singularity_protocol::ItemKind::Plan => {}
            singularity_protocol::ItemKind::AgentMessage => {
                let agent_delta = agent_item
                    .payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "committed assistant item is missing its string delta".to_string(),
                        )
                    })?;
                events.push(AppEvent::item_agent_message_delta(
                    agent_item.item_id.clone(),
                    agent_delta,
                ));
            }
            _ => {
                return Err(StoreError::InvalidState(format!(
                    "unsupported committed terminal item kind: {:?}",
                    agent_item.kind
                ))
                .into());
            }
        }
        events.push(AppEvent::item_completed(agent_item.item_id.clone()));
        Ok(events
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
            Ok(events) => json_response(message.required_id(), TraceListResult { events }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_RUN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn trace_show(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceShowParams = parse_params(&message)?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => json_response(message.required_id(), TraceShowResult { event }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_EVENT_NOT_FOUND)
            }
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
                    message.required_id(),
                    serde_json::to_value(TraceListResult { events })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_RUN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn json_response<T: serde::Serialize>(id: JsonRpcId, result: T) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

fn approval_decision_response(
    id: JsonRpcId,
    decision: &ApprovalDecision,
) -> AppServerResult<Value> {
    Ok(JsonRpcMessage::response(
        id,
        serde_json::to_value(ApprovalDecisionResult {
            decision: decision.clone(),
        })?,
    )
    .to_wire_value())
}

fn emit_messages(emit: &mut impl FnMut(Value), messages: Vec<Value>) {
    for message in messages {
        emit(message);
    }
}

/// 监视持久化 turn 状态，使外部中断能够到达进程内 `AgentLoop`。
fn cancellation_monitor(
    store: Option<SessionStore>,
    turn_id: &str,
    cancellation: CancellationToken,
) -> AppServerResult<Option<CancellationMonitor>> {
    let Some(store) = store else {
        return Ok(None);
    };
    let turn_id = turn_id.to_string();
    let (wake, wake_receiver) = mpsc::channel();
    let (done_sender, done) = mpsc::channel();
    let control = Arc::new(CancellationMonitorControl {
        started: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        wake,
    });
    let thread_control = Arc::clone(&control);
    let thread = ThreadBuilder::new()
        .name("turn-cancellation-monitor".to_string())
        .spawn(move || {
            while !thread_control.started.load(Ordering::SeqCst)
                && !thread_control.stop.load(Ordering::SeqCst)
            {
                match wake_receiver.recv_timeout(Duration::from_millis(TURN_CANCELLATION_POLL_MS)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            while !thread_control.stop.load(Ordering::SeqCst) && !cancellation.is_cancelled() {
                match store.get_turn(&turn_id) {
                    Ok(turn) if turn.agent_loop_status == AgentStatus::CancelRequested.as_str() => {
                        cancellation.cancel();
                        break;
                    }
                    Ok(_) => {}
                    Err(error) if error.is_transient_contention() => {}
                    Err(_) => {
                        cancellation.cancel();
                        break;
                    }
                }
                match wake_receiver.recv_timeout(Duration::from_millis(TURN_CANCELLATION_POLL_MS)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let _ = done_sender.send(());
        })
        .map_err(|error| {
            AppServerError::Workspace(format!("cannot spawn turn cancellation monitor: {error}"))
        })?;
    Ok(Some(CancellationMonitor {
        control,
        done,
        thread: Some(thread),
    }))
}

fn approval_terminal_status(
    thread: &Thread,
    decision: &ApprovalDecision,
    pending_tool_call: Option<&Value>,
    status: AgentStatus,
    audit_decision: &str,
    message: impl Into<String>,
) -> AgentRunStatus {
    let mut run_status = AgentRunStatus::failed(message).with_status(status);
    run_status.approval_count = 1;
    let mut audit_event = json!({
        "approval_policy": thread.approval_policy,
        "approval_decision": audit_decision,
        "approval_request_id": decision.request_id,
        "approval_decision_id": decision.decision_id,
        "command_provenance": "agent_requested",
    });
    if let Some(command_audit) = pending_command_audit_metadata(pending_tool_call, thread) {
        merge_json_object(&mut audit_event, command_audit);
    }
    run_status
        .audit_events
        .push(project_audit_event(&audit_event));
    run_status
}

fn pending_command_audit_metadata(
    pending_tool_call: Option<&Value>,
    thread: &Thread,
) -> Option<Value> {
    let pending_value = pending_tool_call?;
    let tool_name = pending_value.get("tool_name").and_then(Value::as_str)?;
    if tool_name != TOOL_COMMAND {
        return None;
    }
    let resources = serde_json::from_value::<PendingToolCall>(pending_value.clone())
        .map(|pending| pending.resources)
        .unwrap_or_default();
    let scope_digest = resources.iter().find_map(|resource| match resource {
        PermissionResource::CommandScope(digest) => Some(digest.as_str()),
        _ => None,
    });
    let audit = json!({
        "sandbox_backend": "not_executed",
        "sandbox_enforcement": "not_executed",
        "sandbox_mode": sandbox_mode_audit_label(thread.sandbox_mode),
        "network_access": "denied",
        "command_scope_digest": scope_digest.unwrap_or("unavailable"),
        "policy_scope_binding": if scope_digest.is_some() {
            "bound"
        } else {
            "unavailable"
        },
    });
    Some(audit)
}

fn merge_json_object(target: &mut Value, source: Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn approval_checkpoints(result: &AgentLoopResult) -> AppServerResult<Vec<ApprovalCheckpoint>> {
    result
        .approval_requests
        .iter()
        .map(|request| {
            let checkpoint = result
                .approval_checkpoint(&request.request_id)
                .ok_or_else(|| {
                    AppServerError::Store(StoreError::InvalidState(
                        APPROVAL_CHECKPOINT_REQUIRED.to_string(),
                    ))
                })?;
            Ok((request.clone(), checkpoint))
        })
        .collect()
}

fn workspace_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for entry in workspace_tool_entries()
        .into_iter()
        .chain(agent_control_tool_entries())
    {
        registry.register(entry).expect("valid builtin tool");
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
    let workspace_tools =
        WorkspaceTools::new(path).map_err(|error| format!("failed to bind thread cwd: {error}"))?;
    workspace_tools
        .workspace_root()
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())
}
fn workspace_path(thread: &Thread) -> Result<PathBuf, String> {
    let cwd = thread
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| "thread does not have an absolute workspace".to_string())?;
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err("thread does not have an absolute workspace".to_string());
    }
    Ok(path.to_path_buf())
}

fn workspace_tools_for_thread(
    thread: &Thread,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
) -> Result<WorkspaceTools, String> {
    let workspace_path = workspace_path(thread)?;
    WorkspaceTools::new(workspace_path)
        .map(|tools| tools.with_shared_sandbox_backend(sandbox_backend))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn workspace_tools(
    workspace_root: PathBuf,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
) -> AppServerResult<WorkspaceTools> {
    WorkspaceTools::new(workspace_root)
        .map(|tools| tools.with_shared_sandbox_backend(sandbox_backend))
        .map_err(|error| AppServerError::Workspace(error.to_string()))
}

fn agent_loop_input(
    thread: &Thread,
    params: &TurnStartParams,
    turn_id: &str,
    workspace_root: &std::path::Path,
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
    if let Some(instructions) = load_project_instructions(workspace_root, workspace_root)? {
        input = input.with_project_instructions(instructions);
    }
    Ok(input)
}

fn agent_loop_capability(sandbox_backend: &dyn SandboxBackend) -> AgentLoopCapability {
    if sandbox_backend.capabilities().enforcement() == SandboxBackendEnforcement::Strict {
        AgentLoopCapability::available(format!(
            "AgentLoop uses the {} strict sandbox backend",
            sandbox_backend.name()
        ))
    } else {
        AgentLoopCapability::unavailable(
            format!(
                "AgentLoop requires a strict command sandbox; backend {} is unavailable",
                sandbox_backend.name()
            ),
            STRICT_COMMAND_SANDBOX_UNAVAILABLE,
        )
    }
}

fn agent_loop_ready(sandbox_backend: &dyn SandboxBackend) -> bool {
    let capability = agent_loop_capability(sandbox_backend);
    agent_loop_capability_ready(&capability)
}

fn provider_configuration(snapshot: &ProviderConfigSnapshot) -> ProviderConfigurationStatus {
    let config = snapshot.redacted_config();
    let configuration = snapshot.configuration();
    ProviderConfigurationStatus {
        source: snapshot.source().map(|source| source.as_str().to_string()),
        snapshot_id: snapshot.snapshot_id().to_string(),
        configured: configuration.configured,
        configuration_blocker: configuration
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
fn workspace_policy(
    sandbox_mode: PermissionProfileName,
    approval_policy: ApprovalPolicy,
) -> PolicyEngine {
    let mut profile = match sandbox_mode {
        PermissionProfileName::ReadOnly => PermissionProfile::read_only(),
        PermissionProfileName::WorkspaceWrite => PermissionProfile::workspace_write(),
    };
    profile.approval_policy = approval_policy;
    PolicyEngine::new(profile)
        .with_rule(workspace_read_tool_rule())
        .with_rule(sandbox_command_rule())
}

fn sandbox_mode_audit_label(mode: PermissionProfileName) -> &'static str {
    match mode {
        PermissionProfileName::ReadOnly => "read_only",
        PermissionProfileName::WorkspaceWrite => "workspace_write",
    }
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

fn json_error(id: Option<JsonRpcId>, error: ErrorCode) -> AppServerResult<Vec<Value>> {
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
    matches!(
        status,
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
    )
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
    status.error_category = None;
    status.provider_diagnostic = None;
}

fn agent_loop_trace(turn: &Turn, status: &AgentRunStatus) -> TraceEvent {
    let mut event = TraceEvent::for_turn(
        format!(
            "trace_{}_agent_loop_{}_{}",
            turn.turn_id,
            status.status.as_str(),
            status.model_turns
        ),
        &turn.thread_id,
        &turn.turn_id,
        "agent_loop",
        "AgentLoop result translated",
    );
    event.payload = json!({
        "component": "agent_loop",
        "status": status.status.as_str(),
        "model_turns": status.model_turns,
        "model_turn_limit": status.model_turn_limit,
        "context": status.context_trace.as_ref().map(|context| json!({
            "included_item_ids": context
                .included_item_ids
                .iter()
                .map(|item_id| redact_app_server_text(item_id))
                .collect::<Vec<_>>(),
            "excluded_item_ids": context
                .excluded_item_ids
                .iter()
                .map(|item_id| redact_app_server_text(item_id))
                .collect::<Vec<_>>(),
            "budget": &context.budget,
            "compaction_count": context.compaction_count,
            "compacted_message_count": context.compacted_message_count,
            "last_compaction_before_tokens": context.last_compaction_before_tokens,
            "last_compaction_after_tokens": context.last_compaction_after_tokens,
        })),
        "tool_calls": status.tool_calls,
        "approval_count": status.approval_count,
        "plan": &status.plan,
        "plan_update_count": status.plan_update_count,
        "recovery_metrics": &status.recovery_metrics,
        "model_usage": &status.model_usage,
        "provider_attempts": &status.provider_attempts,
        "provider_protocol": {
            "contract": &status.provider_protocol_contract,
            "capability_metadata": &status.provider_capability_metadata,
        },
        "audit_events": &status.audit_events,
        "verification": &status.verification,
        "error": status.error.as_deref().map(redact_app_server_text),
        "provider_diagnostic": status.provider_diagnostic,
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

fn not_found_response(id: JsonRpcId, message: &'static str) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::not_found(message)).to_wire_value(),
    ])
}

fn invalid_state_response(
    id: JsonRpcId,
    message: impl Into<String>,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::new(APP_ERROR_INVALID_STATE, message)).to_wire_value(),
    ])
}

fn invalid_params_response(id: JsonRpcId) -> AppServerResult<Vec<Value>> {
    json_error(Some(id), ErrorCode::invalid_params("Invalid params"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use singularity_agent::PendingToolCall;
    use singularity_model::{
        ModelError, ModelErrorCategory, ModelErrorKind, ModelRole, ModelToolCall,
        ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, Provider,
        ProviderError, ProviderProtocolContract,
    };
    use singularity_policy::{CommandScopeDigest, ToolId, WorkspaceRelativePath};
    use singularity_protocol::ItemKind;
    use singularity_sandbox::CommandScriptRequest;
    use singularity_tools::{
        CommandRequest, CommandResult, SandboxFilesystemMode, SandboxNetworkMode,
        command_script_scope_digest_with_policy,
    };

    use super::*;

    fn tool_id(value: &str) -> ToolId {
        ToolId::new(value).expect("valid tool id")
    }

    fn workspace_resource(value: &str) -> PermissionResource {
        PermissionResource::WorkspacePath(
            WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
        )
    }

    fn command_resource(value: String) -> PermissionResource {
        PermissionResource::CommandScope(
            CommandScopeDigest::new(value).expect("valid command scope"),
        )
    }

    fn app_server(store: SessionStore) -> AppServer {
        AppServer::new(store, ProviderConfigSnapshot::capture(|_| None))
    }

    #[test]
    fn ordinary_and_evaluation_traces_share_safe_audit_projection_and_store_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        let mut status = AgentRunStatus::failed("safe failure");
        status.audit_events.push(project_audit_event(&json!({
            "cwd": "C:/sensitive/workspace",
            "raw_arguments": {"command": "echo secret"},
            "approval_reason": "operator reason",
            "approval_request_id": "approval-secret",
            "approval_grant_id": "grant-secret",
            "sandbox_mode": "workspace_write",
            "network_access": "allowed",
            "sandbox_backend": "test_backend",
            "sandbox_enforcement": "strict",
            "local_process_fallback": false,
            "command_scope_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "command_provenance": "agent_requested",
            "approval_policy": "on-request",
            "approval_decision": "approved",
            "timeout_seconds": 5,
        })));

        let ordinary = agent_loop_trace(&turn, &status);
        let evaluation = json!({"audit_events": &status.audit_events});
        for serialized in [
            serde_json::to_string(&ordinary).expect("ordinary trace JSON"),
            serde_json::to_string(&evaluation).expect("evaluation trace JSON"),
        ] {
            for forbidden in [
                "C:/sensitive/workspace",
                "raw_arguments",
                "operator reason",
                "approval-secret",
                "grant-secret",
            ] {
                assert!(!serialized.contains(forbidden), "leaked {forbidden}");
            }
        }

        store.append_trace(&ordinary).expect("append trace");
        let restored = store
            .show_trace(&ordinary.event_id)
            .expect("roundtrip trace");
        assert!(restored.redaction_applied);
        assert_eq!(
            restored.payload["audit_events"][0]["sandbox_mode"],
            "workspace_write"
        );
        let restored_json = serde_json::to_string(&restored).expect("restored trace JSON");
        assert!(!restored_json.contains("raw_arguments"));
        assert!(!restored_json.contains("approval-secret"));
    }

    #[test]
    fn duplicate_activation_preserves_the_original_and_global_stop_cancels_future_turns() {
        let server = app_server(SessionStore::open(":memory:").expect("store"));
        let (original, _guard) = server.activate_turn("turn_1").expect("activate turn");

        let duplicate = server.activate_turn("turn_1");

        assert!(matches!(duplicate, Err(AppServerError::Workspace(_))));
        assert!(!original.is_cancelled());
        server
            .request_execution_stop()
            .expect("request execution stop");
        assert!(original.is_cancelled());
        let (late, _late_guard) = server.activate_turn("turn_2").expect("late activation");
        assert!(late.is_cancelled());
    }

    #[test]
    fn workspace_tool_binding_failure_is_a_typed_app_server_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing-workspace");

        assert!(matches!(
            workspace_tools(missing, Arc::new(CompletedSandboxBackend)),
            Err(AppServerError::Workspace(message))
                if message.contains("workspace tool read failed")
        ));
    }

    #[test]
    fn workspace_binding_failure_precedes_running_turn_persistence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        std::fs::remove_dir(&workspace).expect("remove workspace before turn");
        let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

        let response = server
            .turn_start(JsonRpcMessage::request(
                Method::TurnStart,
                json!(1),
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "must not persist"}],
                }),
            ))
            .expect("turn response");

        assert!(
            response[0]["error"]["message"]
                .as_str()
                .expect("error message")
                .contains("workspace tool read failed")
        );
        let history = server
            .store
            .read_thread_history(&thread.thread_id, None, 8)
            .expect("thread history");
        assert!(history.messages.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn persisted_workspace_replacement_with_junction_is_not_rebound() {
        use std::os::windows::process::CommandExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        let retained = dir.path().join("retained-workspace");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&workspace).expect("create workspace");
        std::fs::create_dir(&outside).expect("create outside");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        std::fs::rename(&workspace, &retained).expect("replace workspace namespace");
        let link_arg = format!("\"{}\"", workspace.display());
        let target_arg = format!("\"{}\"", outside.display());
        let output = std::process::Command::new("cmd.exe")
            .raw_arg("/d /c ")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link_arg)
            .raw_arg(&target_arg)
            .output()
            .expect("create junction process");
        if !output.status.success() {
            return;
        }

        let error = workspace_tools_for_thread(&thread, Arc::new(CompletedSandboxBackend))
            .expect_err("replacement junction must fail closed");
        assert!(
            error.contains("workspace tool read failed")
                || error.contains("workspace root")
                || error.contains("reparse"),
            "unexpected workspace binding error: {error}"
        );
        std::fs::remove_dir(&workspace).expect("remove junction");
    }

    #[test]
    fn monitor_open_failure_does_not_publish_active_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let server = app_server(SessionStore::open(&db_path).expect("store"));
        std::fs::hard_link(&db_path, dir.path().join("sessions-alias.sqlite3"))
            .expect("hard link store");

        assert!(matches!(
            server.activate_turn("turn_monitor_failure"),
            Err(AppServerError::Store(StoreError::InvalidState(message)))
                if message.contains("hard links")
        ));
        assert!(
            server
                .active_turns
                .lock()
                .expect("active turn registry")
                .is_empty()
        );
    }

    #[test]
    fn turn_start_monitor_failure_does_not_persist_a_running_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
        std::fs::hard_link(&db_path, dir.path().join("sessions-alias.sqlite3"))
            .expect("hard link store");
        let message: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "turn/start",
            "params": {
                "threadId": &thread.thread_id,
                "input": [{"type": "text", "text": "must not persist"}]
            }
        }))
        .expect("turn/start message");

        assert!(matches!(
            server.handle_turn_start_streaming(message, |_| {}),
            Err(AppServerError::Store(StoreError::InvalidState(message)))
                if message.contains("hard links")
        ));
        assert!(
            server
                .active_turns
                .lock()
                .expect("active registry")
                .is_empty()
        );
        server
            .store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("no running turn was persisted before monitor setup");
    }

    #[test]
    fn stopped_execution_does_not_consume_a_pending_tool_approval() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            "approval_stopped_execution",
            thread.thread_id,
            turn.turn_id,
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1");
        let checkpoint = json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": "call_1",
            "tool_name": TOOL_EDIT,
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        });
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(checkpoint),
                "approval",
                "approval requested",
            )
            .expect("pending approval");
        let mut server = app_server(store);
        server
            .request_execution_stop()
            .expect("request execution stop");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let message = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "approval/decision",
            "id": 1,
            "params": decision,
        }))
        .expect("approval decision message");

        let response = server
            .approval_decision(message)
            .expect("decision response");

        assert_eq!(response[0]["error"]["message"], EXECUTION_STOPPED);
        assert_eq!(
            server
                .store
                .get_pending_approval(&request.request_id)
                .expect("approval remains pending"),
            request
        );
        assert!(
            server
                .store
                .has_pending_tool_call(&request.request_id)
                .expect("checkpoint remains pending")
        );
    }

    struct StaticProvider {
        responses: Vec<ModelTurnResponse>,
        seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    }

    impl Provider for StaticProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
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

    fn failed_model_response(error: ModelError) -> ModelTurnResponse {
        let mut response = ModelTurnResponse::completed("request_1", "response_1", "unused");
        response.status = ModelTurnStatus::Failed;
        response.assistant_message = None;
        response.error = Some(error);
        response
    }

    #[test]
    fn app_server_preserves_typed_provider_failure_category() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "user goal".to_string(),
            }],
        };
        let provider = StaticProvider {
            responses: vec![failed_model_response(ModelError::new(
                ModelErrorKind::AuthError,
                "provider failure remains diagnostic text",
            ))],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        };

        let status = app_server(store)
            .run_agent_loop_with_provider(
                provider,
                &thread,
                &params,
                "turn_1",
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");

        assert_eq!(status.status, AgentStatus::Failed);
        assert_eq!(
            status.error_category,
            Some(ModelErrorCategory::Authentication)
        );
        assert!(
            !serde_json::to_string(&status)
                .expect("serialize status")
                .contains("error_category")
        );
    }

    #[test]
    fn agent_loop_loads_bounded_agents_md_from_thread_cwd() {
        let temp = tempfile::tempdir().expect("temp dir");
        let ancestor = temp
            .path()
            .join("SINGULARITY_API_KEY=must-not-leak")
            .join("ancestor");
        let workspace = ancestor.join("workspace");
        std::fs::create_dir_all(ancestor.join(".git")).expect("ancestor git marker");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(ancestor.join("AGENTS.md"), "ancestor instructions")
            .expect("ancestor agents");
        std::fs::write(workspace.join("AGENTS.md"), "workspace instructions")
            .expect("workspace agents");
        std::fs::write(workspace.join("AGENTS.override.md"), "workspace override")
            .expect("workspace override");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
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
        let developer = &requests[0].messages[0].content;
        assert!(developer.starts_with("You are a coding agent working in the current workspace."));
        assert!(developer.ends_with("Project instructions:\nworkspace override"));
        assert!(!developer.contains("ancestor instructions"));
        assert_eq!(requests[0].messages[1].content, "user goal");
        let hidden_workspace_marker = workspace.to_string_lossy();
        assert!(!requests[0].tools.iter().any(|tool| {
            serde_json::to_string(tool)
                .expect("serialize tool")
                .contains(hidden_workspace_marker.as_ref())
        }));
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
        store
            .update_turn_state(
                &blocked.turn_id,
                TurnStatus::Interrupted,
                AgentStatus::Cancelled.as_str(),
            )
            .expect("release blocked fixture");

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
        let developer = &requests[0].messages[0].content;
        assert!(developer.starts_with("You are a coding agent working in the current workspace."));
        assert!(developer.ends_with("Project instructions:\nproject instructions"));
        assert_eq!(
            requests[0].messages[1..]
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["previous user", "previous assistant", "current user",]
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
        let command = workspace_tool_entries()
            .into_iter()
            .find(|entry| entry.spec.name == TOOL_COMMAND)
            .expect("command tool entry");
        let properties = command
            .spec
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("command properties");

        assert!(!properties.contains_key("sandbox_mode"));
        assert!(!properties.contains_key("network_access"));
    }

    #[test]
    fn app_server_registers_the_agent_control_tools() {
        let registry = workspace_tool_registry();
        let plan = registry
            .get(singularity_agent::UPDATE_PLAN_TOOL)
            .expect("plan tool registered");
        assert_eq!(plan.input_schema["properties"]["steps"]["maxItems"], 64);
        assert_eq!(plan.input_schema["additionalProperties"], false);
    }

    #[test]
    fn committed_plan_terminal_path_emits_independent_item_events() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "inspect the workspace"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "inspect the workspace".to_string(),
            }],
        };
        let plan_arguments = json!({
            "steps": [{"step": "inspect the workspace", "status": "completed"}]
        });
        let mut plan_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
        plan_response.tool_calls.push(ModelToolCall {
            tool_call_id: "plan_call_1".to_string(),
            tool_name: "update_plan".to_string(),
            raw_arguments: plan_arguments.to_string(),
            arguments: plan_arguments,
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
        let server = app_server(store);
        let status = server
            .run_agent_loop_with_provider(
                StaticProvider {
                    responses: vec![plan_response, final_response],
                    seen_requests: Arc::new(Mutex::new(Vec::new())),
                },
                &thread,
                &params,
                &turn.turn_id,
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");
        assert_eq!(status.status, AgentStatus::Completed);
        assert!(status.plan.is_some());

        let committed = server
            .commit_turn_run_status(turn, &status, &CancellationToken::new())
            .expect("commit terminal outcome");
        let plan_item = committed.plan_item.as_ref().expect("plan item");
        assert_eq!(plan_item.kind, ItemKind::Plan);
        let events = server
            .committed_turn_events(&committed)
            .expect("terminal events");
        let methods = events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "item/started",
                "item/completed",
                "turn/plan/updated",
                "item/started",
                "item/agentMessage/delta",
                "item/completed",
                "turn/completed",
            ]
        );
        assert_eq!(events[0]["params"]["item"]["item_id"], plan_item.item_id);
        assert_eq!(events[1]["params"]["item"]["item_id"], plan_item.item_id);
        assert_eq!(events[2]["params"]["plan"], plan_item.payload);
        assert!(events[0]["params"].get("delta").is_none());
        assert!(events[1]["params"].get("delta").is_none());
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
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1")
        .with_resources([workspace_resource("README.md")]);
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
                None,
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
    fn approval_resume_uses_stored_policy_snapshot_instead_of_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread_with_policy(
                Some("gpt-test"),
                Some(&workspace.to_string_lossy()),
                PermissionProfileName::WorkspaceWrite,
                ApprovalPolicy::Never,
            )
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

        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1")
        .with_resources([workspace_resource("README.md")]);
        let arguments = json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        });
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_EDIT),
            raw_arguments: arguments.to_string(),
            resources: request.resources.clone(),
        };
        let pending_payload = json!({
            "request_id": pending.request_id,
            "thread_id": thread.thread_id,
            "turn_id": turn.turn_id,
            "tool_call_id": pending.tool_call_id,
            "tool_name": pending.tool_name,
            "raw_arguments": pending.raw_arguments,
            "resources": pending.resources,
            "checkpoint_version": 1,
            "messages": [{
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "tool_call_id": "call_1",
                    "tool_name": TOOL_EDIT,
                    "arguments": arguments.clone(),
                    "raw_arguments": arguments.to_string(),
                    "parse_status": "valid",
                    "validation_errors": []
                }]
            }],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {
                "workspace_mutated": false,
                "successful_command_count": 0,
                "required_command_counts": {},
                "terminal_command_scope_digests": [],
                "unresolved_failures": []
            },
            "last_completion_error": null
        });
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let resumed = app_server(store)
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(pending_payload),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "done",
                    )],
                    seen_requests: Arc::clone(&seen_requests),
                },
                &CancellationToken::new(),
                Some(
                    WorkspaceTools::new(&workspace)
                        .expect("bind workspace tools")
                        .with_sandbox_backend(CompletedSandboxBackend),
                ),
            )
            .expect("resume")
            .expect("terminal status");

        assert_eq!(resumed.1.status, AgentStatus::Failed);
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
        assert!(
            seen_requests.lock().expect("seen requests").len() <= 1,
            "the denied continuation must not execute or continue through another model turn"
        );
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
            tool_id(TOOL_COMMAND),
        )
        .with_tool_call_id("call_1");
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_COMMAND),
            raw_arguments: json!({
                "command": "test-program success",
                "timeout_seconds": 5
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
            run_status.audit_events[0]["sandbox_mode"],
            "workspace_write"
        );
        assert_eq!(run_status.audit_events[0]["network_access"], "denied");
        assert_eq!(
            run_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert_eq!(
            run_status.audit_events[0]["sandbox_enforcement"],
            "not_executed"
        );
        assert_eq!(
            run_status.audit_events[0]["command_scope_digest"],
            "unavailable"
        );
        assert_eq!(
            run_status.audit_events[0]["policy_scope_binding"],
            "unavailable"
        );
        let serialized = serde_json::to_string(&run_status.audit_events[0]).expect("audit JSON");
        assert!(!serialized.contains("raw_arguments"));
        assert!(!serialized.contains("test-program success"));
        assert!(!serialized.contains("approval_request_id"));
        assert!(!serialized.contains("approval_decision_id"));
        assert_eq!(
            run_status.audit_events[0]["approval_decision"],
            "unavailable"
        );
    }

    #[test]
    fn approval_resolution_cancellation_wins_without_persisting_a_next_approval() {
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
                json!([{"type": "text", "text": "edit"}]),
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
        let checkpoint = |request: &ApprovalRequest, tool_call_id: &str| {
            json!({
                "request_id": &request.request_id,
                "thread_id": &request.thread_id,
                "turn_id": &request.turn_id,
                "tool_call_id": tool_call_id,
                "tool_name": &request.action,
                "raw_arguments": "{}",
                "resources": &request.resources,
                "checkpoint_version": 1,
                "messages": [],
                "tool_results": [],
                "used_approval_grants": [],
                "approval_count": 1,
                "model_turns": 1,
                "completion": {}
            })
        };
        let request = ApprovalRequest::new(
            "approval_cancel_race",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1");
        let pending_payload = checkpoint(&request, "call_1");
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(pending_payload.clone()),
                "approval",
                "approval requested",
            )
            .expect("approval");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("claim execution");
        let cancellation_trace = TraceEvent {
            payload: json!({"turn_id": &turn.turn_id, "agent_loop_status": "cancel_requested"}),
            ..TraceEvent::for_turn(
                "trace_cancel_race",
                thread.thread_id.clone(),
                turn.turn_id.clone(),
                "app_server",
                "turn interrupt requested",
            )
        };
        store
            .request_turn_cancellation(&turn.turn_id, &cancellation_trace)
            .expect("request cancellation");
        let server = app_server(store);
        let (_turn, no_resume_status) = server
            .approval_no_resume_status(&request, &decision, Some(&pending_payload))
            .expect("no-resume status")
            .expect("terminal status");
        assert_eq!(no_resume_status.status, AgentStatus::Cancelled);

        let next = ApprovalRequest::new(
            "approval_must_not_persist",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_2");
        let stale_status = AgentRunStatus::failed("stale local result");
        let committed = server
            .commit_effective_turn_status_resolving_approval(
                &request.request_id,
                &turn,
                &stale_status,
                &[(next.clone(), checkpoint(&next, "call_2"))],
            )
            .expect("cancellation wins approval resolution");
        assert_eq!(committed.turn.status, TurnStatus::Interrupted);
        assert_eq!(committed.turn.agent_loop_status, "cancelled");
        assert!(
            !server
                .store
                .has_pending_tool_call(&request.request_id)
                .expect("old execution")
        );
        assert!(matches!(
            server.store.get_pending_approval(&next.request_id),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn initial_approval_handoff_interruption_is_an_idempotent_terminal_commit() {
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
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "edit"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let request = ApprovalRequest::new(
            "approval_initial_interrupt",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1");
        let checkpoint = json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": "call_1",
            "tool_name": TOOL_EDIT,
            "raw_arguments": "{}",
            "resources": [],
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        });
        store
            .create_approval_with_pending_tool_call_and_trace(
                &request,
                Some(checkpoint),
                "approval",
                "approval requested",
            )
            .expect("persist initial approval");
        let interrupt_trace = TraceEvent {
            payload: json!({"turn_id": &turn.turn_id, "agent_loop_status": "cancel_requested"}),
            ..TraceEvent::for_turn(
                "trace_interrupt_initial_handoff",
                thread.thread_id.clone(),
                turn.turn_id.clone(),
                "app_server",
                "turn interrupt requested",
            )
        };
        let interrupted = store
            .request_turn_cancellation(&turn.turn_id, &interrupt_trace)
            .expect("interrupt pending approval");
        assert_eq!(interrupted.status, TurnStatus::Interrupted);
        let server = app_server(store);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let stale_blocked =
            AgentRunStatus::failed("stale blocked result").with_status(AgentStatus::Blocked);
        let committed = server
            .commit_turn_run_status(turn.clone(), &stale_blocked, &cancellation)
            .expect("interrupted handoff is idempotent");
        assert_eq!(committed.turn.status, TurnStatus::Interrupted);
        assert_eq!(committed.turn.agent_loop_status, "cancelled");
        assert!(
            server
                .store
                .list_pending_approvals()
                .expect("pending")
                .is_empty()
        );
        server
            .store
            .recover_unowned_workspace_executions()
            .expect("recovery");
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
            tool_id(TOOL_COMMAND),
        )
        .with_tool_call_id("call_1");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let valid_command = json!({
            "command": "test-program success",
            "timeout_seconds": 5
        })
        .to_string();
        let bound_resource = command_resource(command_script_scope_digest_with_policy(
            "test-program success",
            ".",
            5,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Allowed,
        ));
        let mismatched_pending = PendingToolCall {
            request_id: "approval_other_call_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_COMMAND),
            raw_arguments: valid_command.clone(),
            resources: vec![bound_resource.clone()],
        };
        let invalid_args_pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_COMMAND),
            raw_arguments: "{not-json".to_string(),
            resources: Vec::new(),
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let server = app_server(store);

        let (_turn, mismatch_status, _next_approvals) = server
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
                None,
            )
            .expect("resume")
            .expect("terminal status");
        assert_eq!(mismatch_status.status, AgentStatus::Failed);
        assert_eq!(
            mismatch_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert_eq!(
            mismatch_status.audit_events[0]["command_scope_digest"],
            match &bound_resource {
                PermissionResource::CommandScope(digest) => digest.as_str(),
                _ => panic!("bound command resource must be typed"),
            }
        );
        assert_eq!(
            mismatch_status.audit_events[0]["policy_scope_binding"],
            "bound"
        );

        let (_turn, invalid_args_status, _next_approvals) = server
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
                None,
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
        let db_path = dir.path().join("sessions.sqlite3");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("AGENTS.md"), "stable project instructions")
            .expect("stable agents");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(&db_path).expect("store");
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
        let history = store
            .read_thread_history_before_turn(
                &thread.thread_id,
                &turn.turn_id,
                DEFAULT_THREAD_HISTORY_TURN_LIMIT,
            )
            .expect("history before approval turn");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "edit readme".to_string(),
            }],
        };
        let mut initial_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before approval");
        initial_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_EDIT.to_string(),
            arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            }),
            raw_arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let mut verification_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
        verification_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_verify".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            arguments: json!({
                "command": "cmd.exe /C \"echo verified\"",
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "command": "cmd.exe /C \"echo verified\"",
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
        let initial_seen_requests = Arc::new(Mutex::new(Vec::new()));
        let initial_provider = StaticProvider {
            responses: vec![initial_response],
            seen_requests: Arc::clone(&initial_seen_requests),
        };
        let resumed_seen_requests = Arc::new(Mutex::new(Vec::new()));
        let resumed_provider = StaticProvider {
            responses: vec![verification_response, final_response],
            seen_requests: Arc::clone(&resumed_seen_requests),
        };
        let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

        let cancellation = CancellationToken::new();
        let blocked_status = server
            .run_agent_loop_with_provider(
                initial_provider,
                &thread,
                &params,
                &turn.turn_id,
                &history.messages,
                &cancellation,
            )
            .expect("initial agent loop");
        assert_eq!(blocked_status.status, AgentStatus::Blocked);
        assert_eq!(blocked_status.approval_count, 1);
        server
            .commit_turn_run_status(turn.clone(), &blocked_status, &cancellation)
            .expect("commit blocked turn");
        let blocked_json = serde_json::to_string(&blocked_status).expect("blocked status json");
        assert!(!blocked_json.contains("checkpoint_version"));
        assert!(!blocked_json.contains("raw_arguments"));
        for trace in server
            .store
            .list_trace(&thread.thread_id)
            .expect("thread trace")
        {
            let trace_json = serde_json::to_string(&trace.payload).expect("trace payload json");
            assert!(!trace_json.contains("checkpoint_version"));
            assert!(!trace_json.contains("raw_arguments"));
        }
        drop(server);
        let server = app_server(SessionStore::open(&db_path).expect("reopen store"))
            .with_sandbox_backend(CompletedSandboxBackend);
        let request = server
            .store
            .get_pending_approval(&format!("approval_{}_call_1", turn.turn_id))
            .expect("stored approval");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let recorded = server
            .store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("record approval");
        let pending_payload = recorded.pending_tool_call.expect("checkpoint payload");
        assert_eq!(pending_payload["checkpoint_version"], 1);
        assert!(
            pending_payload["project_instructions_digest"]
                .as_str()
                .is_some_and(|digest| digest.starts_with("sha256:"))
        );
        assert!(pending_payload["messages"].is_array());
        assert!(pending_payload["tool_results"].is_array());

        let resumed = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(pending_payload),
                resumed_provider,
                &CancellationToken::new(),
                Some(
                    WorkspaceTools::new(&workspace)
                        .expect("bind workspace tools")
                        .with_sandbox_backend(CompletedSandboxBackend),
                ),
            )
            .expect("resume")
            .expect("resumed");

        assert_eq!(resumed.0.turn_id, turn.turn_id);
        assert_eq!(resumed.1.status, AgentStatus::Completed);
        assert_eq!(resumed.1.final_answer.as_deref(), Some("done"));
        assert_eq!(resumed.1.model_turns, 3);
        assert_eq!(resumed.1.tool_calls, 2);
        assert_eq!(resumed.1.approval_count, 1);
        assert!(resumed.1.verification.required);
        assert!(resumed.1.verification.passed);
        assert_eq!(resumed.1.verification.successful_command_count, 1);
        let committed = server
            .commit_effective_turn_status_resolving_approval(
                &request.request_id,
                &resumed.0,
                &resumed.1,
                &resumed.2,
            )
            .expect("commit resumed outcome");
        assert_eq!(committed.turn.status, TurnStatus::Completed);
        let terminal_trace = server
            .store
            .list_trace(&thread.thread_id)
            .expect("thread trace")
            .into_iter()
            .find(|trace| trace.component == "agent_loop" && trace.payload["status"] == "completed")
            .expect("terminal agent trace");
        assert!(terminal_trace.payload.get("tool_outcomes").is_none());
        let terminal_trace_json =
            serde_json::to_string(&terminal_trace.payload).expect("terminal trace json");
        for full_result_field in ["content", "preview", "artifact_refs", "result_id"] {
            assert!(!terminal_trace_json.contains(full_result_field));
        }
        assert!(
            !server
                .store
                .has_pending_tool_call(&request.request_id)
                .expect("pending lookup")
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "after"
        );
        let requests = resumed_seen_requests.lock().expect("seen requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
        assert_eq!(requests[0].messages[1].role, ModelRole::User);
        assert_eq!(requests[0].messages[1].content, "previous approval user");
        assert_eq!(requests[0].messages[2].role, ModelRole::Assistant);
        assert_eq!(
            requests[0].messages[2].content,
            "previous approval assistant"
        );
        assert_eq!(requests[0].messages[3].role, ModelRole::User);
        assert_eq!(requests[0].messages[3].content, "edit readme");
        assert_eq!(requests[0].messages[4].role, ModelRole::Assistant);
        assert_eq!(requests[0].messages[4].content, "before approval");
        assert_eq!(requests[0].messages[4].tool_calls.len(), 1);
        assert_eq!(requests[0].messages[5].role, ModelRole::Tool);
        assert_eq!(
            requests[0].messages[5].tool_call_id.as_deref(),
            Some("call_1")
        );
    }

    #[test]
    fn agent_loop_approval_resume_fails_closed_when_project_instructions_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("AGENTS.md"), "initial project instructions")
            .expect("initial agents");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(&db_path).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "edit readme".to_string(),
            }],
        };
        let mut initial_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before");
        initial_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_EDIT.to_string(),
            arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            }),
            raw_arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let initial_seen_requests = Arc::new(Mutex::new(Vec::new()));
        let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
        let blocked_status = server
            .run_agent_loop_with_provider(
                StaticProvider {
                    responses: vec![initial_response],
                    seen_requests: Arc::clone(&initial_seen_requests),
                },
                &thread,
                &params,
                &turn.turn_id,
                &[],
                &CancellationToken::new(),
            )
            .expect("initial agent loop");
        assert_eq!(blocked_status.status, AgentStatus::Blocked);
        server
            .commit_turn_run_status(turn.clone(), &blocked_status, &CancellationToken::new())
            .expect("commit blocked turn");
        assert!(
            !serde_json::to_string(&blocked_status)
                .expect("blocked status json")
                .contains("project_instructions_digest")
        );
        let initial_request_json =
            serde_json::to_string(&initial_seen_requests.lock().expect("initial requests")[0])
                .expect("initial request json");
        assert!(!initial_request_json.contains("AGENTS.md"));
        assert!(!initial_request_json.contains(workspace.to_string_lossy().as_ref()));
        for trace in server
            .store
            .list_trace(&thread.thread_id)
            .expect("thread trace")
        {
            assert!(
                !serde_json::to_string(&trace.payload)
                    .expect("trace json")
                    .contains("project_instructions_digest")
            );
        }
        drop(server);

        std::fs::write(
            workspace.join("AGENTS.override.md"),
            "replacement project instructions",
        )
        .expect("override agents");
        let server = app_server(SessionStore::open(&db_path).expect("reopen store"))
            .with_sandbox_backend(CompletedSandboxBackend);
        let request = server
            .store
            .get_pending_approval(&format!("approval_{}_call_1", turn.turn_id))
            .expect("stored approval");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let recorded = server
            .store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("record approval");
        let pending_payload = recorded.pending_tool_call.expect("checkpoint payload");
        let checkpoint_digest = pending_payload["project_instructions_digest"]
            .as_str()
            .expect("checkpoint project instruction digest");
        assert!(checkpoint_digest.starts_with("sha256:"));

        let resumed_seen_requests = Arc::new(Mutex::new(Vec::new()));
        let resumed = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(pending_payload),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "must not run",
                    )],
                    seen_requests: Arc::clone(&resumed_seen_requests),
                },
                &CancellationToken::new(),
                Some(
                    WorkspaceTools::new(&workspace)
                        .expect("bind workspace tools")
                        .with_sandbox_backend(CompletedSandboxBackend),
                ),
            )
            .expect("resume")
            .expect("terminal status");

        assert_eq!(resumed.1.status, AgentStatus::Failed);
        assert_eq!(
            resumed.1.error.as_deref(),
            Some("approval checkpoint project instructions digest mismatch")
        );
        assert!(
            resumed_seen_requests
                .lock()
                .expect("resumed requests")
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
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

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }
    }

    struct UnavailableSandboxBackend;

    impl SandboxBackend for UnavailableSandboxBackend {
        fn name(&self) -> &'static str {
            "unavailable_test"
        }

        fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
            singularity_tools::SandboxCapabilities::unavailable()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::sandbox_backend_unavailable(&request.command_id)
        }

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            CommandResult::sandbox_backend_unavailable(&request.command_id)
        }
    }

    #[test]
    fn agent_loop_capability_is_projected_from_the_bound_sandbox_backend() {
        let available = agent_loop_capability(&CompletedSandboxBackend);
        assert!(available.available);
        assert!(available.blockers.is_empty());
        assert!(available.reason.contains("completed_test"));

        let unavailable = agent_loop_capability(&UnavailableSandboxBackend);
        assert!(!unavailable.available);
        assert_eq!(
            unavailable.blockers,
            vec![STRICT_COMMAND_SANDBOX_UNAVAILABLE]
        );
        assert!(unavailable.reason.contains("unavailable_test"));
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
            tool_name: "command".to_string(),
            arguments: json!({
                "command": "cmd.exe /C \"echo app-server-sandbox-ok\"",
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "command": "cmd.exe /C \"echo app-server-sandbox-ok\"",
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
