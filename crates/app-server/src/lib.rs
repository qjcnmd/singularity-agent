#![forbid(unsafe_code)]

//! 在进程边界负责 turn 准入、`AgentLoop` 执行、持久化和取消的 JSON-RPC 应用服务。
//!
//! 服务将协议处理与工作线程执行分离，并通过 `SessionStore` 提交终态后再发出对应事件。

mod approval;
mod dispatch;
mod evaluation;
mod events;
mod lifecycle;
mod observability;

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{Builder as ThreadBuilder, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use singularity_agent::{
    AgentContextItem, AgentLoop, AgentLoopCapability, AgentLoopEvent, AgentLoopEventSinkError,
    AgentLoopInput, AgentLoopResult, AgentRunStatus, AgentStatus, ApprovalGrant,
    PendingApprovalOccurrence, agent_control_tool_entries, project_audit_event,
};
use singularity_core::{
    CancellationToken, ErrorCode, JSON_RPC_INTERNAL_ERROR, ProjectInstructionError,
    contains_sensitive_text, load_project_instructions,
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
    ConversationMessage, ConversationRole, EvalRunParams, EventClass, EventDelivery, EventGap,
    EventGapReason, EventMetadata, EventRecoveryQuery, EventSubscribeParams, EventSubscribeResult,
    InitializeParams, InitializeResult, Item, JsonRpcId, JsonRpcMessage, Method, MethodKind,
    ProviderConfigurationStatus, ServerCapabilitiesResult, ServerShutdownResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadReadParams, ThreadReadResult, ThreadResult, ThreadStartParams, ThreadStartResult,
    TraceEvent, TransportCapability, Turn, TurnIdParams, TurnInterruptResult, TurnResult,
    TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_sandbox::{PlatformSandboxBackend, SandboxBackend, SandboxBackendEnforcement};
use singularity_store::{
    AllocatedAssistantItemId, CommitTurnOutcomeParams, CommittedTurnOutcome,
    CreateStartedTurnParams, SessionStore, StoreError, TurnOutcomeAuthority,
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
const SAFE_PROVIDER_FAILURE: &str = "provider request failed";
const SAFE_PROJECT_INSTRUCTIONS_FAILURE: &str = "project instructions unavailable";
const SAFE_WORKSPACE_FAILURE: &str = "workspace capability unavailable";
const SAFE_AGENT_LOOP_FAILURE: &str = "agent loop execution failed";
const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";
const APP_ERROR_INVALID_STATE: i64 = -32005;
const CANCELLATION_MONITOR_FROZEN: u8 = 0x80;
const CANCELLATION_MONITOR_OUTCOME_MASK: u8 = !CANCELLATION_MONITOR_FROZEN;

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
    #[error("turn execution failed during {stage} ({cause})")]
    TurnExecution {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
    },
    #[error("turn execution failed during {stage} ({cause}); terminalization failed ({failure})")]
    TurnTerminalization {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        failure: TurnTerminalizationFailure,
    },
}

/// `AppServer` 请求处理和生命周期操作使用的结果类型。
pub type AppServerResult<T> = Result<T, AppServerError>;

/// 已持久化 Running Turn 的失败阶段；仅暴露稳定分类，不携带底层错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureStage {
    AgentLoop,
    ApprovalCheckpoint,
    CancellationMonitor,
    TerminalOutcome,
    EventNotification,
}

impl TurnFailureStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::ApprovalCheckpoint => "approval_checkpoint",
            Self::CancellationMonitor => "cancellation_monitor",
            Self::TerminalOutcome => "terminal_outcome",
            Self::EventNotification => "event_notification",
        }
    }
}

impl fmt::Display for TurnFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 已进入 Running Turn 后失败的稳定原始原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureCause {
    Store,
    Workspace,
    ProjectInstructions,
    Serialization,
    CancellationMonitor,
    StoredInputUnavailable,
    Internal,
}

impl TurnFailureCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Workspace => "workspace",
            Self::ProjectInstructions => "project_instructions",
            Self::Serialization => "serialization",
            Self::CancellationMonitor => "cancellation_monitor",
            Self::StoredInputUnavailable => "stored_input_unavailable",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 终态补偿失败的稳定分类；不把 SQLite 路径、SQL 或原始错误带到协议边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTerminalizationFailure {
    Store,
    StateChanged,
    EventNotification,
}

impl fmt::Display for TurnTerminalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
            Self::StateChanged => "state_changed",
            Self::EventNotification => "event_notification",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TurnFailure {
    stage: TurnFailureStage,
    cause: TurnFailureCause,
}

impl From<TurnFailureStage> for TurnFailure {
    fn from(stage: TurnFailureStage) -> Self {
        Self {
            stage,
            cause: match stage {
                TurnFailureStage::ApprovalCheckpoint => TurnFailureCause::Store,
                TurnFailureStage::CancellationMonitor => TurnFailureCause::CancellationMonitor,
                _ => TurnFailureCause::Internal,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationMonitorOutcome {
    UserCancellation,
    InfrastructureFailure,
}

impl CancellationMonitorOutcome {
    const USER_CANCELLATION_CODE: u8 = 1;
    const INFRASTRUCTURE_FAILURE_CODE: u8 = 2;

    const fn code(self) -> u8 {
        match self {
            Self::UserCancellation => Self::USER_CANCELLATION_CODE,
            Self::InfrastructureFailure => Self::INFRASTRUCTURE_FAILURE_CODE,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code & CANCELLATION_MONITOR_OUTCOME_MASK {
            Self::USER_CANCELLATION_CODE => Some(Self::UserCancellation),
            Self::INFRASTRUCTURE_FAILURE_CODE => Some(Self::InfrastructureFailure),
            _ => None,
        }
    }
}

fn monitor_infrastructure_failure(control: Option<&CancellationMonitorControl>) -> bool {
    control.and_then(|control| {
        CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst))
    }) == Some(CancellationMonitorOutcome::InfrastructureFailure)
}

struct AgentLoopInvocation<'a> {
    thread: &'a Thread,
    params: &'a TurnStartParams,
    turn_id: &'a str,
    history: &'a [ConversationMessage],
    cancellation: &'a CancellationToken,
    monitor_control: Option<&'a CancellationMonitorControl>,
}

/// 一次 AgentLoop 调用预分配的 assistant item 及实际通过订阅过滤器生成的事件状态。
struct AssistantItemEventState {
    item_id: AllocatedAssistantItemId,
    first_delta_observed: bool,
    started_generated: bool,
    delta_generated: bool,
}

impl AssistantItemEventState {
    fn new(item_id: AllocatedAssistantItemId) -> Self {
        Self {
            item_id,
            first_delta_observed: false,
            started_generated: false,
            delta_generated: false,
        }
    }

    fn appeared(&self) -> bool {
        self.started_generated || self.delta_generated
    }
}

struct ApprovalResumeContext<'a> {
    cancellation: &'a CancellationToken,
    monitor_control: Option<&'a CancellationMonitorControl>,
    prepared_workspace_tools: Option<WorkspaceTools>,
}

struct ApprovalResumeInput<'a> {
    request: &'a ApprovalRequest,
    decision: &'a ApprovalDecision,
    turn: &'a Turn,
    thread: &'a Thread,
    pending_approval: Option<PendingApprovalOccurrence>,
}

struct ApprovalTerminalizationContext<'a> {
    turn: &'a Turn,
    thread: &'a Thread,
    prior_status: Option<&'a AgentRunStatus>,
    cancellation: &'a CancellationToken,
    monitor_outcome: Option<CancellationMonitorOutcome>,
    failure: TurnFailure,
}

enum TurnTerminalizationResult {
    Committed(Box<CommittedTurnOutcome>),
    Preserved,
}

/// stdout transport 使用的全局输出顺序与事件 cursor reservation。
///
/// 一个 reservation 同时占用输出 order 和可选事件 cursor；request worker 在交给
/// stdout queue 前先取得两者，避免事件 cursor 已分配而 worker 尚未排队时被并发 worker 越过。
#[derive(Clone, Debug, Default)]
pub struct OutputOrderCoordinator {
    state: Arc<Mutex<OutputOrderState>>,
}

#[derive(Debug, Default)]
struct OutputOrderState {
    next_order: u64,
    next_event_cursor: u64,
    next_write_order: u64,
    in_flight: BTreeSet<u64>,
    ready: BTreeSet<u64>,
    skipped: BTreeSet<u64>,
}

/// 一个已经原子预留的 stdout 输出位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputReservation {
    /// 全局 stdout 输出顺序。
    pub order: u64,
    /// 事件输出对应的事件 cursor；控制输出为 `None`。
    pub event_cursor: Option<u64>,
}

