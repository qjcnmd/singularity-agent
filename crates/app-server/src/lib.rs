#![forbid(unsafe_code)]

//! 在进程边界负责 turn 准入、`AgentLoop` 执行、持久化和取消的 JSON-RPC 应用服务。
//!
//! 服务将协议处理与工作线程执行分离，并通过 `SessionStore` 提交终态后再发出对应事件。

mod approval;
mod dispatch;
mod events;
mod lifecycle;
mod observability;

pub use observability::TraceProjector;

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
    agent::{Agent, AgentConfig, AgentError, AgentEvents, AgentOutcome, SteerHandle},
    session::SessionManager,
    tools::ToolRegistry,
};
use singularity_core::{
    CancellationToken, ErrorCode, ProjectInstructionError, contains_sensitive_text,
    load_project_instructions,
};
use singularity_model::{DEFAULT_MAX_CONTEXT_TOKENS, ModelUsage, Provider, ProviderConfigSnapshot};
use singularity_policy::{
    ApprovalDecision, ApprovalPolicy, ApprovalRequest, PermissionProfileName,
};
use singularity_protocol::{
    AgentCapabilityResult, AgentLoopCapabilityStatus, AppEvent, ApprovalCenterResult,
    ApprovalDecisionResult, ApprovalListResult, ArtifactFetchParams, ArtifactFetchResult,
    EventClass, EventDelivery, EventGap, EventGapReason,
    EventMetadata, EventRecoveryQuery, EventSubscribeParams, EventSubscribeResult,
    InitializeParams, InitializeResult, Item, JsonRpcId, JsonRpcMessage, Method, MethodKind,
    ProviderConfigurationStatus, ServerCapabilitiesResult, ServerShutdownResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadReadParams, ThreadReadResult, ThreadResult, ThreadStartParams, ThreadStartResult,
    TraceEvent, TransportCapability, Turn, TurnIdParams, TurnInputDelivery, TurnInputParams,
    TurnInterruptResult, TurnResult, TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_store::{
    AllocatedAssistantItemId, CommitTurnOutcomeParams, CommittedTurnOutcome,
    CreateStartedTurnParams, SessionStore, StoreError, TurnOutcomeAuthority,
};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const THREAD_ARCHIVED: &str = "Thread is archived; resume it before starting a turn";
const THREAD_ARCHIVED_CONTINUATION: &str =
    "Thread is archived; resume it before continuing the turn";
const WORKSPACE_EXECUTION_ACTIVE: &str = "Workspace already has an active or pending turn";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_REQUEST_INTERNAL_ONLY: &str =
    "approval/request is internal to the AgentLoop approval history";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const DEFAULT_THREAD_HISTORY_TURN_LIMIT: usize = 64;
const MAX_THREAD_HISTORY_TURN_LIMIT: usize = 256;
const TURN_CANCELLATION_POLL_MS: u64 = 25;
const TURN_MONITOR_SHUTDOWN_WAIT_MS: u64 = 100;
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
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
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

/// 一次 agent 运行的稳定生命周期状态（app-server 本地枚举）。
///
/// 字符串值保持 store `agent_loop_status` 列与 CLI 渲染兼容（Phase 3b 本地化，
/// 替代旧链 `singularity_agent::AgentStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Paused,
    CancelRequested,
    Completed,
    Blocked,
    Cancelled,
    Failed,
}

impl AgentStatus {
    /// 返回稳定的生命周期状态字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// agent 运行终态（app-server 内部类型，替代旧链 `AgentRunStatus`）。
///
/// 只保留 app-server 实际消费的字段；`status` 的字符串值写入 store
/// `agent_loop_status` 列，CLI 按文本渲染。
#[derive(Debug, Clone, PartialEq)]
pub struct RunStatus {
    pub status: AgentStatus,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub model_usage: ModelUsage,
    pub audit_events: Vec<Value>,
    pub error: Option<String>,
    pub model_turn_limit: u32,
}

impl RunStatus {
    /// 构造普通失败状态。
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentStatus::Failed,
            final_answer: None,
            model_turns: 0,
            model_usage: ModelUsage::default(),
            audit_events: Vec::new(),
            error: Some(message.into()),
            model_turn_limit: 0,
        }
    }

    /// 更新状态并保留已有字段。
    pub fn with_status(mut self, status: AgentStatus) -> Self {
        self.status = status;
        self
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
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 每个活动 turn 的 steer 注入句柄（turn/input 运行中注入通道）。
    steer_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
    execution_stopped: Arc<AtomicBool>,
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
        Ok(())
    }
}

