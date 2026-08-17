#![forbid(unsafe_code)]

//! 在进程边界负责 session/turn 准入、AgentLoop 执行和取消的 stdio JSON-RPC 应用服务。
//!
//! JSONL rollout 是会话正文的唯一权威；SQLite `session_index` 只保存定位与展示元数据。

mod delete;
mod dispatch;
mod events;
mod lifecycle;
pub mod paths;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use singularity_agent::{
    agent::{Agent, AgentConfig, AgentError, AgentEvents, AgentOutcome, SteerHandle},
    session::{
        SessionEntryFilter, SessionError, SessionManager, SessionReadOptions, SessionRepository,
    },
    tools::{ToolExecution, ToolRegistry},
};
use singularity_core::{
    CancellationToken, ErrorCode, ProjectInstructionError, load_project_instructions_from_cwd,
    user_singularity_home,
};
use singularity_model::{DEFAULT_MAX_CONTEXT_TOKENS, ModelUsage, Provider, ProviderConfigSnapshot};
use singularity_protocol::{
    AgentCapabilityResult, AppEvent, EventClass, EventDelivery, EventMetadata, InitializeParams,
    InitializeResult, JsonRpcId, JsonRpcMessage, Method, MethodKind, ProviderConfigurationStatus,
    ServerCapabilitiesResult, ServerShutdownResult, SessionDeleteResult, SessionIdParams,
    SessionReadParams, SessionReadResult, Thread, ThreadIdParams, ThreadListResult, ThreadResult,
    ThreadStartParams, ThreadStartResult, TransportCapability, Turn, TurnIdParams,
    TurnInjectionParams, TurnInterruptResult, TurnResult, TurnStartParams, TurnStartResult,
    TurnStatus,
};
use singularity_store::{
    SessionMetadataUpdate, SessionRecord, SessionStatus, SessionStore, StoreError,
    ensure_owner_only_file, now_iso,
};
use thiserror::Error;
use uuid::Uuid;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const SESSION_DELETE_TURN_ACTIVE: &str =
    "session/delete rejected: a turn is still active for this session";
const MAX_SESSION_TITLE_CHARS: usize = 120;
const SAFE_WORKSPACE_FAILURE: &str = "workspace capability unavailable";
const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";
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
    #[error("session error: {0}")]
    Session(#[from] SessionError),
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
        /// 原始失败文本；仅在 RPC 边界用于透出真实原因，不参与持久化分类。
        original: Option<String>,
    },
    #[error("turn execution failed during {stage} ({cause}); terminalization failed ({failure})")]
    TurnTerminalization {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        failure: TurnTerminalizationFailure,
        /// 原始失败文本；仅在 RPC 边界用于透出真实原因，不参与持久化分类。
        original: Option<String>,
    },
}

/// `AppServer` 请求处理和生命周期操作使用的结果类型。
pub type AppServerResult<T> = Result<T, AppServerError>;

/// 已进入 Running Turn 的失败阶段；仅暴露稳定分类，不携带底层错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
    EventNotification,
}

impl TurnFailureStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
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

/// provider/模型边界失败的具体类别（对齐 Codex `TurnError` 粒度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    RateLimited,
    QuotaExceeded,
    Network,
    Timeout,
    Auth,
    Validation,
    Overloaded,
    Cancelled,
    ContextOverflow,
    Unknown,
}

/// 已进入 Running Turn 后失败的稳定原始原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureCause {
    Store,
    Workspace,
    ProjectInstructions,
    Serialization,
    StoredInputUnavailable,
    /// provider/模型边界失败（限流/配额/网络/校验等，见 `ProviderFailureKind`）。
    Provider(ProviderFailureKind),
    Internal,
}