impl OutputOrderCoordinator {
    /// 创建新的进程级输出 reservation 协调器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 原子预留一个输出 order，并按需分配连续事件 cursor。
    pub fn reserve(&self, event: bool) -> Result<OutputReservation, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "output ordering state poisoned".to_string())?;
        let order = state.next_order;
        let next_order = state
            .next_order
            .checked_add(1)
            .ok_or_else(|| "output order exhausted".to_string())?;
        let event_cursor = if event {
            let cursor = state
                .next_event_cursor
                .checked_add(1)
                .ok_or_else(|| "event cursor exhausted".to_string())?;
            state.next_event_cursor = cursor;
            Some(cursor)
        } else {
            None
        };
        state.next_order = next_order;
        state.in_flight.insert(order);
        Ok(OutputReservation {
            order,
            event_cursor,
        })
    }

    /// 返回当前已观察到的最大事件 cursor。
    pub fn current_event_cursor(&self) -> Result<u64, String> {
        self.state
            .lock()
            .map(|state| state.next_event_cursor)
            .map_err(|_| "output ordering state poisoned".to_string())
    }

    /// 丢弃未进入 transport queue 的 reservation，避免 direct API 阻塞后续输出。
    pub fn complete(&self, order: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.in_flight.remove(&order);
            state.skipped.insert(order);
            advance_write_order(&mut state);
        }
    }

    /// 丢弃一段未进入 transport queue 的 reservation。
    pub fn complete_range(&self, from_order: u64, to_order: u64) {
        if let Ok(mut state) = self.state.lock() {
            for order in from_order..=to_order {
                state.in_flight.remove(&order);
                state.skipped.insert(order);
            }
            advance_write_order(&mut state);
        }
    }

    /// 标记一条输出已经进入 transport queue，但仍须由 writer 按 order 写出。
    pub fn enqueue(&self, order: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.in_flight.remove(&order);
            state.ready.insert(order);
        }
    }

    /// 标记一个 gap 输出代表一段连续 reservation。
    pub fn enqueue_range(&self, from_order: u64, to_order: u64) {
        if let Ok(mut state) = self.state.lock() {
            for order in from_order..=to_order {
                state.in_flight.remove(&order);
            }
            state.ready.insert(from_order);
        }
    }

    /// 判断指定输出是否正好轮到 writer 写出。
    pub fn is_next_ready(&self, order: u64) -> bool {
        self.state
            .lock()
            .map(|state| state.next_write_order == order && state.ready.contains(&order))
            .unwrap_or(false)
    }

    /// 写入并 flush 成功后推进 writer 游标；`to_order` 用于 gap 的跳跃。
    pub fn acknowledge_written(&self, from_order: u64, to_order: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.ready.remove(&from_order);
            state.next_write_order = to_order.saturating_add(1);
            advance_write_order(&mut state);
        }
    }
}

fn advance_write_order(state: &mut OutputOrderState) {
    while state.skipped.remove(&state.next_write_order) {
        state.next_write_order = state.next_write_order.saturating_add(1);
    }
}

/// stdout transport 使用的真实持久化 Turn 绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportTraceBinding {
    /// Store trace 的 run/thread identity。
    pub thread_id: String,
    /// Store trace 的 session/turn identity。
    pub turn_id: String,
}

impl TransportTraceBinding {
    /// 从真实持久化 Turn 建立 transport trace 绑定。
    pub fn for_turn(thread_id: impl Into<String>, turn_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
        }
    }
}

/// AppServer 交给 stdout transport 的已排序消息。
#[derive(Debug, Clone)]
pub struct AppServerOutput {
    /// 与其他 worker 共享的全局 stdout order。
    pub reservation: OutputReservation,
    /// 脱敏 JSON-RPC wire value。
    pub message: Value,
    /// 仅由真实 Turn 生产者提供；transport 不从公共 JSON 反推身份。
    pub trace_binding: Option<TransportTraceBinding>,
}

/// 协调线程、turn、approval、追踪和工作线程的有状态 JSON-RPC 服务。
pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
    event_filter: Arc<Mutex<EventSubscriptionState>>,
    shutdown_requested: bool,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    execution_stopped: Arc<AtomicBool>,
    evaluation_cancellation: CancellationToken,
    output_order: OutputOrderCoordinator,
    pending_transport_trace_binding: Option<TransportTraceBinding>,
    #[doc(hidden)]
    pub test_provider_override:
        Option<std::sync::Arc<dyn singularity_model::Provider + Send + Sync>>,
}

/// 由请求工作线程与标准输入输出传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    execution_stopped: Arc<AtomicBool>,
    evaluation_cancellation: CancellationToken,
}

/// 每个 stdio app-server 生命周期共享的事件订阅和传输 cursor。
#[derive(Debug, Default)]
struct EventSubscriptionState {
    event_types: Option<Vec<String>>,
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并将取消传播到每个活动 turn。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        for cancellation in self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .values()
        {
            cancellation.cancel();
        }
        self.evaluation_cancellation.cancel();
        Ok(())
    }
}

struct ActiveTurnGuard {
    turn_id: String,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    cancellation: CancellationToken,
    monitor: Option<CancellationMonitor>,
    stabilized_monitor_outcome: Option<Option<CancellationMonitorOutcome>>,
}

struct CancellationMonitorControl {
    started: AtomicBool,
    stop: AtomicBool,
    outcome: AtomicU8,
    wake: Sender<()>,
}

impl CancellationMonitorControl {
    /// 发布 monitor 结果；基础设施故障可在冻结前优先于用户取消。
    fn record_outcome(&self, outcome: CancellationMonitorOutcome) -> bool {
        let mut state = self.outcome.load(Ordering::SeqCst);
        loop {
            if state & CANCELLATION_MONITOR_FROZEN != 0 {
                return false;
            }
            let current = CancellationMonitorOutcome::from_code(state);
            if current == Some(CancellationMonitorOutcome::InfrastructureFailure)
                || (current == Some(CancellationMonitorOutcome::UserCancellation)
                    && outcome == CancellationMonitorOutcome::UserCancellation)
            {
                return false;
            }
            match self.outcome.compare_exchange(
                state,
                outcome.code(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(next) => state = next,
            }
        }
    }

    /// 在没有强制结果时冻结当前快照，阻止 detached monitor 再发布结果。
    fn freeze(&self) -> Option<CancellationMonitorOutcome> {
        let state = self
            .outcome
            .fetch_or(CANCELLATION_MONITOR_FROZEN, Ordering::SeqCst);
        CancellationMonitorOutcome::from_code(state)
    }

    /// 以基础设施故障原子冻结 monitor；超时本身是 owner 发布的故障结果。
    fn freeze_as_infrastructure_failure(&self) -> (Option<CancellationMonitorOutcome>, bool) {
        let mut state = self.outcome.load(Ordering::SeqCst);
        loop {
            if state & CANCELLATION_MONITOR_FROZEN != 0 {
                return (CancellationMonitorOutcome::from_code(state), false);
            }
            let next = CANCELLATION_MONITOR_FROZEN
                | CancellationMonitorOutcome::InfrastructureFailure.code();
            match self
                .outcome
                .compare_exchange(state, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    return (
                        Some(CancellationMonitorOutcome::InfrastructureFailure),
                        true,
                    );
                }
                Err(next_state) => state = next_state,
            }
        }
    }
}

struct CancellationMonitor {
    control: Arc<CancellationMonitorControl>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    shutdown_wait: Duration,
}

impl CancellationMonitor {
    fn stabilize(self, cancellation: &CancellationToken) -> Option<CancellationMonitorOutcome> {
        let shutdown_wait = self.shutdown_wait;
        self.stabilize_with_timeout(cancellation, shutdown_wait)
    }

    fn stabilize_with_timeout(
        self,
        cancellation: &CancellationToken,
        shutdown_wait: Duration,
    ) -> Option<CancellationMonitorOutcome> {
        self.control.stop.store(true, Ordering::SeqCst);
        let _ = self.control.wake.send(());
        match self.done.recv_timeout(shutdown_wait) {
            Ok(()) => {
                if cancellation.is_cancelled() {
                    let _ = self
                        .control
                        .record_outcome(CancellationMonitorOutcome::UserCancellation);
                }
                self.control.freeze()
            }
            Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => {
                // The monitor may still be inside SQLite. Publish and freeze before
                // detaching so a late read result cannot change the token or outcome.
                let (outcome, published) = self.control.freeze_as_infrastructure_failure();
                if published {
                    cancellation.cancel();
                }
                outcome.or(Some(CancellationMonitorOutcome::InfrastructureFailure))
            }
        }
    }
}

impl ActiveTurnGuard {
    fn start_monitor(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.control.started.store(true, Ordering::SeqCst);
            let _ = monitor.control.wake.send(());
        }
    }

    fn monitor_outcome(&self) -> Option<CancellationMonitorOutcome> {
        if let Some(outcome) = self.stabilized_monitor_outcome {
            return outcome;
        }
        self.monitor.as_ref().and_then(|monitor| {
            CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst))
        })
    }

    fn monitor_control(&self) -> Option<&CancellationMonitorControl> {
        self.monitor
            .as_ref()
            .map(|monitor| monitor.control.as_ref())
    }

    /// 在终态提交前冻结 monitor 结果，避免取消 token 覆盖晚到的基础设施故障。
    fn stabilize_monitor(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Option<CancellationMonitorOutcome> {
        if let Some(outcome) = self.stabilized_monitor_outcome {
            return outcome;
        }
        let outcome = match self.monitor.take() {
            Some(monitor) => monitor.stabilize(cancellation),
            None if cancellation.is_cancelled() => {
                Some(CancellationMonitorOutcome::UserCancellation)
            }
            None => None,
        };
        self.stabilized_monitor_outcome = Some(outcome);
        outcome
    }

    fn teardown_monitor_with_timeout(&mut self, shutdown_wait: Duration) {
        if let Some(mut monitor) = self.monitor.take() {
            monitor.control.stop.store(true, Ordering::SeqCst);
            let _ = monitor.control.wake.send(());
            match monitor.done.recv_timeout(shutdown_wait) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    // `done` is sent after the final monitor operation; dropping the
                    // handle avoids an unbounded join during request teardown.
                    monitor.control.freeze();
                    drop(monitor.thread.take());
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Freeze before detaching. A successful infrastructure publication
                    // also stops the token; late monitor results cannot cancel it again.
                    let (_, published) = monitor.control.freeze_as_infrastructure_failure();
                    if published {
                        self.cancellation.cancel();
                    }
                }
            }
        }
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        let shutdown_wait = self.monitor.as_ref().map_or(
            Duration::from_millis(TURN_MONITOR_SHUTDOWN_WAIT_MS),
            |monitor| monitor.shutdown_wait,
        );
        self.teardown_monitor_with_timeout(shutdown_wait);
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
    }
}

