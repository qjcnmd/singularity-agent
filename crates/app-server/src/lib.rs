#![forbid(unsafe_code)]

//! 在进程边界负责 turn 准入、`AgentLoop` 执行、持久化和取消的 JSON-RPC 应用服务。
//!
//! 服务将协议处理与工作线程执行分离，并通过 `SessionStore` 提交终态后再发出对应事件。

mod dispatch;
mod events;
mod lifecycle;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
use singularity_protocol::{
    AgentCapabilityResult, AgentLoopCapabilityStatus, AppEvent, EventClass, EventDelivery,
    EventMetadata, EventSubscribeParams, EventSubscribeResult, InitializeParams, InitializeResult,
    Item, JsonRpcId, JsonRpcMessage, Method, MethodKind, ProviderConfigurationStatus,
    ServerCapabilitiesResult, ServerShutdownResult, Thread, ThreadDeleteResult, ThreadForkParams,
    ThreadForkResult, ThreadIdParams, ThreadListResult, ThreadReadParams, ThreadReadResult,
    ThreadResult, ThreadStartParams, ThreadStartResult, TransportCapability, Turn, TurnIdParams,
    TurnInputParams, TurnInterruptResult, TurnResult, TurnStartParams, TurnStartResult, TurnStatus,
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
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const DEFAULT_THREAD_HISTORY_TURN_LIMIT: usize = 64;
const MAX_THREAD_HISTORY_TURN_LIMIT: usize = 256;
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

/// 已进入 Running Turn 后失败的稳定原始原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureCause {
    Store,
    Workspace,
    ProjectInstructions,
    Serialization,
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

/// 将 provider 层聚合 usage 投影为协议线格式（评估工具数据源）。
fn usage_to_wire(usage: &ModelUsage) -> singularity_protocol::TurnModelUsage {
    singularity_protocol::TurnModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cost_estimate: usage.cost_estimate,
    }
}

impl From<TurnFailureStage> for TurnFailure {
    fn from(stage: TurnFailureStage) -> Self {
        Self {
            stage,
            cause: TurnFailureCause::Internal,
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

/// AppServer 交给 stdout transport 的消息；单 worker 传输无需全局排序。
pub type AppServerOutput = Value;

/// 协调线程、turn、追踪和工作线程的有状态 JSON-RPC 服务。
pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
    shutdown_requested: bool,
    provider_snapshot: ProviderConfigSnapshot,
    active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 每个活动 turn 的 steer 注入句柄（turn/input 运行中注入通道）。
    steer_handles: Arc<Mutex<HashMap<String, SteerHandle>>>,
    /// 已提交 turn 的聚合 usage（进程内缓存；usage 不持久化，裁决 6）。
    usage_by_turn: Arc<Mutex<HashMap<String, singularity_model::ModelUsage>>>,
    execution_stopped: Arc<AtomicBool>,
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
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
        if let Ok(mut steer_handles) = self.steer_handles.lock() {
            steer_handles.remove(&self.turn_id);
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
///
/// 非取消但无最终答复（如耗尽 max_turns）时标为 Failed：store 不变量要求
/// Completed 必须携带 assistant 消息，直接提交会以 Internal error 失败。
fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus::failed("agent loop did not reach a final assistant message");
    if outcome.aborted {
        mark_run_cancelled(&mut status);
    } else if outcome.final_text.trim().is_empty() {
        status.status = AgentStatus::Failed;
        status.error = Some(
            "agent loop exhausted its turn budget without a final message".to_string(),
        );
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