impl TurnFailureCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Workspace => "workspace",
            Self::ProjectInstructions => "project_instructions",
            Self::Serialization => "serialization",
            Self::StoredInputUnavailable => "stored_input_unavailable",
            Self::Provider(kind) => match kind {
                ProviderFailureKind::RateLimited => "provider_rate_limited",
                ProviderFailureKind::QuotaExceeded => "provider_quota_exceeded",
                ProviderFailureKind::Network => "provider_network",
                ProviderFailureKind::Timeout => "provider_timeout",
                ProviderFailureKind::Auth => "provider_auth",
                ProviderFailureKind::Validation => "provider_validation",
                ProviderFailureKind::Overloaded => "provider_overloaded",
                ProviderFailureKind::Cancelled => "provider_cancelled",
                ProviderFailureKind::ContextOverflow => "provider_context_overflow",
                ProviderFailureKind::Unknown => "provider_unknown",
            },
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 终态补偿失败的稳定分类。
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    CancelRequested,
    Completed,
    Cancelled,
    Failed,
}

impl AgentStatus {
    /// 返回稳定的生命周期状态字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// agent 运行终态（app-server 内部类型）。
#[derive(Debug, Clone, PartialEq)]
pub struct RunStatus {
    pub status: AgentStatus,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub model_usage: ModelUsage,
    pub error: Option<String>,
}

impl RunStatus {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentStatus::Failed,
            final_answer: None,
            model_turns: 0,
            model_usage: ModelUsage::default(),
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnFailure {
    stage: TurnFailureStage,
    cause: TurnFailureCause,
    /// 携带到 RPC 边界的原始失败文本；无原文时为 `None`。
    original: Option<String>,
}

/// 将 provider 层聚合 usage 投影为协议线格式。
pub fn usage_to_wire(usage: &ModelUsage) -> singularity_protocol::TurnModelUsage {
    singularity_protocol::TurnModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cost_estimate: usage.cost_estimate,
        usage_present: usage.usage_present,
    }
}

impl From<TurnFailureStage> for TurnFailure {
    fn from(stage: TurnFailureStage) -> Self {
        Self {
            stage,
            cause: TurnFailureCause::Internal,
            original: None,
        }
    }
}

/// 一次 AgentLoop 调用预分配的 assistant item 事件状态（只用于实时协议事件）。
pub struct AssistantItemEventState {
    pub item_id: String,
    first_delta_observed: bool,
    started_generated: bool,
    delta_generated: bool,
}

impl AssistantItemEventState {
    pub fn new(item_id: String) -> Self {
        Self {
            item_id,
            first_delta_observed: false,
            started_generated: false,
            delta_generated: false,
        }
    }

    pub fn appeared(&self) -> bool {
        self.started_generated || self.delta_generated
    }
}

/// AppServer 交给 stdout transport 的消息。
pub type AppServerOutput = Value;

/// 协调 session 索引、信任和活动 turn 的有状态 JSON-RPC 服务。
pub struct AppServer {
    store: SessionStore,
    sessions_dir: PathBuf,
    initialized: bool,
    initialized_acknowledged: bool,
    shutdown_requested: bool,
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// turn id -> session id（同一连接内 turn/steer、turn/followUp 响应需要）。
    turn_threads: Arc<Mutex<HashMap<String, String>>>,
    /// 每个活动 turn 的 steer/follow-up 注入句柄（turn/steer、turn/followUp）。
    steer_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
    follow_up_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
    /// turn 已终态后到达的 steer 输入按 thread（session）排队，下一次 turn/start 取走
    /// （Pi 式 thread 级队列；M2 裁决方案 B）。
    thread_steer_pending: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    thread_follow_up_pending: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    /// 已提交 turn 的聚合 usage（进程内缓存；usage 不持久化到索引之外）。
    usage_by_turn: Arc<Mutex<HashMap<String, singularity_model::ModelUsage>>>,
    execution_stopped: Arc<AtomicBool>,
    #[cfg(test)]
    terminalization_faults: Arc<Mutex<TerminalizationFaults>>,
    #[doc(hidden)]
    pub test_provider_override:
        Option<std::sync::Arc<dyn singularity_model::Provider + Send + Sync>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct TerminalizationFaults {
    metadata_failures_remaining: usize,
    event_failures_remaining: usize,
}

/// 由请求工作线程与 stdio 传输层共享的可克隆停止句柄。
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
    follow_up_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
        // turn_threads 保留 turn→thread 历史映射（M2：终态后 steer/followUp 仍
        // 需要按 turn_id 解析 thread 入待办队列）；活跃判定只看 active_turns。
        if let Ok(mut steer_handles) = self.steer_handles.lock() {
            steer_handles.remove(&self.turn_id);
        }
        if let Ok(mut follow_up_handles) = self.follow_up_handles.lock() {
            follow_up_handles.remove(&self.turn_id);
        }
    }
}