fn sequence_output(
    coordinator: &OutputOrderCoordinator,
    mut message: Value,
    trace_binding: Option<TransportTraceBinding>,
) -> AppServerResult<AppServerOutput> {
    let is_event = message
        .get("params")
        .and_then(Value::as_object)
        .is_some_and(|params| params.contains_key("event"));
    let reservation = coordinator
        .reserve(is_event)
        .map_err(AppServerError::Workspace)?;
    if let Some(cursor) = reservation.event_cursor {
        let patch_result: AppServerResult<()> = (|| {
            let is_gap = message["method"] == "event/gap";
            let params = message
                .get_mut("params")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    AppServerError::Workspace("event params are unavailable".to_string())
                })?;
            if is_gap && let Some(gap) = params.get_mut("gap").and_then(Value::as_object_mut) {
                gap.insert("toCursor".to_string(), cursor.into());
            }
            let metadata = params
                .get_mut("event")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    AppServerError::Workspace("event metadata is unavailable".to_string())
                })?;
            metadata.insert("sequence".to_string(), cursor.into());
            metadata.insert("cursor".to_string(), cursor.into());
            if is_gap && let Some(gap) = metadata.get_mut("gap").and_then(Value::as_object_mut) {
                gap.insert("toCursor".to_string(), cursor.into());
            }
            Ok(())
        })();
        if let Err(error) = patch_result {
            coordinator.complete(reservation.order);
            return Err(error);
        }
    }
    Ok(AppServerOutput {
        reservation,
        message,
        trace_binding,
    })
}

fn event_contract(event: &AppEvent) -> (EventClass, EventDelivery, Option<EventRecoveryQuery>) {
    match event.method.as_str() {
        "item/agentMessage/delta" | "item/commandExecution/outputDelta" => {
            (EventClass::Progress, EventDelivery::BestEffort, None)
        }
        "thread/started" => (
            EventClass::State,
            EventDelivery::Reliable,
            event.params["thread"]["thread_id"]
                .as_str()
                .map(|thread_id| EventRecoveryQuery::ThreadRead {
                    thread_id: thread_id.to_string(),
                }),
        ),
        "turn/started" | "turn/completed" => (
            EventClass::State,
            EventDelivery::Reliable,
            event.params["turn"]["turn_id"].as_str().map(|turn_id| {
                EventRecoveryQuery::TurnStatus {
                    turn_id: turn_id.to_string(),
                }
            }),
        ),
        "approval/requested" => (
            EventClass::State,
            EventDelivery::Reliable,
            Some(EventRecoveryQuery::ApprovalList(
                singularity_protocol::EmptyParams {},
            )),
        ),
        "turn/plan/updated" => (
            EventClass::State,
            EventDelivery::Reliable,
            event.params["turnId"]
                .as_str()
                .map(|turn_id| EventRecoveryQuery::TurnStatus {
                    turn_id: turn_id.to_string(),
                }),
        ),
        _ => (EventClass::State, EventDelivery::Reliable, None),
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
        outcome: AtomicU8::new(0),
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
                        if thread_control
                            .record_outcome(CancellationMonitorOutcome::UserCancellation)
                        {
                            cancellation.cancel();
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(error) if error.is_transient_contention() => {}
                    Err(_) => {
                        if thread_control
                            .record_outcome(CancellationMonitorOutcome::InfrastructureFailure)
                        {
                            cancellation.cancel();
                        }
                        break;
                    }
                }
                match wake_receiver.recv_timeout(Duration::from_millis(TURN_CANCELLATION_POLL_MS)) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            if !thread_control.stop.load(Ordering::SeqCst) && cancellation.is_cancelled() {
                let _ = thread_control.record_outcome(CancellationMonitorOutcome::UserCancellation);
            }
            let _ = done_sender.send(());
        })
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
    Ok(Some(CancellationMonitor {
        control,
        done,
        thread: Some(thread),
        shutdown_wait: Duration::from_millis(TURN_MONITOR_SHUTDOWN_WAIT_MS),
    }))
}

fn approval_terminal_status(
    thread: &Thread,
    decision: &ApprovalDecision,
    pending_approval: Option<&PendingApprovalOccurrence>,
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
    if let Some(command_audit) = pending_command_audit_metadata(pending_approval, thread) {
        merge_json_object(&mut audit_event, command_audit);
    }
    run_status
        .audit_events
        .push(project_audit_event(&audit_event));
    run_status
}

/// 在 approval 请求进入执行状态前，唯一解码 opaque checkpoint 的 typed 边界。
fn decode_pending_approval(
    request: &ApprovalRequest,
    payload: Option<&Value>,
) -> AppServerResult<Option<PendingApprovalOccurrence>> {
    match (request.tool_call_id.is_some(), payload) {
        (false, None) => Ok(None),
        (true, Some(payload)) => {
            PendingApprovalOccurrence::from_checkpoint_payload(request.clone(), payload)
                .map(Some)
                .map_err(|error| {
                    AppServerError::Store(StoreError::InvalidState(format!(
                        "{APPROVAL_CHECKPOINT_REQUIRED}: {error}"
                    )))
                })
        }
        (true, None) | (false, Some(_)) => Err(AppServerError::Store(StoreError::InvalidState(
            APPROVAL_CHECKPOINT_REQUIRED.to_string(),
        ))),
    }
}