struct ActiveTurnGuard {
    turn_id: String,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    steer_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
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
        if let Ok(mut steer_handles) = self.steer_handles.lock() {
            steer_handles.remove(&self.turn_id);
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
    // canonicalize 保留旧语义：cwd 必须是存在的真实目录（解析符号链接）。
    let canonical = std::fs::canonicalize(&path)
        .map_err(|_| "failed to bind thread cwd".to_string())?;
    canonical
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

/// 线程级会话目录：workspace 根下的 `.singularity/agent-sessions/`（与旧 `sessions/` 隔离）。
fn agent_sessions_dir(thread: &Thread) -> Result<PathBuf, String> {
    Ok(workspace_path(thread)?.join(".singularity").join("agent-sessions"))
}

/// 打开线程绑定的会话文件（`<thread_id>.jsonl`）；文件不存在时创建新会话。
///
/// thread ↔ 会话文件的确定性映射是跨轮历史的唯一通道（Phase 3a 起）。
fn open_or_create_thread_session(thread: &Thread) -> AppServerResult<SessionManager> {
    let sessions_dir = agent_sessions_dir(thread).map_err(|_| {
        AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string())
    })?;
    let file = sessions_dir.join(format!("{}.jsonl", thread.thread_id));
    if file.exists() {
        SessionManager::open(&file)
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))
    } else {
        SessionManager::create_with_name(
            Path::new(thread.cwd.as_deref().unwrap_or_default()),
            &sessions_dir,
            &thread.thread_id,
        )
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))
    }
}

/// 将持久化的 `InputItem` 数组投影为拼接文本（Agent 输入/转向消息的本地边界）。
fn input_items_to_text(input: &Value) -> AppServerResult<String> {
    let items: Vec<singularity_protocol::InputItem> =
        serde_json::from_value(input.clone()).map_err(AppServerError::InvalidJson)?;
    let text = items
        .into_iter()
        .map(|item| match item {
            singularity_protocol::InputItem::Text { text } => text,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(AppServerError::Workspace(
            "persisted turn input is empty".to_string(),
        ));
    }
    Ok(text)
}

/// 组装新核心 `Agent` 的配置：model 选择器、system prompt（项目指令）、
/// context window（provider 静态声明，缺省时用模型默认值）。
fn agent_config_for_thread(
    thread: &Thread,
    provider: &dyn Provider,
    snapshot: &ProviderConfigSnapshot,
) -> AppServerResult<AgentConfig> {
    let cwd = workspace_path(thread).map_err(|_| {
        AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string())
    })?;
    let system_prompt = match load_project_instructions(&cwd, &cwd) {
        Ok(Some(instructions)) => instructions.content().to_string(),
        Ok(None) => String::new(),
        Err(_) => String::new(),
    };
    let context_window = provider
        .protocol_contract()
        .max_context_tokens
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS) as u64;
    Ok(AgentConfig {
        model: thread
            .model
            .clone()
            .or_else(|| snapshot.resolved_default_selector())
            .unwrap_or_default(),
        system_prompt,
        context_window,
        ..AgentConfig::default()
    })
}

/// 新核心 `AgentOutcome` → store/CLI 依赖的 `RunStatus` 投影。
///
/// aborted 对应取消（Cancelled）；其余按 Completed 提交，final_text 为空时
/// `agent_completed_delta` 的兜底路径会省略终态 delta（事件层 item/failed）。
fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus::failed("agent loop did not reach a final assistant message");
    if outcome.aborted {
        mark_run_cancelled(&mut status);
    } else {
        status.status = AgentStatus::Completed;
        status.error = None;
        status.final_answer = Some(outcome.final_text.clone())
            .filter(|text| !text.trim().is_empty());
    }
    status.model_turns = outcome.turns;
    status.model_usage = outcome.usage;
    status
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