fn event_contract(event: &AppEvent) -> (EventClass, EventDelivery) {
    match event.method.as_str() {
        "item/agentMessage/delta" => (EventClass::Progress, EventDelivery::BestEffort),
        _ => (EventClass::State, EventDelivery::Reliable),
    }
}

fn json_response<T: serde::Serialize>(id: JsonRpcId, result: T) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

fn emit_messages(emit: &mut impl FnMut(Value), messages: Vec<Value>) {
    for message in messages {
        emit(message);
    }
}

fn canonical_thread_cwd(cwd: Option<&str>) -> Result<String, String> {
    let path = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => Path::new(cwd).to_path_buf(),
        Some(_) => return Err("thread cwd must not be empty".to_string()),
        None => std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    };
    let canonical =
        std::fs::canonicalize(&path).map_err(|_| "failed to bind thread cwd".to_string())?;
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

/// 持久化状态的原始投影：仅供内部（打开会话、provider 配置）使用；
/// wire 可见的 thread 摘要必须经过 [`AppServer::project_thread`]。
pub fn thread_from_record(record: &SessionRecord) -> Thread {
    Thread {
        thread_id: record.session_id.clone(),
        model: record.model.clone(),
        cwd: Some(record.cwd.clone()),
        last_turn_status: match record.status {
            None => None,
            Some(SessionStatus::Active) => Some(singularity_protocol::ThreadStatus::Active),
            Some(SessionStatus::Completed) => Some(singularity_protocol::ThreadStatus::Completed),
            Some(SessionStatus::Failed) => Some(singularity_protocol::ThreadStatus::Failed),
            Some(SessionStatus::Interrupted) => {
                Some(singularity_protocol::ThreadStatus::Interrupted)
            }
        },
    }
}

/// 将持久化的 `InputItem` 数组投影为拼接文本。
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

fn title_from_input(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_SESSION_TITLE_CHARS)
        .collect()
}

/// 组装新核心 `Agent` 的配置。
fn agent_config_for_thread(
    thread: &Thread,
    provider: &dyn Provider,
    snapshot: &ProviderConfigSnapshot,
) -> AppServerResult<AgentConfig> {
    let cwd = workspace_path(thread)
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
    // AGENTS.md 与 Pi 一致无条件逐层加载（root→cwd），不再做 trust 门控。
    let system_prompt = match load_project_instructions_from_cwd(&cwd) {
        Ok(Some(instructions)) => instructions.content().to_string(),
        Ok(None) => String::new(),
        Err(error) => return Err(AppServerError::ProjectInstructions(error)),
    };
    let context_window = provider
        .protocol_contract()
        .max_context_tokens
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS) as u64;
    let max_output_tokens = provider.protocol_contract().max_output_tokens as u64;
    Ok(AgentConfig {
        model: thread
            .model
            .clone()
            .or_else(|| snapshot.resolved_default_selector())
            .unwrap_or_default(),
        system_prompt,
        context_window,
        max_output_tokens,
        ..AgentConfig::default()
    })
}