fn pending_command_audit_metadata(
    pending_approval: Option<&PendingApprovalOccurrence>,
    thread: &Thread,
) -> Option<Value> {
    let pending = pending_approval?.pending_tool_call();
    if pending.tool_name.as_str() != TOOL_COMMAND {
        return None;
    }
    let scope_digest = pending
        .resources
        .iter()
        .find_map(|resource| match resource {
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

/// 在 AppServer 与 Store 的持久化边界一次性编码 typed checkpoint。
fn encode_pending_approvals(
    checkpoints: &[PendingApprovalOccurrence],
) -> Result<Vec<(ApprovalRequest, Value)>, StoreError> {
    checkpoints
        .iter()
        .map(|occurrence| {
            let payload = occurrence
                .encode_checkpoint()
                .map_err(StoreError::InvalidState)?;
            Ok((occurrence.request().clone(), payload))
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
    let workspace_path = workspace_path(thread).map_err(|_| SAFE_WORKSPACE_FAILURE.to_string())?;
    WorkspaceTools::new(workspace_path)
        .map(|tools| tools.with_shared_sandbox_backend(sandbox_backend))
        .map_err(|_| SAFE_WORKSPACE_FAILURE.to_string())
}

#[cfg(test)]
fn workspace_tools(
    workspace_root: PathBuf,
    sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
) -> AppServerResult<WorkspaceTools> {
    WorkspaceTools::new(workspace_root)
        .map(|tools| tools.with_shared_sandbox_backend(sandbox_backend))
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))
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
        .map_err(|_| AppServerError::InvalidParams("Invalid params".to_string()))
}

fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    matches!(
        status,
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
    )
}

fn is_safe_turn_state(turn: &Turn) -> bool {
    (turn.status == TurnStatus::Blocked && turn.agent_loop_status == AgentStatus::Blocked.as_str())
        || is_terminal_turn_status(&turn.status)
        || (turn.status == TurnStatus::Interrupted
            && turn.agent_loop_status == AgentStatus::Cancelled.as_str())
}

fn turn_failure_cause(error: &AppServerError) -> TurnFailureCause {
    match error {
        AppServerError::Store(_) => TurnFailureCause::Store,
        AppServerError::ProjectInstructions(_) => TurnFailureCause::ProjectInstructions,
        AppServerError::Workspace(_) => TurnFailureCause::Workspace,
        AppServerError::InvalidJson(_) => TurnFailureCause::Serialization,
        AppServerError::InvalidParams(_) => TurnFailureCause::Internal,
        AppServerError::TurnExecution { cause, .. }
        | AppServerError::TurnTerminalization { cause, .. } => *cause,
    }
}

fn turn_failure_from_error(
    error: &AppServerError,
    fallback_stage: TurnFailureStage,
) -> TurnFailure {
    match error {
        AppServerError::TurnExecution { stage, cause }
        | AppServerError::TurnTerminalization { stage, cause, .. } => TurnFailure {
            stage: *stage,
            cause: *cause,
        },
        _ => TurnFailure {
            stage: fallback_stage,
            cause: turn_failure_cause(error),
        },
    }
}

fn monitor_failure_or(
    monitor_outcome: Option<CancellationMonitorOutcome>,
    fallback: TurnFailure,
) -> TurnFailure {
    if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
        TurnFailure {
            stage: TurnFailureStage::CancellationMonitor,
            cause: TurnFailureCause::CancellationMonitor,
        }
    } else {
        fallback
    }
}

fn failed_turn_status(failure: TurnFailure) -> AgentRunStatus {
    let mut status =
        AgentRunStatus::failed(format!("turn execution failed during {}", failure.stage));
    status.audit_events.push(json!({
        "component": "app_server",
        "failure_kind": "turn_execution",
        "failure_stage": failure.stage.as_str(),
        "failure_cause": failure.cause.as_str(),
    }));
    status
}

/// 将 AgentLoop 内部失败投影为固定摘要，并保留稳定的阶段/原因审计字段。
fn safe_failed_agent_status(message: &'static str, cause: &'static str) -> AgentRunStatus {
    let mut status = AgentRunStatus::failed(message).with_status(AgentStatus::Failed);
    status.audit_events.push(json!({
        "component": "app_server",
        "failure_kind": "agent_loop",
        "failure_stage": TurnFailureStage::AgentLoop.as_str(),
        "failure_cause": cause,
    }));
    status
}

/// 不允许 AgentLoop、provider 或 workspace 的原始错误进入状态和持久化边界。
fn sanitize_agent_run_status_error(status: &mut AgentRunStatus) {
    if status.error.is_some() {
        status.error = Some(SAFE_AGENT_LOOP_FAILURE.to_string());
    }
    if let Some(provider_diagnostic) = status.provider_diagnostic.as_mut() {
        provider_diagnostic.validation_errors.clear();
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
        "error": status
            .error
            .as_deref()
            .map(|_| SAFE_AGENT_LOOP_FAILURE),
        "provider_diagnostic": status.provider_diagnostic.as_ref().map(|diagnostic| json!({
            "code": &diagnostic.code,
            "stage": &diagnostic.stage,
            "transport_category": &diagnostic.transport_category,
            "timeout_seconds": diagnostic.timeout_seconds,
            "http_status": diagnostic.http_status,
        })),
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    use singularity_agent::{AgentRecoveryMetrics, PendingToolCall};
    use singularity_model::{
        ModelError, ModelErrorCategory, ModelErrorKind, ModelRole, ModelToolCall,
        ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
        Provider, ProviderAttemptMetadata, ProviderError, ProviderProtocolContract,
        ProviderStreamEvent,
    };
    use singularity_policy::{ToolId, WorkspaceRelativePath};
    use singularity_protocol::ItemKind;
    use singularity_sandbox::{CommandScriptRequest, WorkspaceMutation};
    use singularity_tools::{CommandRequest, CommandResult};

    use super::*;

    fn tool_id(value: &str) -> ToolId {
        ToolId::new(value).expect("valid tool id")
    }

    fn workspace_resource(value: &str) -> PermissionResource {
        PermissionResource::WorkspacePath(
            WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
        )
    }

    fn app_server(store: SessionStore) -> AppServer {
        AppServer::new(store, ProviderConfigSnapshot::capture(|_| None))
    }

    fn pending_approval_for_test(
        request: &ApprovalRequest,
        arguments: Value,
    ) -> PendingApprovalOccurrence {
        let tool_call_id = request.tool_call_id.clone().expect("tool call id");
        let raw_arguments = arguments.to_string();
        let payload = json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": &tool_call_id,
            "tool_name": &request.action,
            "raw_arguments": &raw_arguments,
            "resources": &request.resources,
            "checkpoint_version": 2,
            "project_instructions_digest": null,
            "messages": [{
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "tool_call_id": &tool_call_id,
                    "tool_name": &request.action,
                    "arguments": arguments,
                    "raw_arguments": &raw_arguments,
                    "parse_status": "valid",
                    "validation_errors": []
                }]
            }],
            "tool_result_occurrences": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {
                "workspace_mutated": false,
                "workspace_revision": null,
                "successful_command_count": 0,
                "required_command_counts": {},
                "terminal_command_scope_digests": [],
                "terminal_command_revisions": [],
                "unresolved_failures": []
            },
            "last_completion_error": null,
            "plan": null,
            "plan_update_count": 0,
            "recovery_metrics": AgentRecoveryMetrics::default(),
            "model_usage": ModelUsage::default(),
            "provider_attempts": ProviderAttemptMetadata::default(),
            "context_trace": null,
            "seen_tool_call_fingerprints": [],
            "last_repair_failure": null
        });
        decode_pending_approval(request, Some(&payload))
            .expect("pending approval")
            .expect("pending occurrence")
    }

    #[test]
    fn app_server_checkpoint_codec_rejects_legacy_resources() {
        let request = ApprovalRequest::new(
            "approval_legacy_resource",
            "thread_legacy_resource",
            "turn_legacy_resource",
            tool_id(TOOL_EDIT),
        )
        .with_tool_call_id("call_1")
        .with_resources([workspace_resource("README.md")]);
        let pending = pending_approval_for_test(&request, json!({}));
        let mut legacy = pending.encode_checkpoint().expect("checkpoint");
        legacy["checkpoint_version"] = json!(1);
        legacy["resources"] = json!(["README.md"]);
        legacy["tool_results"] = legacy["tool_result_occurrences"].clone();
        legacy
            .as_object_mut()
            .expect("checkpoint object")
            .remove("tool_result_occurrences");
        legacy["tool_result_context_bindings"] = json!([]);

        assert_eq!(
            decode_pending_approval(&request, Some(&legacy))
                .expect_err("legacy checkpoint must fail closed")
                .to_string(),
            "store error: invalid store state: approval request requires an internal AgentLoop checkpoint: unsupported approval checkpoint version"
        );
    }

    #[test]
    fn initialized_request_is_rejected_as_an_invalid_envelope() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = app_server(store);
        server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");

        let response = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","id":2,"params":{}}"#)
            .expect("initialized request");

        assert_eq!(response.len(), 1);
        assert_eq!(response[0]["jsonrpc"], "2.0");
        assert_eq!(response[0]["id"], 2);
        assert_eq!(response[0]["error"]["code"], -32600);
        assert_eq!(response[0]["error"]["message"], "Invalid Request");

        let still_uninitialized = server
            .handle_json(r#"{"jsonrpc":"2.0","method":"server/capabilities","id":3,"params":{}}"#)
            .expect("server remains unacknowledged");
        assert_eq!(
            still_uninitialized[0]["error"]["message"],
            "Not initialized"
        );
    }

    #[test]
    fn event_subscription_binds_gap_cursor_to_one_output_reservation() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = app_server(store);
        server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
            .expect("initialized");

        let message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":2,"params":{"eventTypes":["thread/started"]}}"#,
        )
        .expect("subscription request");
        let outputs = server
            .handle_with_output(message)
            .expect("subscription outputs");

        assert_eq!(outputs.len(), 2);
        assert_eq!(
            outputs[1].reservation.order,
            outputs[0].reservation.order + 1
        );
        assert_eq!(outputs[0].reservation.event_cursor, Some(1));
        assert_eq!(outputs[1].reservation.event_cursor, None);
        assert_eq!(outputs[0].message["params"]["event"]["cursor"], 1);
        assert_eq!(outputs[1].message["result"]["cursor"], 1);
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
    fn cancellation_monitor_classifies_non_contention_store_failure_as_infrastructure() {
        let token = CancellationToken::new();
        let monitor = cancellation_monitor(
            Some(SessionStore::open(":memory:").expect("store")),
            "missing-turn",
            token.clone(),
        )
        .expect("monitor setup")
        .expect("monitor");
        monitor.control.started.store(true, Ordering::SeqCst);
        monitor.control.wake.send(()).expect("start monitor");
        monitor
            .done
            .recv_timeout(Duration::from_secs(1))
            .expect("monitor completion");
        assert_eq!(
            CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancellation_monitor_classifies_persisted_user_cancellation_separately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        let monitor_store = store.trusted_reopen().expect("monitor store");
        let token = CancellationToken::new();
        let monitor = cancellation_monitor(Some(monitor_store), &turn.turn_id, token.clone())
            .expect("monitor setup")
            .expect("monitor");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Running,
                AgentStatus::CancelRequested.as_str(),
            )
            .expect("request cancellation");
        monitor.control.started.store(true, Ordering::SeqCst);
        monitor.control.wake.send(()).expect("start monitor");
        monitor
            .done
            .recv_timeout(Duration::from_secs(1))
            .expect("monitor completion");
        assert_eq!(
            CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::UserCancellation)
        );
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancellation_monitor_classifies_external_cancellation_as_user_cancellation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        let monitor_store = store.trusted_reopen().expect("monitor store");
        let token = CancellationToken::new();
        let monitor = cancellation_monitor(Some(monitor_store), &turn.turn_id, token.clone())
            .expect("monitor setup")
            .expect("monitor");
        token.cancel();
        monitor.control.started.store(true, Ordering::SeqCst);
        monitor.control.wake.send(()).expect("start monitor");
        monitor
            .done
            .recv_timeout(Duration::from_secs(1))
            .expect("monitor completion");
        assert_eq!(
            CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::UserCancellation)
        );
    }

    fn in_flight_monitor_for_teardown_test(
        cancellation: &CancellationToken,
        shutdown_wait: Duration,
    ) -> (
        CancellationMonitor,
        Arc<CancellationMonitorControl>,
        Sender<()>,
        Receiver<bool>,
        Receiver<()>,
    ) {
        let (wake, _wake_receiver) = mpsc::channel();
        let (done_sender, done) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (published_sender, published_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let control = Arc::new(CancellationMonitorControl {
            started: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            outcome: AtomicU8::new(0),
            wake,
        });
        let thread_control = Arc::clone(&control);
        let thread_cancellation = cancellation.clone();
        let thread = std::thread::spawn(move || {
            release_receiver.recv().expect("release in-flight monitor");
            let published =
                thread_control.record_outcome(CancellationMonitorOutcome::UserCancellation);
            if published {
                thread_cancellation.cancel();
            }
            published_sender.send(published).expect("published result");
            let _ = done_sender.send(());
            finished_sender.send(()).expect("monitor finished");
        });
        (
            CancellationMonitor {
                control: Arc::clone(&control),
                done,
                thread: Some(thread),
                shutdown_wait,
            },
            control,
            release_sender,
            published_receiver,
            finished_receiver,
        )
    }

    #[test]
    fn in_flight_monitor_timeout_freezes_before_late_cancellation() {
        let cancellation = CancellationToken::new();
        let (monitor, control, release, published, finished) =
            in_flight_monitor_for_teardown_test(&cancellation, Duration::ZERO);

        assert_eq!(
            monitor.stabilize(&cancellation),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
        assert!(cancellation.is_cancelled());
        release.send(()).expect("release monitor");
        assert!(!published.recv().expect("late publication result"));
        finished.recv().expect("monitor finished");
        assert_eq!(
            CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
    }

    #[test]
    fn drop_timeout_freezes_before_detached_monitor_can_cancel() {
        let cancellation = CancellationToken::new();
        let (monitor, control, release, published, finished) =
            in_flight_monitor_for_teardown_test(&cancellation, Duration::ZERO);
        {
            let _guard = ActiveTurnGuard {
                turn_id: "drop-timeout-turn".to_string(),
                active_turns: Arc::new(Mutex::new(HashMap::new())),
                cancellation: cancellation.clone(),
                monitor: Some(monitor),
                stabilized_monitor_outcome: None,
            };
        }

        assert!(cancellation.is_cancelled());
        assert_eq!(
            CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
        release.send(()).expect("release detached monitor");
        assert!(!published.recv().expect("late publication result"));
        finished.recv().expect("monitor finished");
        assert_eq!(
            CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
    }

    #[test]
    fn frozen_monitor_outcome_wins_over_late_cancellation_and_preserves_safe_states() {
        let (wake, _wake_receiver) = mpsc::channel();
        let (done_sender, done) = mpsc::channel();
        let (recorded_sender, recorded) = mpsc::channel();
        let control = Arc::new(CancellationMonitorControl {
            started: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            outcome: AtomicU8::new(0),
            wake,
        });
        let thread_control = Arc::clone(&control);
        std::thread::spawn(move || {
            thread_control.record_outcome(CancellationMonitorOutcome::InfrastructureFailure);
            recorded_sender.send(()).expect("recorded outcome");
            done_sender.send(()).expect("monitor done");
        });
        recorded.recv().expect("monitor outcome recorded");
        let token = CancellationToken::new();

        let mut guard = ActiveTurnGuard {
            turn_id: "synthetic-turn".to_string(),
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            cancellation: token.clone(),
            monitor: Some(CancellationMonitor {
                control: Arc::clone(&control),
                done,
                thread: None,
                shutdown_wait: Duration::from_millis(TURN_MONITOR_SHUTDOWN_WAIT_MS),
            }),
            stabilized_monitor_outcome: None,
        };
        token.cancel();
        assert_eq!(
            guard.stabilize_monitor(&token),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );
        control.record_outcome(CancellationMonitorOutcome::UserCancellation);
        assert_eq!(
            CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
            Some(CancellationMonitorOutcome::InfrastructureFailure)
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("running turn");
        let server = app_server(store);
        let failure_status = AgentRunStatus::failed("late monitor failure");
        assert!(matches!(
            server.commit_turn_run_status(
                turn.clone(),
                &failure_status,
                None,
                &token,
                Some(CancellationMonitorOutcome::InfrastructureFailure),
            ),
            Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            })
        ));
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);
        let failure = TurnFailure {
            stage: TurnFailureStage::CancellationMonitor,
            cause: TurnFailureCause::CancellationMonitor,
        };
        assert!(matches!(
            server.finish_turn_failure(
                &mut emit,
                &turn,
                None,
                &token,
                Some(CancellationMonitorOutcome::InfrastructureFailure),
                failure,
            ),
            Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            })
        ));
        let failed = server.store.get_turn(&turn.turn_id).expect("failed turn");
        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.agent_loop_status, AgentStatus::Failed.as_str());

        let cancelled_turn = server
            .store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("cancelled turn");
        let cancelled_token = CancellationToken::new();
        cancelled_token.cancel();
        server
            .commit_turn_run_status(
                cancelled_turn.clone(),
                &AgentRunStatus::failed("late user result"),
                None,
                &cancelled_token,
                Some(CancellationMonitorOutcome::UserCancellation),
            )
            .expect("user cancellation commit");
        let interrupted = server
            .store
            .get_turn(&cancelled_turn.turn_id)
            .expect("interrupted turn");
        assert_eq!(interrupted.status, TurnStatus::Interrupted);
        assert_eq!(
            interrupted.agent_loop_status,
            AgentStatus::Cancelled.as_str()
        );

        let blocked_thread = server
            .store
            .create_thread(None, None)
            .expect("blocked thread");
        let blocked_turn = server
            .store
            .create_turn(&blocked_thread.thread_id, AgentStatus::Running.as_str())
            .expect("blocked turn");
        server
            .store
            .update_turn_state(
                &blocked_turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked state");
        let blocked_result = server
            .terminalize_turn_failure(
                &blocked_turn,
                &token,
                Some(CancellationMonitorOutcome::InfrastructureFailure),
                failure,
            )
            .expect("preserve blocked turn");
        assert!(matches!(
            blocked_result,
            TurnTerminalizationResult::Preserved
        ));
        assert_eq!(
            server
                .store
                .get_turn(&blocked_turn.turn_id)
                .expect("blocked")
                .status,
            TurnStatus::Blocked
        );

        let completed_thread = server
            .store
            .create_thread(None, None)
            .expect("completed thread");
        let completed_turn = server
            .store
            .create_turn(&completed_thread.thread_id, AgentStatus::Running.as_str())
            .expect("completed turn");
        server
            .store
            .update_turn_state(
                &completed_turn.turn_id,
                TurnStatus::Completed,
                AgentStatus::Completed.as_str(),
            )
            .expect("completed state");
        let completed_result = server
            .terminalize_turn_failure(
                &completed_turn,
                &token,
                Some(CancellationMonitorOutcome::InfrastructureFailure),
                failure,
            )
            .expect("preserve completed turn");
        assert!(matches!(
            completed_result,
            TurnTerminalizationResult::Preserved
        ));
        assert_eq!(
            server
                .store
                .get_turn(&completed_turn.turn_id)
                .expect("completed")
                .status,
            TurnStatus::Completed
        );
    }

    #[test]
    fn turn_started_event_failure_terminalizes_the_running_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
        let filter = Arc::clone(&server.event_filter);
        let poisoned = std::thread::spawn(move || {
            let _guard = filter.lock().expect("event filter");
            panic!("poison event filter");
        })
        .join();
        assert!(poisoned.is_err());

        let message = JsonRpcMessage::request(
            Method::TurnStart,
            1,
            json!({
                "threadId": &thread.thread_id,
                "input": [{"type": "text", "text": "event failure"}]
            }),
        )
        .expect("turn start request");
        let error = server
            .handle_turn_start_streaming(message, |_| {})
            .expect_err("event failure must be surfaced");
        assert!(matches!(
            error,
            AppServerError::TurnTerminalization {
                stage: TurnFailureStage::EventNotification,
                cause: TurnFailureCause::Workspace,
                failure: TurnTerminalizationFailure::EventNotification,
            }
        ));
        let persisted = server
            .store
            .list_threads()
            .expect("threads")
            .first()
            .expect("thread")
            .thread_id
            .clone();
        let turns = server
            .store
            .read_thread_history(&persisted, None, 8)
            .expect("history");
        assert!(turns.messages.is_empty());
        let failed = server
            .store
            .list_trace(&persisted)
            .expect("trace")
            .into_iter()
            .find(|trace| trace.payload["status"] == AgentStatus::Failed.as_str())
            .expect("failed terminal trace");
        assert_eq!(failed.payload["status"], AgentStatus::Failed.as_str());
    }

    #[test]
    fn late_turn_failure_does_not_overwrite_a_blocked_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
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
        let server = app_server(store);

        assert!(
            server
                .commit_turn_run_status(
                    turn.clone(),
                    &AgentRunStatus::failed("stale run failure"),
                    None,
                    &CancellationToken::new(),
                    None,
                )
                .is_err()
        );
        let persisted = server
            .store
            .get_turn(&turn.turn_id)
            .expect("persisted turn");
        assert_eq!(persisted.status, TurnStatus::Blocked);
        assert_eq!(persisted.agent_loop_status, AgentStatus::Blocked.as_str());
    }

    #[test]
    fn running_turn_failure_stages_terminalize_as_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let stages = [
            TurnFailureStage::AgentLoop,
            TurnFailureStage::ApprovalCheckpoint,
            TurnFailureStage::TerminalOutcome,
        ];
        let mut turns = Vec::new();
        for stage in stages {
            let thread = store.create_thread(None, None).expect("thread");
            let turn = store
                .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
                .expect("turn");
            turns.push((turn, stage));
        }
        let server = app_server(store);

        for (turn, stage) in turns {
            if stage == TurnFailureStage::TerminalOutcome {
                let invalid_commit = AgentRunStatus::failed("invalid completion")
                    .with_status(AgentStatus::Completed);
                assert!(
                    server
                        .commit_turn_run_status(
                            turn.clone(),
                            &invalid_commit,
                            None,
                            &CancellationToken::new(),
                            None,
                        )
                        .is_err()
                );
            }
            let mut emitted = Vec::new();
            let mut emit = |message| emitted.push(message);
            let result = server.finish_turn_failure(
                &mut emit,
                &turn,
                None,
                &CancellationToken::new(),
                None,
                stage,
            );
            assert!(matches!(
                result,
                Err(AppServerError::TurnExecution { stage: actual, .. }) if actual == stage
            ));
            let persisted = server.store.get_turn(&turn.turn_id).expect("failed turn");
            assert_eq!(persisted.status, TurnStatus::Failed);
            assert_eq!(persisted.agent_loop_status, AgentStatus::Failed.as_str());
            let trace = server
                .store
                .list_trace(&persisted.thread_id)
                .expect("trace")
                .into_iter()
                .find(|trace| {
                    trace.payload["audit_events"]
                        .to_string()
                        .contains(stage.as_str())
                })
                .expect("typed failure trace");
            assert!(
                trace.payload["audit_events"]
                    .to_string()
                    .contains("turn_execution")
            );
        }
    }

    #[test]
    fn terminalization_preserves_interrupted_and_blocked_turns() {
        let cases = [
            (TurnStatus::Interrupted, AgentStatus::Cancelled),
            (TurnStatus::Blocked, AgentStatus::Blocked),
        ];
        for (expected_status, expected_agent_status) in cases {
            let dir = tempfile::tempdir().expect("temp dir");
            let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
            let thread = store.create_thread(None, None).expect("thread");
            let turn = store
                .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
                .expect("turn");
            store
                .update_turn_state(
                    &turn.turn_id,
                    expected_status.clone(),
                    expected_agent_status.as_str(),
                )
                .expect("safe state");
            let server = app_server(store);
            let mut emitted = Vec::new();
            let mut emit = |message| emitted.push(message);

            let result = server.finish_turn_failure(
                &mut emit,
                &turn,
                None,
                &CancellationToken::new(),
                None,
                TurnFailureStage::AgentLoop,
            );

            assert!(matches!(
                result,
                Err(AppServerError::TurnExecution {
                    stage: TurnFailureStage::AgentLoop,
                    ..
                })
            ));
            let persisted = server.store.get_turn(&turn.turn_id).expect("safe turn");
            assert_eq!(persisted.status, expected_status);
            assert_eq!(persisted.agent_loop_status, expected_agent_status.as_str());
        }
    }

    #[test]
    fn approval_failure_terminalizes_failed_without_raw_cause() {
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
                json!([{"type": "text", "text": "approval"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let request = ApprovalRequest::new(
            "approval_terminalize_failure",
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
                Some(checkpoint.clone()),
                "approval",
                "approval requested",
            )
            .expect("pending approval");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        store
            .record_approval_decision(&decision, "approval", "approval decision recorded")
            .expect("claim approval execution");
        let server = app_server(store);

        let terminal = server
            .terminalize_claimed_approval_error(
                &request,
                &decision,
                None,
                ApprovalTerminalizationContext {
                    turn: &turn,
                    thread: &thread,
                    prior_status: None,
                    cancellation: &CancellationToken::new(),
                    monitor_outcome: None,
                    failure: TurnFailureStage::ApprovalCheckpoint.into(),
                },
            )
            .expect("terminalize approval failure");
        let committed = match terminal {
            TurnTerminalizationResult::Committed(committed) => committed,
            TurnTerminalizationResult::Preserved => panic!("approval must be terminalized"),
        };
        assert_eq!(committed.turn.status, TurnStatus::Failed);
        assert_eq!(
            committed.turn.agent_loop_status,
            AgentStatus::Failed.as_str()
        );
        assert!(
            !server
                .store
                .has_pending_tool_call(&request.request_id)
                .expect("resolved approval")
        );
        let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
        assert!(!trace_json.contains("sqlite"));
        assert!(!trace_json.contains("raw_arguments"));
    }

    #[test]
    fn terminalization_failure_keeps_stage_and_redacts_cleanup_cause() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        let mut missing_turn = turn;
        missing_turn.turn_id = "missing-turn-with-secret-path".to_string();
        let server = app_server(store);
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);

        let error = server
            .finish_turn_failure(
                &mut emit,
                &missing_turn,
                None,
                &CancellationToken::new(),
                None,
                TurnFailure {
                    stage: TurnFailureStage::TerminalOutcome,
                    cause: TurnFailureCause::Store,
                },
            )
            .expect_err("terminalization must report its cleanup failure");
        assert!(matches!(
            error,
            AppServerError::TurnTerminalization {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                failure: TurnTerminalizationFailure::Store,
            }
        ));
        assert!(!error.to_string().contains("missing-turn-with-secret-path"));
    }

    #[test]
    fn workspace_tool_binding_failure_is_a_typed_app_server_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace_sentinel = "workspace-path-sentinel";
        let missing = dir.path().join(workspace_sentinel);

        assert!(matches!(
            workspace_tools(missing, Arc::new(CompletedSandboxBackend)),
            Err(AppServerError::Workspace(message))
                if message == SAFE_WORKSPACE_FAILURE && !message.contains(workspace_sentinel)
        ));
    }

    #[test]
    fn workspace_binding_failure_precedes_running_turn_persistence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace-path-sentinel");
        std::fs::create_dir(&workspace).expect("create workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(None, Some(&workspace.to_string_lossy()))
            .expect("thread");
        std::fs::remove_dir(&workspace).expect("remove workspace before turn");
        let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

        let response = server
            .turn_start(
                JsonRpcMessage::request(
                    Method::TurnStart,
                    1,
                    json!({
                        "threadId": thread.thread_id,
                        "input": [{"type": "text", "text": "must not persist"}],
                    }),
                )
                .expect("request"),
            )
            .expect("turn response");

        assert!(
            response[0]["error"]["message"]
                .as_str()
                .expect("error message")
                == SAFE_WORKSPACE_FAILURE
        );
        assert!(!response[0].to_string().contains("workspace-path-sentinel"));
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
        assert_eq!(error, SAFE_WORKSPACE_FAILURE);
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

    struct StreamingProvider {
        responses: Vec<(Vec<ProviderStreamEvent>, ModelTurnResponse)>,
        seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    }

    impl Provider for StreamingProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
            on_event: &mut dyn FnMut(ProviderStreamEvent),
        ) -> Result<ModelTurnResponse, ProviderError> {
            let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
            let response_index = seen_requests.len();
            seen_requests.push(request.clone());
            let (events, response) = self
                .responses
                .get(response_index)
                .unwrap_or_else(|| self.responses.last().expect("streaming response"));
            for event in events {
                on_event(event.clone());
            }
            let mut response = response.clone();
            response.request_id = request.request_id.clone();
            Ok(response)
        }

        fn complete(
            &self,
            _request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> Result<ModelTurnResponse, ProviderError> {
            panic!("streaming provider must use complete_stream")
        }
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
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "user goal".to_string(),
            }],
        };
        let provider_sentinel = "provider-body-sentinel";
        let mut provider_error =
            ModelError::new(ModelErrorKind::AuthError, provider_sentinel.to_string());
        provider_error.validation_errors = vec![provider_sentinel.to_string()];
        let provider = StaticProvider {
            responses: vec![failed_model_response(provider_error)],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        };

        let server = app_server(store);
        let status = server
            .run_agent_loop_with_provider(
                provider,
                &thread,
                &params,
                &turn.turn_id,
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");

        assert_eq!(status.status, AgentStatus::Failed);
        assert_eq!(
            status.error_category,
            Some(ModelErrorCategory::Authentication)
        );
        let status_json = serde_json::to_string(&status).expect("serialize status");
        assert_eq!(status.error.as_deref(), Some(SAFE_AGENT_LOOP_FAILURE));
        assert!(!status_json.contains(provider_sentinel));
        assert!(!status_json.contains("validation_errors"));
        let committed = server
            .commit_turn_run_status(turn, &status, None, &CancellationToken::new(), None)
            .expect("commit provider failure");
        let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
        assert!(!trace_json.contains(provider_sentinel));
        assert_eq!(committed.trace.payload["error"], SAFE_AGENT_LOOP_FAILURE);
        assert!(
            committed.trace.payload["provider_diagnostic"]
                .get("validation_errors")
                .is_none()
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
        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(vec![
            "item/started".to_string(),
            "item/completed".to_string(),
            "turn/plan/updated".to_string(),
            "item/agentMessage/delta".to_string(),
            "turn/completed".to_string(),
        ]);

        let assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let committed = server
            .commit_turn_run_status(
                turn,
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
                None,
            )
            .expect("commit terminal outcome");
        let plan_item = committed.plan_item.as_ref().expect("plan item");
        assert_eq!(plan_item.kind, ItemKind::Plan);
        let events = server
            .committed_turn_events(&committed, Some(&assistant_events))
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

    #[test]
    fn responses_finalization_deltas_share_item_id_with_terminal_store_item() {
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
                json!([{"type": "text", "text": "say hello"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "say hello".to_string(),
            }],
        };
        let server = app_server(store).with_sandbox_backend(MutatingCommandSandboxBackend {
            calls: AtomicUsize::new(0),
        });
        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(vec![
            "item/started".to_string(),
            "item/agentMessage/delta".to_string(),
            "item/completed".to_string(),
            "turn/completed".to_string(),
        ]);
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let mut events = Vec::new();
        let mut mutation_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "mutating");
        mutation_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_mutate".to_string(),
            tool_name: "command".to_string(),
            arguments: json!({
                "command": "test-program mutate",
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "command": "test-program mutate",
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let mut verification_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "verifying");
        verification_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_verify".to_string(),
            tool_name: "command".to_string(),
            arguments: json!({
                "command": "test-program verify",
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "command": "test-program verify",
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        });
        let status = server
            .run_agent_loop_with_provider_and_text_deltas(
                StreamingProvider {
                    responses: vec![
                        (
                            vec![ProviderStreamEvent::OutputTextDelta {
                                delta: "mutating".to_string(),
                            }],
                            mutation_response,
                        ),
                        (
                            vec![ProviderStreamEvent::OutputTextDelta {
                                delta: "verifying".to_string(),
                            }],
                            verification_response,
                        ),
                        (
                            vec![
                                ProviderStreamEvent::OutputTextDelta {
                                    delta: "do".to_string(),
                                },
                                ProviderStreamEvent::OutputTextDelta {
                                    delta: "ne".to_string(),
                                },
                            ],
                            ModelTurnResponse::completed(
                                "model_request_turn_1_2",
                                "response_3",
                                "done",
                            ),
                        ),
                    ],
                    seen_requests: Arc::new(Mutex::new(Vec::new())),
                },
                &thread,
                &params,
                &turn.turn_id,
                &[],
                &CancellationToken::new(),
                &mut |delta| {
                    events.extend(
                        server
                            .project_assistant_delta(&mut assistant_events, delta)
                            .expect("project delta"),
                    );
                },
            )
            .expect("agent loop");
        assert_eq!(status.status, AgentStatus::Completed);
        assert_eq!(status.final_answer.as_deref(), Some("done"));
        assert!(status.verification.required);
        assert!(status.verification.passed);

        let committed = server
            .commit_turn_run_status(
                turn,
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
                None,
            )
            .expect("commit terminal outcome");
        events.extend(
            server
                .committed_turn_events(&committed, Some(&assistant_events))
                .expect("terminal events"),
        );

        let methods = events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "item/started",
                "item/agentMessage/delta",
                "item/agentMessage/delta",
                "item/completed",
                "turn/completed",
            ]
        );
        assert_eq!(events[1]["params"]["delta"], "do");
        assert_eq!(events[2]["params"]["delta"], "ne");
        let events_json = serde_json::to_string(&events).expect("events json");
        assert!(!events_json.contains("mutating"));
        assert!(!events_json.contains("verifying"));
        let item_id = assistant_events.item_id.as_str();
        assert!(
            events[..4]
                .iter()
                .all(|event| event["params"]["item"]["item_id"] == item_id)
        );
        assert_eq!(
            committed
                .assistant_item
                .as_ref()
                .map(|item| item.item_id.as_str()),
            Some(item_id)
        );
        assert_eq!(
            committed
                .assistant_item
                .as_ref()
                .and_then(|item| item.payload["delta"].as_str()),
            Some("done")
        );
    }

    #[test]
    fn partial_realtime_item_fails_without_persisting_or_completing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let server = app_server(store);
        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(vec![
            "item/started".to_string(),
            "item/agentMessage/delta".to_string(),
            "item/failed".to_string(),
            "item/completed".to_string(),
            "turn/completed".to_string(),
        ]);

        for (label, cancelled) in [
            ("raw provider stream failure sentinel", false),
            ("raw terminal mismatch sentinel", false),
            ("raw cancellation sentinel", true),
        ] {
            let thread = server
                .store
                .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
                .expect("thread");
            let (turn, _, _) = server
                .store
                .create_turn_with_input_and_trace(
                    &thread.thread_id,
                    AgentStatus::Running.as_str(),
                    json!([{"type": "text", "text": "run"}]),
                    "app_server",
                    "turn started",
                )
                .expect("turn");
            let mut assistant_events =
                AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
            let mut events = server
                .project_assistant_delta(&mut assistant_events, "partial")
                .expect("project partial");
            let mut status = AgentRunStatus::failed(label);
            if cancelled {
                status.status = AgentStatus::Cancelled;
            }

            let committed = server
                .commit_turn_run_status(
                    turn,
                    &status,
                    Some(&assistant_events.item_id),
                    &CancellationToken::new(),
                    None,
                )
                .expect("commit safe terminal status");
            events.extend(
                server
                    .committed_turn_events(&committed, Some(&assistant_events))
                    .expect("terminal events"),
            );

            let methods = events
                .iter()
                .map(|event| event["method"].as_str().expect("event method"))
                .collect::<Vec<_>>();
            assert_eq!(
                methods,
                vec![
                    "item/started",
                    "item/agentMessage/delta",
                    "item/failed",
                    "turn/completed",
                ]
            );
            assert_eq!(events[2]["params"]["error"], SAFE_ASSISTANT_ITEM_FAILURE);
            assert!(
                !serde_json::to_string(&events)
                    .expect("events json")
                    .contains(label)
            );
            assert!(committed.assistant_item.is_none());
            assert!(
                server
                    .store
                    .read_thread_history(&thread.thread_id, None, 10)
                    .expect("history")
                    .messages
                    .iter()
                    .all(|message| message.role != ConversationRole::Assistant)
            );
        }
    }

    #[test]
    fn terminal_store_failure_fails_visible_realtime_item_without_completion() {
        let temp = tempfile::tempdir().expect("temp dir");
        let db_path = temp.path().join("sessions.sqlite3");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
        let store = SessionStore::open(&db_path).expect("store");
        let assistant_item_id = SessionStore::allocate_assistant_item_id();
        let collision_thread = store.create_thread(None, None).expect("collision thread");
        let collision_turn = store
            .create_turn(&collision_thread.thread_id, AgentStatus::Running.as_str())
            .expect("collision turn");
        store
            .commit_turn_outcome(
                &collision_turn.turn_id,
                CommitTurnOutcomeParams {
                    status: TurnStatus::Completed,
                    agent_loop_status: AgentStatus::Completed.as_str(),
                    assistant_item_id: Some(&assistant_item_id),
                    assistant_delta: Some("existing assistant"),
                    plan: None,
                    trace: &TraceEvent::for_turn(
                        "trace_existing_assistant",
                        &collision_thread.thread_id,
                        &collision_turn.turn_id,
                        "test",
                        "existing assistant",
                    ),
                },
            )
            .expect("existing assistant");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _, _) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "complete"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let server = app_server(store);
        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(vec![
            "item/started".to_string(),
            "item/agentMessage/delta".to_string(),
            "item/failed".to_string(),
            "item/completed".to_string(),
            "turn/completed".to_string(),
        ]);
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "complete".to_string(),
            }],
        };
        let status = server
            .run_agent_loop_with_provider(
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "complete",
                    )],
                    seen_requests: Arc::new(Mutex::new(Vec::new())),
                },
                &thread,
                &params,
                &turn.turn_id,
                &[],
                &CancellationToken::new(),
            )
            .expect("agent loop");
        let mut assistant_events = AssistantItemEventState::new(assistant_item_id);
        let mut events = server
            .project_assistant_delta(&mut assistant_events, "partial")
            .expect("partial delta");

        assert!(
            server
                .commit_turn_run_status(
                    turn.clone(),
                    &status,
                    Some(&assistant_events.item_id),
                    &CancellationToken::new(),
                    None,
                )
                .is_err()
        );
        let result = server.finish_turn_failure(
            &mut |event| events.push(event),
            &turn,
            Some(&assistant_events),
            &CancellationToken::new(),
            None,
            TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
            },
        );

        assert!(matches!(result, Err(AppServerError::TurnExecution { .. })));
        assert_eq!(
            events
                .iter()
                .map(|event| event["method"].as_str().expect("event method"))
                .collect::<Vec<_>>(),
            vec![
                "item/started",
                "item/agentMessage/delta",
                "item/failed",
                "turn/completed",
            ]
        );
        assert_eq!(
            server.store.get_turn(&turn.turn_id).expect("turn").status,
            TurnStatus::Failed
        );
        assert!(
            server
                .store
                .read_thread_history(&thread.thread_id, None, 10)
                .expect("history")
                .messages
                .iter()
                .all(|message| message.role != ConversationRole::Assistant)
        );
    }

    #[test]
    fn realtime_item_tracks_started_and_delta_filtering_independently() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
        let server = app_server(store);

        for (subscriptions, expected, started_generated, delta_generated) in [
            (
                vec!["item/started".to_string()],
                vec!["item/started"],
                true,
                false,
            ),
            (
                vec!["item/agentMessage/delta".to_string()],
                vec!["item/agentMessage/delta", "item/agentMessage/delta"],
                false,
                true,
            ),
            (Vec::new(), Vec::new(), false, false),
        ] {
            server
                .event_filter
                .lock()
                .expect("event filter")
                .event_types = Some(subscriptions);
            let mut assistant_events =
                AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
            let mut events = server
                .project_assistant_delta(&mut assistant_events, "first")
                .expect("first delta");
            events.extend(
                server
                    .project_assistant_delta(&mut assistant_events, "second")
                    .expect("second delta"),
            );

            assert_eq!(
                events
                    .iter()
                    .map(|event| event["method"].as_str().expect("event method"))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(assistant_events.started_generated, started_generated);
            assert_eq!(assistant_events.delta_generated, delta_generated);
        }
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
        let pending_payload = pending_approval_for_test(&request, arguments.clone())
            .encode_checkpoint()
            .expect("current checkpoint");
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
        let pending_approval = pending_approval_for_test(
            &request,
            json!({
                "command": "test-program success",
                "timeout_seconds": 5
            }),
        );
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let server = app_server(store);

        let (_turn, run_status) = server
            .approval_no_resume_status(&request, &decision, &turn, &thread, Some(&pending_approval))
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
        let pending_approval = pending_approval_for_test(&request, json!({}));
        let server = app_server(store);
        let current_turn = server
            .store
            .get_turn(&turn.turn_id)
            .expect("cancelled turn");
        let (_turn, no_resume_status) = server
            .approval_no_resume_status(
                &request,
                &decision,
                &current_turn,
                &thread,
                Some(&pending_approval),
            )
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
                &[],
                None,
                None,
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
            .commit_turn_run_status(turn.clone(), &stale_blocked, None, &cancellation, None)
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
    fn agent_loop_approval_resume_rejects_untyped_checkpoint_payloads() {
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
        let mismatched_pending = PendingToolCall {
            request_id: "approval_other_call_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_COMMAND),
            raw_arguments: json!({
                "command": "test-program success",
                "timeout_seconds": 5
            })
            .to_string(),
            resources: Vec::new(),
        };
        let invalid_args_pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: tool_id(TOOL_COMMAND),
            raw_arguments: "{not-json".to_string(),
            resources: Vec::new(),
        };
        let server = app_server(store);

        let mismatch_error = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&mismatched_pending).expect("pending payload")),
                StaticProvider {
                    responses: Vec::new(),
                    seen_requests: Arc::new(Mutex::new(Vec::new())),
                },
                &CancellationToken::new(),
                None,
            )
            .expect_err("mismatched checkpoint must fail closed");
        assert!(matches!(
            mismatch_error,
            AppServerError::Store(StoreError::InvalidState(_))
        ));

        let invalid_args_error = server
            .resume_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&invalid_args_pending).expect("pending payload")),
                StaticProvider {
                    responses: Vec::new(),
                    seen_requests: Arc::new(Mutex::new(Vec::new())),
                },
                &CancellationToken::new(),
                None,
            )
            .expect_err("invalid checkpoint arguments must fail closed");
        assert!(matches!(
            invalid_args_error,
            AppServerError::Store(StoreError::InvalidState(_))
        ));
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
        let resumed_provider = StreamingProvider {
            responses: vec![
                (Vec::new(), verification_response),
                (
                    vec![
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "do".to_string(),
                        },
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "ne".to_string(),
                        },
                    ],
                    final_response,
                ),
            ],
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
            .commit_turn_run_status(turn.clone(), &blocked_status, None, &cancellation, None)
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
        assert_eq!(pending_payload["checkpoint_version"], 2);
        assert!(
            pending_payload["project_instructions_digest"]
                .as_str()
                .is_some_and(|digest| digest.starts_with("sha256:"))
        );
        assert!(pending_payload["messages"].is_array());
        assert!(pending_payload["tool_result_occurrences"].is_array());

        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(vec![
            "item/started".to_string(),
            "item/agentMessage/delta".to_string(),
            "item/completed".to_string(),
            "turn/completed".to_string(),
        ]);
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let mut realtime_events = Vec::new();
        let resumed = server
            .resume_agent_loop_after_gate_with_text_deltas(
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
                &mut |delta| {
                    realtime_events.extend(
                        server
                            .project_assistant_delta(&mut assistant_events, delta)
                            .expect("project approval delta"),
                    );
                },
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
                Some(&assistant_events.item_id),
                None,
            )
            .expect("commit resumed outcome");
        realtime_events.extend(
            server
                .committed_turn_events(&committed, Some(&assistant_events))
                .expect("approval terminal events"),
        );
        assert_eq!(committed.turn.status, TurnStatus::Completed);
        assert_eq!(
            realtime_events
                .iter()
                .map(|event| event["method"].as_str().expect("event method"))
                .collect::<Vec<_>>(),
            vec![
                "item/started",
                "item/agentMessage/delta",
                "item/agentMessage/delta",
                "item/completed",
                "turn/completed",
            ]
        );
        assert_eq!(realtime_events[1]["params"]["delta"], "do");
        assert_eq!(realtime_events[2]["params"]["delta"], "ne");
        assert!(realtime_events[..4].iter().all(|event| {
            event["params"]["item"]["item_id"] == assistant_events.item_id.as_str()
        }));
        assert_eq!(
            committed
                .assistant_item
                .as_ref()
                .map(|item| item.item_id.as_str()),
            Some(assistant_events.item_id.as_str())
        );
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
            .commit_turn_run_status(
                turn.clone(),
                &blocked_status,
                None,
                &CancellationToken::new(),
                None,
            )
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

        let project_sentinel = "project-instruction-sentinel";
        std::fs::write(workspace.join("AGENTS.override.md"), project_sentinel)
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
        assert_eq!(resumed.1.error.as_deref(), Some(SAFE_AGENT_LOOP_FAILURE));
        let resumed_json = serde_json::to_string(&resumed.1).expect("resumed status json");
        assert!(!resumed_json.contains(project_sentinel));
        assert!(
            resumed_seen_requests
                .lock()
                .expect("resumed requests")
                .is_empty()
        );
        let committed = server
            .commit_effective_turn_status_resolving_approval(
                &request.request_id,
                &resumed.0,
                &resumed.1,
                &resumed.2,
                Some(&SessionStore::allocate_assistant_item_id()),
                None,
            )
            .expect("commit project instruction failure");
        let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
        assert!(!trace_json.contains(project_sentinel));
        assert_eq!(committed.trace.payload["error"], SAFE_AGENT_LOOP_FAILURE);
        let history_json = serde_json::to_string(
            &server
                .store
                .read_thread_history(&thread.thread_id, None, DEFAULT_THREAD_HISTORY_TURN_LIMIT)
                .expect("history"),
        )
        .expect("history json");
        assert!(!history_json.contains(project_sentinel));
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
    }

    struct CompletedSandboxBackend;

    struct MutatingCommandSandboxBackend {
        calls: AtomicUsize,
    }

    impl SandboxBackend for MutatingCommandSandboxBackend {
        fn name(&self) -> &'static str {
            "mutating_command_test"
        }

        fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
            singularity_tools::SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "command ok")
                .with_workspace_mutation(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    WorkspaceMutation::Changed
                } else {
                    WorkspaceMutation::Unchanged
                })
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "command ok")
                .with_workspace_mutation(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    WorkspaceMutation::Changed
                } else {
                    WorkspaceMutation::Unchanged
                })
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }
    }

    impl SandboxBackend for CompletedSandboxBackend {
        fn name(&self) -> &'static str {
            "completed_test"
        }

        fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
            singularity_tools::SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
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
                .with_workspace_mutation(WorkspaceMutation::Unknown)
        }

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            CommandResult::sandbox_backend_unavailable(&request.command_id)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
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

    #[test]
    fn unsequenced_turn_outputs_carry_one_explicit_transport_trace_binding() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = app_server(store);
        let binding = TransportTraceBinding::for_turn("thread_trace", "turn_trace");
        server.pending_transport_trace_binding = Some(binding.clone());

        let outputs = server
            .sequence_outputs(vec![json!({"jsonrpc": "2.0", "id": 1, "result": {}})])
            .expect("sequence bound output");
        assert_eq!(outputs[0].trace_binding.as_ref(), Some(&binding));
        server.output_order.complete(outputs[0].reservation.order);

        let unbound = server
            .sequence_outputs(vec![json!({"jsonrpc": "2.0", "id": 2, "result": {}})])
            .expect("sequence unbound output");
        assert!(unbound[0].trace_binding.is_none());
        server.output_order.complete(unbound[0].reservation.order);
    }
}