impl AppServer {
    /// Validate an explicit thread/fork selector against the startup snapshot
    /// before any Store row is created.  This never performs provider I/O.
    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        if let Some(selector) = selector {
            // Legacy environment configuration historically accepted a bare model name.
            // Composite selectors and every explicit catalog configuration remain strict.
            if self.provider_snapshot.has_explicit_model_selection()
                || selector.contains('/')
                || selector.contains('#')
            {
                self.provider_snapshot
                    .provider_for_selector(Some(selector))
                    .map(|_| ())
                    .map_err(|_| {
                        AppServerError::InvalidParams("invalid model selector".to_string())
                    })?;
            }
        }
        Ok(())
    }

    /// Resolve the persisted thread selector against the one process snapshot.
    fn provider_for_thread(
        &self,
        thread: &Thread,
    ) -> Result<singularity_model::OpenAiProvider, singularity_model::ProviderError> {
        self.provider_snapshot
            .provider_for_selector(thread.model.as_deref())
    }

    /// 返回解析后的 provider（测试覆盖优先），并组装新核心 Agent 配置。
    fn provider_and_config_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<(Arc<dyn Provider + Send + Sync>, AgentConfig)> {
        let provider: Arc<dyn Provider + Send + Sync> =
            if let Some(test_provider) = &self.test_provider_override {
                Arc::clone(test_provider)
            } else {
                Arc::new(self.provider_for_thread(thread).map_err(|_| {
                    AppServerError::TurnExecution {
                        stage: TurnFailureStage::AgentLoop,
                        cause: TurnFailureCause::Internal,
                    }
                })?)
            };
        let config = agent_config_for_thread(thread, provider.as_ref(), &self.provider_snapshot)?;
        Ok((provider, config))
    }
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
        AppServerError::Agent(_) => TurnFailureCause::Internal,
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

fn failed_turn_status(failure: TurnFailure) -> RunStatus {
    let mut status = RunStatus::failed(format!("turn execution failed during {}", failure.stage));
    status.audit_events.push(json!({
        "component": "app_server",
        "failure_kind": "turn_execution",
        "failure_stage": failure.stage.as_str(),
        "failure_cause": failure.cause.as_str(),
    }));
    status
}

fn turn_status_for_agent(status: &AgentStatus) -> TurnStatus {
    match status {
        AgentStatus::Completed => TurnStatus::Completed,
        AgentStatus::Paused => TurnStatus::Paused,
        AgentStatus::Blocked => TurnStatus::Blocked,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => TurnStatus::Interrupted,
        AgentStatus::Running => TurnStatus::Running,
        AgentStatus::Failed => TurnStatus::Failed,
    }
}

fn mark_run_cancelled(status: &mut RunStatus) {
    status.status = AgentStatus::Cancelled;
    status.final_answer = None;
    status.error = None;
}

fn agent_loop_trace(turn: &Turn, status: &RunStatus) -> TraceEvent {
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
    // 新核心（Phase 3a）不再提供 context/provider attempt/provider protocol 观测；
    // 终态 trace 只投影 AgentOutcome 能提供的字段。
    event.payload = json!({
        "component": "agent_loop",
        "status": status.status.as_str(),
        "model_turns": status.model_turns,
        "model_turn_limit": status.model_turn_limit,
        "model_usage": &status.model_usage,
        "final_text": status.final_answer.as_deref().map(redact_app_server_text),
        "audit_events": &status.audit_events,
        "error": status
            .error
            .as_deref()
            .map(|_| SAFE_AGENT_LOOP_FAILURE),
    });
    event
}

fn agent_completed_delta(run_status: &RunStatus) -> Option<String> {
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
mod tests;