/// 新核心 `AgentOutcome` → app-server `RunStatus` 投影。
fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus::failed("agent loop did not reach a final assistant message");
    if outcome.aborted {
        mark_run_cancelled(&mut status);
    } else if outcome.final_text.trim().is_empty() {
        status.status = AgentStatus::Failed;
        status.error =
            Some("agent loop exhausted its turn budget without a final message".to_string());
    } else {
        status.status = AgentStatus::Completed;
        status.error = None;
        status.final_answer = Some(outcome.final_text.clone());
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
    pub fn new(store: SessionStore, provider_snapshot: ProviderConfigSnapshot) -> Self {
        let sessions_dir = user_singularity_home()
            .map(|home| home.join(paths::SESSIONS_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(".singularity/sessions"));
        Self {
            store,
            sessions_dir,
            initialized: false,
            initialized_acknowledged: false,
            shutdown_requested: false,
            provider_snapshot,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            turn_threads: Arc::new(Mutex::new(HashMap::new())),
            steer_handles: Arc::new(Mutex::new(HashMap::new())),
            follow_up_handles: Arc::new(Mutex::new(HashMap::new())),
            thread_steer_pending: Arc::new(Mutex::new(HashMap::new())),
            thread_follow_up_pending: Arc::new(Mutex::new(HashMap::new())),
            usage_by_turn: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            terminalization_faults: Arc::new(Mutex::new(TerminalizationFaults::default())),
            test_provider_override: None,
        }
    }

    /// 仅测试：覆盖会话目录。
    #[doc(hidden)]
    pub fn with_sessions_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.sessions_dir = dir.as_ref().to_path_buf();
        self
    }

    /// 仅测试：注入动态 provider 覆盖。
    #[doc(hidden)]
    pub fn with_test_provider(
        mut self,
        provider: std::sync::Arc<dyn singularity_model::Provider + Send + Sync>,
    ) -> Self {
        self.test_provider_override = Some(provider);
        self
    }

    #[cfg(test)]
    pub(crate) fn inject_terminalization_faults(
        &self,
        metadata_failures: usize,
        event_failures: usize,
    ) {
        if let Ok(mut faults) = self.terminalization_faults.lock() {
            faults.metadata_failures_remaining = metadata_failures;
            faults.event_failures_remaining = event_failures;
        }
    }

    #[cfg(test)]
    fn consume_terminal_metadata_failure(&self) -> bool {
        let Ok(mut faults) = self.terminalization_faults.lock() else {
            return false;
        };
        if faults.metadata_failures_remaining == 0 {
            return false;
        }
        faults.metadata_failures_remaining -= 1;
        true
    }

    #[cfg(not(test))]
    fn consume_terminal_metadata_failure(&self) -> bool {
        false
    }

    #[cfg(test)]
    pub(crate) fn consume_terminal_event_failure(&self, method: &str) -> bool {
        if !matches!(method, "turn/completed" | "turn/error" | "item/failed") {
            return false;
        }
        let Ok(mut faults) = self.terminalization_faults.lock() else {
            return false;
        };
        if faults.event_failures_remaining == 0 {
            return false;
        }
        faults.event_failures_remaining -= 1;
        true
    }

    #[cfg(not(test))]
    pub(crate) fn consume_terminal_event_failure(&self, _method: &str) -> bool {
        false
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn ready_for_turn_worker(&self) -> bool {
        self.initialized_acknowledged
    }

    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.cancellation_handle().request_execution_stop()
    }

    pub fn cancellation_handle(&self) -> AppServerCancellationHandle {
        AppServerCancellationHandle {
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
        }
    }

    /// 为单一 turn 工作线程打开独立索引连接，同时共享停止与注入状态。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: self.store.trusted_reopen()?,
            sessions_dir: self.sessions_dir.clone(),
            initialized: true,
            initialized_acknowledged: true,
            shutdown_requested: false,
            provider_snapshot: self.provider_snapshot.clone(),
            active_turns: Arc::clone(&self.active_turns),
            turn_threads: Arc::clone(&self.turn_threads),
            steer_handles: Arc::clone(&self.steer_handles),
            follow_up_handles: Arc::clone(&self.follow_up_handles),
            thread_steer_pending: Arc::clone(&self.thread_steer_pending),
            thread_follow_up_pending: Arc::clone(&self.thread_follow_up_pending),
            usage_by_turn: Arc::clone(&self.usage_by_turn),
            execution_stopped: Arc::clone(&self.execution_stopped),
            #[cfg(test)]
            terminalization_faults: Arc::clone(&self.terminalization_faults),
            test_provider_override: self.test_provider_override.clone(),
        })
    }

    pub(crate) fn activate_turn(
        &self,
        turn_id: &str,
        thread_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let cancellation = CancellationToken::new();
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        if active_turns.contains_key(turn_id) {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is already active"
            )));
        }
        if self.execution_stopped.load(Ordering::SeqCst) {
            cancellation.cancel();
        }
        active_turns.insert(turn_id.to_string(), cancellation.clone());
        drop(active_turns);
        self.turn_threads
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .insert(turn_id.to_string(), thread_id.to_string());
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
            steer_handles: Arc::clone(&self.steer_handles),
            follow_up_handles: Arc::clone(&self.follow_up_handles),
        };
        Ok((cancellation, guard))
    }

    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        if let Some(selector) = selector
            && (self.provider_snapshot.has_explicit_model_selection()
                || selector.contains('/')
                || selector.contains('#'))
        {
            self.provider_snapshot
                .provider_for_selector(Some(selector))
                .map(|_| ())
                .map_err(|_| AppServerError::InvalidParams("invalid model selector".to_string()))?;
        }
        Ok(())
    }

    fn provider_for_thread(
        &self,
        thread: &Thread,
    ) -> Result<singularity_model::OpenAiProvider, singularity_model::ProviderError> {
        self.provider_snapshot
            .provider_for_selector(thread.model.as_deref())
    }

    fn provider_and_config_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<(Arc<dyn Provider + Send + Sync>, AgentConfig)> {
        let provider: Arc<dyn Provider + Send + Sync> =
            if let Some(test_provider) = &self.test_provider_override {
                Arc::clone(test_provider)
            } else {
                Arc::new(self.provider_for_thread(thread).map_err(|error| {
                    AppServerError::TurnExecution {
                        stage: TurnFailureStage::AgentLoop,
                        cause: TurnFailureCause::Internal,
                        original: Some(error.to_string()),
                    }
                })?)
            };
        let config = agent_config_for_thread(thread, provider.as_ref(), &self.provider_snapshot)?;
        Ok((provider, config))
    }

    pub(crate) fn open_session_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<SessionManager> {
        let record = self.store.get_session(&thread.thread_id)?;
        let session = SessionManager::open_existing(Path::new(&record.rollout_path))?;
        if session.session_id() != record.session_id {
            return Err(AppServerError::Store(StoreError::InvalidState(format!(
                "rollout header id {} does not match index session id {}",
                session.session_id(),
                record.session_id
            ))));
        }
        Ok(session)
    }

    pub(crate) fn update_session_status_and_usage(
        &self,
        session_id: &str,
        status: SessionStatus,
        usage: &ModelUsage,
    ) -> AppServerResult<SessionRecord> {
        if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
        ) && self.consume_terminal_metadata_failure()
        {
            return Err(AppServerError::Store(StoreError::InvalidState(
                "injected terminal metadata failure".to_string(),
            )));
        }
        let token_usage = serde_json::to_value(usage_to_wire(usage))?;
        Ok(self.store.update_session(
            session_id,
            SessionMetadataUpdate {
                status: Some(status),
                token_usage: Some(&token_usage),
                ..SessionMetadataUpdate::default()
            },
        )?)
    }

    /// wire 可见的 thread 摘要：持久化 `Active` 只有在本进程存在该会话的
    /// 存活 turn 时才成立；崩溃遗留的 `Active` 投影为 `interrupted`，读取
    /// 不回写索引（终态只能由 turn 的真实结束写入）。
    pub(crate) fn project_thread(&self, record: &SessionRecord) -> Thread {
        let mut thread = thread_from_record(record);
        if thread.last_turn_status == Some(singularity_protocol::ThreadStatus::Active)
            && !self.thread_has_live_turn(&record.session_id)
        {
            thread.last_turn_status = Some(singularity_protocol::ThreadStatus::Interrupted);
        }
        thread
    }

    fn thread_has_live_turn(&self, session_id: &str) -> bool {
        // turn_threads 保留历史映射（M2 队列寻址），存活判定只看仍持有取消
        // 令牌的 turn（active_turns ∩ turn_threads）。
        let active = self.active_turns.lock();
        let threads = self.turn_threads.lock();
        match (active, threads) {
            (Ok(active), Ok(turn_threads)) => active.keys().any(|turn_id| {
                turn_threads
                    .get(turn_id)
                    .is_some_and(|sid| sid == session_id)
            }),
            // 锁中毒视为没有存活 turn：宁可投影为终态也不伪装运行中。
            _ => false,
        }
    }

    pub(crate) fn turn_with_usage(&self, turn: Turn) -> Turn {
        let usage = self
            .usage_by_turn
            .lock()
            .ok()
            .and_then(|cache| cache.get(&turn.turn_id).cloned());
        match usage {
            Some(usage) => Turn {
                model_usage: Some(usage_to_wire(&usage)),
                ..turn
            },
            None => turn,
        }
    }

    pub(crate) fn remember_usage(&self, turn_id: &str, usage: &ModelUsage) {
        let _ = self.usage_by_turn.lock().map(|mut cache| {
            cache.insert(turn_id.to_string(), usage.clone());
        });
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

fn turn_failure_cause(error: &AppServerError) -> TurnFailureCause {
    match error {
        AppServerError::Store(_) => TurnFailureCause::Store,
        AppServerError::ProjectInstructions(_) => TurnFailureCause::ProjectInstructions,
        AppServerError::Workspace(_) => TurnFailureCause::Workspace,
        AppServerError::Agent(AgentError::Provider(provider_error)) => {
            TurnFailureCause::Provider(provider_failure_kind(&provider_error.error.kind))
        }
        AppServerError::Agent(_) => TurnFailureCause::Internal,
        AppServerError::InvalidJson(_) => TurnFailureCause::Serialization,
        AppServerError::InvalidParams(_) => TurnFailureCause::Internal,
        AppServerError::Session(_) => TurnFailureCause::Store,
        AppServerError::TurnExecution { cause, .. }
        | AppServerError::TurnTerminalization { cause, .. } => *cause,
    }
}

/// 把模型错误类型投影为 provider 失败类别（Codex 粒度）。
fn provider_failure_kind(kind: &singularity_model::ModelErrorKind) -> ProviderFailureKind {
    match kind {
        singularity_model::ModelErrorKind::RateLimited => ProviderFailureKind::RateLimited,
        singularity_model::ModelErrorKind::BudgetExceeded => ProviderFailureKind::QuotaExceeded,
        singularity_model::ModelErrorKind::NetworkError => ProviderFailureKind::Network,
        singularity_model::ModelErrorKind::Timeout => ProviderFailureKind::Timeout,
        singularity_model::ModelErrorKind::AuthError => ProviderFailureKind::Auth,
        singularity_model::ModelErrorKind::InvalidRequest
        | singularity_model::ModelErrorKind::ToolCallParseError
        | singularity_model::ModelErrorKind::JsonSchemaViolation
        | singularity_model::ModelErrorKind::ContentFilter => ProviderFailureKind::Validation,
        singularity_model::ModelErrorKind::ProviderOverloaded => ProviderFailureKind::Overloaded,
        singularity_model::ModelErrorKind::Cancelled => ProviderFailureKind::Cancelled,
        singularity_model::ModelErrorKind::ContextLengthExceeded => {
            ProviderFailureKind::ContextOverflow
        }
        singularity_model::ModelErrorKind::UnknownProviderError
        | singularity_model::ModelErrorKind::UnsupportedCapability => ProviderFailureKind::Unknown,
    }
}

fn turn_failure_from_error(
    error: &AppServerError,
    fallback_stage: TurnFailureStage,
) -> TurnFailure {
    match error {
        AppServerError::TurnExecution {
            stage,
            cause,
            original,
        }
        | AppServerError::TurnTerminalization {
            stage,
            cause,
            original,
            ..
        } => TurnFailure {
            stage: *stage,
            cause: *cause,
            original: original.clone().or_else(|| Some(error.to_string())),
        },
        _ => TurnFailure {
            stage: fallback_stage,
            cause: turn_failure_cause(error),
            original: Some(error.to_string()),
        },
    }
}

fn turn_status_for_agent(status: &AgentStatus) -> TurnStatus {
    match status {
        AgentStatus::Completed => TurnStatus::Completed,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => TurnStatus::Interrupted,
        AgentStatus::Running => TurnStatus::Running,
        AgentStatus::Failed => TurnStatus::Failed,
    }
}

fn session_status_for_agent(status: &AgentStatus) -> SessionStatus {
    match status {
        AgentStatus::Completed => SessionStatus::Completed,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => SessionStatus::Interrupted,
        AgentStatus::Running => SessionStatus::Active,
        AgentStatus::Failed => SessionStatus::Failed,
    }
}

fn mark_run_cancelled(status: &mut RunStatus) {
    status.status = AgentStatus::Cancelled;
    status.final_answer = None;
    status.error = None;
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

fn turn_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests;
