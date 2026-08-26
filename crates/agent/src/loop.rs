//! Singularity 核心 Agent 执行循环。
//!
//! 采用三层结构：
//! - **轮步层（`run`）**：内层循环逐轮驱动，发送前基于上一轮真实 usage（缺失时用
//!   装配估算）做主动压缩，调用采样层，并在 provider 明确返回 ContextOverflow 时
//!   强制压缩、恰好一次重发；外层循环在代理将要停止时消费停止窗口内到达的引导输入；
//! - **采样请求层（`sample_request`）**：按 `TurnRequestSpec` 装配请求一次，独立的
//!   重试包装——可重试 provider 错误指数退避重试，ContextOverflow 上抛由轮步层处理；
//! - **发送层（`attempt_request`）**：纯发送，仅调用流式 completion，不感知压缩、
//!   重试与溢出。
//!
//! 会话状态持久化、上下文压缩、工具注册分发与模型调用分别由
//! `session/` facade、`compaction.rs`、`tools/` 与 `singularity_model` 模块提供支持。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use singularity_core::CancellationToken;
use singularity_model::{
    DEFAULT_MAX_TOOLS_PER_REQUEST, ModelError, ModelErrorKind, ModelMessage, ModelPreferences,
    ModelRole, ModelToolSchema, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptEvent, ProviderError,
    ProviderProtocolContract, ProviderReasoningReplay, ProviderStreamEvent,
    ProviderToolReasoningMode, ToolChoicePolicy, is_strict_tool_schema_compatible,
    split_model_selector,
};
use thiserror::Error;
use uuid::Uuid;

use crate::compaction::{
    CompactionBudget, CompactionConfig, CompactionEngine, CompactionOutcome, CompactionReason,
};
use crate::message::{
    AgentMessage, AgentMessageRole, ContentBlock, assistant_response_message, tool_result_message,
    user_message,
};
use crate::session::context::entry_to_llm_messages;
use crate::session::{SessionEntry, SessionEntryType, SessionError, SessionManager};
use crate::tools::{
    ExecuteContext, PreparedTool, ToolError, ToolExecution, ToolPreflight, ToolRegistry,
};

/// Agent 层重试上限：模型调用返回可重试错误时，在此层指数退避重试。
const MAX_TURN_RETRIES: u32 = 3;
/// 重试基础退避毫秒：delay = base × 2^(attempt-1)，再乘 ±10% 抖动。
const TURN_RETRY_BASE_DELAY_MS: u64 = 2_000;
/// 退避等待的取消轮询间隔。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

/// 判断 provider 错误是否属于 agent 层可重试类别。
///
/// 与 pi 的 `isRetryableAssistantError` 同向：限流、网络、超时、过载与未知
/// 错误可重试；认证、校验、配额、取消与上下文溢出（后者走强制压缩路径）
/// 不重试。
fn is_retryable_provider_error(error: &ProviderError) -> bool {
    use ModelErrorKind::*;
    error.automatic_retry_allowed
        && matches!(
            error.error.kind,
            RateLimited | NetworkError | Timeout | ProviderOverloaded | UnknownProviderError
        )
}

/// 指数退避 + ±10% 确定性抖动（Codex `retry.rs` 同款范围；由 attempt
/// 派生的 21 步周期伪随机，避免随机依赖并使同一 attempt 可复现）。
fn retry_delay_ms(
    base_delay_ms: u64,
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> u64 {
    if let Some(retry_after) = retry_after {
        return retry_after.as_millis().min(u128::from(u64::MAX)) as u64;
    }
    let base = base_delay_ms * 2u64.saturating_pow(attempt.saturating_sub(1));
    // 抖动因子 ∈ [0.90, 1.10)。
    let jitter = 0.9 + (u64::from(attempt) * 37 % 21) as f64 / 100.0;
    (base as f64 * jitter) as u64
}

/// 可中断的同步退避等待；返回 false 表示等待期间被取消。
fn sleep_abortable(millis: u64, cancellation: &CancellationToken) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
    while std::time::Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(RETRY_POLL_INTERVAL_MS));
    }
    !cancellation.is_cancelled()
}

/// 非致命运行时诊断的严重级别（AgentLoop 发射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

impl AgentDiagnosticSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for AgentDiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 安全、非持久化的诊断。`code` 对投影方稳定；`message` 文本刻意
/// 不包含原始 provider payload（脱敏边界）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDiagnostic {
    pub severity: AgentDiagnosticSeverity,
    pub code: String,
    pub message: String,
}

impl AgentDiagnostic {
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: AgentDiagnosticSeverity::Info,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: AgentDiagnosticSeverity::Warning,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Agent 运行生命周期事件，统一经 `AgentEvents::on_event` 出口流式投递。
///
/// tool 事件按调用的串行执行顺序投递；持久化的 toolResult 顺序不受影响。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 模型流式文本输出增量更新。
    MessageUpdate { delta: String },
    /// 工具开始执行事件。
    ToolExecutionStarted {
        tool_name: String,
        tool_call_id: String,
        arguments: Value,
    },
    /// 工具执行中产生的流式增量输出事件。
    ToolExecutionUpdate {
        tool_name: String,
        tool_call_id: String,
        arguments: Value,
        partial_result: String,
    },
    /// 工具执行完成事件。
    ToolExecutionEnded {
        tool_name: String,
        tool_call_id: String,
        execution: ToolExecution,
    },
    /// 非致命、脱敏 Agent 诊断；不会写入 Session JSONL。
    Diagnostic(AgentDiagnostic),
    /// provider HTTP attempt 生命周期观测；model-turn 序号已在循环内绑定。
    ///
    /// 投影为尽力而为；消费方自行吸收投影失败，不影响 provider 结果。
    ProviderAttempt {
        model_turn_ordinal: u32,
        event: ProviderAttemptEvent,
    },
}

/// Agent 运行生命周期事件出口。
///
/// 单一回调统一承载全部事件。投影为尽力而为：消费方自行吸收失败，
/// 不改变轮次结果。
pub struct AgentEvents<'a> {
    pub on_event: Option<&'a mut dyn FnMut(AgentEvent)>,
}

impl<'a> AgentEvents<'a> {
    pub fn new() -> Self {
        Self { on_event: None }
    }
}

impl Default for AgentEvents<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Agent 层重试配置（pi 策略：可重试 provider 错误指数退避重试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRetryConfig {
    /// 重试上限；0 表示禁用 agent 层重试。
    pub max_retries: u32,
    /// 基础退避毫秒：delay = base × 2^(attempt-1) × 抖动。
    pub base_delay_ms: u64,
}

impl Default for TurnRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_TURN_RETRIES,
            base_delay_ms: TURN_RETRY_BASE_DELAY_MS,
        }
    }
}

/// Agent 运行配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// `provider/modelId` 选择器（与 config 约定同构，如 `opencode-go/deepseek-v4-flash#max`）。
    /// 为空时使用 provider 自身默认模型。
    pub model: String,
    pub system_prompt: String,
    /// 模型静态声明的 context window（compaction 触发预算依据）。
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub compaction: CompactionConfig,
    /// Agent 层的唯一自动重试策略。
    pub retry: TurnRetryConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system_prompt: String::new(),
            context_window: 128_000,
            max_output_tokens: 4_096,
            compaction: CompactionConfig::default(),
            retry: TurnRetryConfig::default(),
        }
    }
}

impl AgentConfig {
    /// 按 provider 声明的输出上限钳制并校验 compaction 配置：默认 summary
    /// 预算可被更小的 provider 上限安全下调，显式非默认预算保持 fail-closed。
    /// turn 启动前的准备阶段与 `Agent::new` 必须共用这一个入口，使「准备阶段
    /// 已校验」与「Agent 构造不再失败」成为同一事实而不是两份手工同步的逻辑。
    pub fn prepare_for_provider_limits(
        mut config: Self,
        provider_max_output_tokens: u32,
    ) -> Result<Self> {
        if config.compaction == CompactionConfig::default()
            && provider_max_output_tokens < config.compaction.summary_max_tokens
        {
            config.compaction.summary_max_tokens = provider_max_output_tokens;
        }
        config
            .compaction
            .validate(config.context_window, provider_max_output_tokens)?;
        Ok(config)
    }
}

/// Agent 循环错误。
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("compaction error: {0}")]
    Compaction(#[from] crate::compaction::CompactionError),
    #[error("agent loop error: {0}")]
    Loop(String),
    /// 轮次已积累持久事实后的 provider/session 失败。
    /// 内部错误保持权威根因，`outcome` 携带失败前已观察到的下限 turns/usage。
    #[error("agent run failed after partial progress: {error}")]
    RunFailed {
        error: Box<AgentError>,
        outcome: Box<AgentOutcome>,
    },
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 的终止原因。错误细节继续由 `AgentError` 携带，避免在 outcome 中
/// 复制第二套错误事实源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
    Failed,
}

/// 一次 `run` 的最终结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最后一次无工具调用的 assistant 文本（中断/轮数上限时可能为空）。
    pub final_text: String,
    /// 最终 assistant 响应是否因 provider 输出预算耗尽而截断。
    pub truncated: bool,
    pub turns: u32,
    /// 各轮 provider 调用的聚合 usage。
    pub usage: ModelUsage,
    pub compacted: bool,
    /// `true` 表示每个已发出的 provider 请求都带有可确认的 usage；
    /// 取消/失败时未知的末次请求保持 `false`，不得估算成精确值。
    pub usage_complete: bool,
    pub terminal_reason: AgentTerminalReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TurnInboxState {
    #[default]
    Open,
    Closed,
}

/// 活动 turn 的单一转向输入箱。
///
/// `enqueue`、`drain` 与 `take_at_stop` 都在调用方持有的同一把 Mutex 内运行。
/// 自然终止点调用 `take_at_stop` 时，箱内已有输入会被取出并继续执行；只有
/// 箱为空时才原子地转为 Closed，之后的输入明确拒绝。这保证不存在“已接受但
/// 丢失”的中间状态，也不引入持久队列或 grace period。turn 之间的后续输入
/// 队列由调用方的 Thread 协调器持有，不进入本箱。
#[derive(Debug, Default)]
pub struct TurnInbox {
    state: TurnInboxState,
    entries: VecDeque<String>,
}

impl TurnInbox {
    pub fn enqueue(&mut self, text: impl Into<String>) -> bool {
        if self.state == TurnInboxState::Closed {
            return false;
        }
        self.entries.push_back(text.into());
        true
    }

    fn drain(&mut self) -> Vec<String> {
        self.entries.drain(..).collect()
    }

    /// 自然停止点原子屏障：箱内已有输入时保持开启并交给下一轮消费；
    /// 箱为空时永久关闭，之后的输入明确拒绝（不存在“已接受但丢失”）。
    fn take_at_stop(&mut self) -> Option<Vec<String>> {
        if self.entries.is_empty() {
            self.state = TurnInboxState::Closed;
            None
        } else {
            Some(self.drain())
        }
    }

    fn close(&mut self) {
        self.state = TurnInboxState::Closed;
    }
}

/// 活动 turn 转向输入箱的线程安全句柄。
pub type TurnInboxHandle = Arc<Mutex<TurnInbox>>;

/// 单轮 provider 请求的静态规格：除轮次序号外，一次 `run` 内恒定不变。
struct TurnRequestSpec {
    preferences: ModelPreferences,
    tools: Vec<ModelToolSchema>,
    tool_choice: ToolChoicePolicy,
    max_output_tokens: u32,
    turn: u32,
}

/// preflight 判定结果：可执行工具，或模型可见的拒绝执行。
enum Prepared {
    Ready(PreparedTool),
    Rejected(ToolExecution),
}

/// 一次模型工具调用及其 preflight 判定。
struct PreparedToolCall {
    call: singularity_model::ModelToolCall,
    prepared: Prepared,
}

enum AttemptOutcome {
    Response(Box<ModelTurnResponse>),
    Aborted,
    Failed(AgentError),
}

/// 通过单一事件出口投递一个事件；事件投影是尽力而为的，回调不再返回
/// 错误——投影失败由消费方（app-server/CLI）自行吸收诊断，不影响轮次结果。
fn emit(events: &mut AgentEvents<'_>, event: AgentEvent) {
    if let Some(callback) = events.on_event.as_deref_mut() {
        callback(event);
    }
}

fn tool_error_execution(error: impl std::fmt::Display) -> ToolExecution {
    ToolExecution {
        content: format!("tool execution failed: {error}"),
        is_error: true,
    }
}

fn execute_prepared_tool(
    registry: &ToolRegistry,
    prepared: PreparedTool,
    call: &singularity_model::ModelToolCall,
    cwd: &Path,
    cancellation: &CancellationToken,
    mut on_update: impl FnMut(&str),
) -> ToolExecution {
    let mut update = |text: &str| on_update(text);
    match registry.execute_prepared(
        prepared,
        ExecuteContext {
            args: call.arguments.clone(),
            cwd,
            signal: Some(cancellation),
            on_update: Some(&mut update),
        },
    ) {
        Ok(execution) => execution,
        Err(error) => tool_error_execution(error),
    }
}

/// 按模型给定的 source order 串行执行一批工具调用：每个工具保留
/// `catch_unwind` panic 隔离与逐工具事件发射；preflight 拒绝项不进入执行，
/// 直接以模型可见失败收尾。单个工具失败不影响其余调用继续执行。
fn execute_tool_batch(
    registry: &ToolRegistry,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    events: &mut AgentEvents<'_>,
) -> Result<Vec<ToolExecution>> {
    let mut results = Vec::with_capacity(calls.len());
    for item in calls {
        emit(
            events,
            AgentEvent::ToolExecutionStarted {
                tool_name: item.call.tool_name.clone(),
                tool_call_id: item.call.tool_call_id.clone(),
                arguments: item.call.arguments.clone(),
            },
        );
        match &item.prepared {
            Prepared::Rejected(execution) => {
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: item.call.tool_name.clone(),
                        tool_call_id: item.call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                results.push(execution.clone());
                continue;
            }
            Prepared::Ready(prepared) => {
                let prepared = prepared.clone();
                let call = item.call.clone();
                let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_prepared_tool(registry, prepared, &call, cwd, cancellation, |text| {
                        emit(
                            events,
                            AgentEvent::ToolExecutionUpdate {
                                tool_name: call.tool_name.clone(),
                                tool_call_id: call.tool_call_id.clone(),
                                arguments: call.arguments.clone(),
                                partial_result: text.to_string(),
                            },
                        );
                    })
                }))
                .unwrap_or_else(|_| tool_error_execution("tool execution panicked"));
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: call.tool_name.clone(),
                        tool_call_id: call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                results.push(execution);
            }
        }
    }
    Ok(results)
}

/// 把系统/开发者指令投影为请求首条消息：恒以 Developer 角色构造，
/// 对不支持 developer 角色的端点由 wire 层按 `supports_developer_role`
/// 降级为 system（用户配置，默认 true）。
pub(crate) fn instruction_message(instruction: &str) -> Option<ModelMessage> {
    if instruction.is_empty() {
        return None;
    }
    Some(ModelMessage::text(ModelRole::Developer, instruction))
}

/// 有效输出上限 = min(配置值, provider 静态能力声明)；正常请求与
/// compaction 摘要请求共用，避免摘要派生出超过模型上限的 max_tokens。
fn effective_max_output_tokens(provider: &dyn Provider, configured: u64) -> u32 {
    u32::try_from(configured.min(provider.protocol_contract().max_output_tokens as u64))
        .unwrap_or(u32::MAX)
}

fn is_cancelled_agent_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::Provider(provider) if provider.error.kind == ModelErrorKind::Cancelled
    )
}

fn is_context_overflow_error(error: &ModelError) -> bool {
    error.kind == ModelErrorKind::ContextLengthExceeded
}

/// 逐轮聚合 provider 返回的真实 token/cache usage。
fn record_usage(outcome: &mut AgentOutcome, response: &ModelUsage) {
    outcome.usage.merge(response);
    if !response.usage_present {
        outcome.usage_complete = false;
    }
}

fn record_compaction(outcome: &mut AgentOutcome, result: &CompactionOutcome) {
    let CompactionOutcome::Compacted {
        usage,
        usage_complete,
        ..
    } = result
    else {
        return;
    };
    outcome.compacted = true;
    outcome.usage.merge(usage);
    outcome.usage_complete &= *usage_complete;
}

fn emit_diagnostic(events: &mut AgentEvents, diagnostic: AgentDiagnostic) {
    // 诊断是尽力而为的观测侧信道。
    emit(events, AgentEvent::Diagnostic(diagnostic));
}

/// 加锁活动 turn inbox；中毒时恢复，避免一次工具 panic 使输入通道永久不可用。
fn lock_inbox(queue: &Mutex<TurnInbox>) -> std::sync::MutexGuard<'_, TurnInbox> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 新 headless core 的 Agent：会话 + compaction + 工具注册表 + 模型提供方。
pub struct Agent {
    session: SessionManager,
    compaction: CompactionEngine,
    registry: ToolRegistry,
    provider: Arc<dyn Provider + Send + Sync>,
    config: AgentConfig,
    /// 活动 turn 的实时转向输入箱；内存态不持久化。
    inbox: TurnInboxHandle,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider + Send + Sync>,
        registry: ToolRegistry,
        config: AgentConfig,
        session: SessionManager,
    ) -> Result<Self> {
        let provider_max_output_tokens = provider.protocol_contract().max_output_tokens;
        let config = AgentConfig::prepare_for_provider_limits(config, provider_max_output_tokens)?;
        // 摘要请求复用 provider/model 选择，但使用独立的摘要输出上限；
        // 正常 turn 的 max_output_tokens 不应把 8192-token 摘要压缩成 1 token。
        let mut compaction_preferences = ModelPreferences::default();
        if !config.model.is_empty() {
            compaction_preferences.model_name = Some(config.model.clone());
        }
        compaction_preferences.max_output_tokens = Some(effective_max_output_tokens(
            provider.as_ref(),
            config.compaction.summary_max_tokens as u64,
        ));
        let compaction = CompactionEngine::new(Arc::clone(&provider))
            .with_model_preferences(compaction_preferences)
            .with_summary_max_tokens(config.compaction.summary_max_tokens);
        Ok(Self {
            session,
            compaction,
            registry,
            provider,
            config,
            inbox: Arc::new(Mutex::new(TurnInbox::default())),
        })
    }

    /// 返回 turn 实时转向输入队列的线程安全句柄。
    pub fn inbox_handle(&self) -> TurnInboxHandle {
        Arc::clone(&self.inbox)
    }

    /// 回收本轮持有的会话写者；一轮 turn 只打开一次会话文件，终态落盘必须
    /// 复用这里返回的同一 `SessionManager`，而不是再次全量打开。
    pub fn into_session(self) -> SessionManager {
        self.session
    }

    /// 注入转向：下一轮 provider 调用前作为 user 消息追加到会话上下文。
    pub fn steer(&mut self, text: &str) -> bool {
        lock_inbox(&self.inbox).enqueue(text.to_string())
    }

    /// 运行一个完整 Agent 循环：输入持久化为 user 消息，内层循环处理工具调用，
    /// 运行中注入的转向输入在后续轮次生效；停止后返回聚合结果。
    ///
    /// `cancellation` 取消时终止并返回已完成文本（`terminal_reason=Aborted`，不视为错误）。
    pub fn run(
        &mut self,
        input: &str,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let mut outcome = AgentOutcome {
            final_text: String::new(),
            truncated: false,
            turns: 0,
            usage: ModelUsage::default(),
            compacted: false,
            usage_complete: true,
            terminal_reason: AgentTerminalReason::Completed,
        };
        // 上一轮成功响应的 provider usage 只用于判定下一次请求前是否压缩。
        // 首轮或 usage 缺失时由装配估算兜底。
        let mut previous_context_tokens = None;
        self.session.append_message(user_message(input))?;

        let mut preferences = ModelPreferences::default();
        if !self.config.model.is_empty() {
            preferences.model_name = Some(self.config.model.clone());
        }
        // 静态能力声明决定 system prompt 角色、输出上限与 tool 策略。
        let capabilities = self.provider.protocol_contract();
        let max_output_tokens =
            effective_max_output_tokens(self.provider.as_ref(), self.config.max_output_tokens);
        let tools = self.tool_schemas(&capabilities);
        let tool_choice = ToolChoicePolicy {
            // 单条 assistant 消息允许的工具调用数上限；本地按模型给定顺序
            // 串行执行全部调用，wire 侧 parallel_tool_calls 恒为 false。
            max_tool_calls: DEFAULT_MAX_TOOLS_PER_REQUEST,
            strict_tool_schema: capabilities.supports_strict_tool_schema
                && tools
                    .iter()
                    .all(|tool| is_strict_tool_schema_compatible(&tool.parameters_schema)),
        };
        let mut spec = TurnRequestSpec {
            preferences,
            tools,
            tool_choice,
            max_output_tokens,
            turn: 0,
        };

        // 外层循环：代理将要停止时消费停止前到达的转向输入。
        loop {
            // 内层循环：工具调用与 steer 注入。
            loop {
                if cancellation.is_cancelled() {
                    return self.abort_outcome(outcome);
                }
                // 注入转向队列全部消息（作为 user 消息追加到本轮上下文）。
                let steer_messages = lock_inbox(&self.inbox).drain();
                for text in steer_messages {
                    self.append_session_or_fail(&mut outcome, user_message(&text))?;
                }
                let model_turn_ordinal = outcome.turns.saturating_add(1);
                spec.turn = outcome.turns;
                let response = match self.run_turn(
                    &spec,
                    previous_context_tokens,
                    &mut outcome,
                    events,
                    cancellation,
                    model_turn_ordinal,
                ) {
                    AttemptOutcome::Response(response) => *response,
                    AttemptOutcome::Aborted => return self.abort_outcome(outcome),
                    AttemptOutcome::Failed(error) => {
                        return self.fail_after_progress(error, outcome);
                    }
                };
                // 非 Success 响应（如校验失败 Invalid 等）：强类型向上传播，不在此层盲目重试。
                if response.status != ModelTurnStatus::Success {
                    let model_error = response.error.clone().unwrap_or_else(|| {
                        ModelError::new(
                            ModelErrorKind::UnknownProviderError,
                            "unknown provider error",
                        )
                    });
                    return self.fail_after_progress(
                        AgentError::Provider(ProviderError::from_model_error(model_error)),
                        outcome,
                    );
                }
                outcome.turns += 1;
                previous_context_tokens = response
                    .usage
                    .usage_present
                    .then_some(response.usage.total_tokens);
                record_usage(&mut outcome, &response.usage);
                let assistant_text = response
                    .assistant_message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let tool_calls = response.tool_calls.clone();
                let length_truncated = response.is_length_truncated();
                if length_truncated && !tool_calls.is_empty() {
                    // 截断的响应可能含有仅部分解析的工具调用。持久化 assistant
                    // 消息并为每个调用生成模型可见失败，但绝不执行这些调用或将
                    // 它们显示为成功的工具事件。
                    self.append_session_or_fail(
                        &mut outcome,
                        assistant_response_message(&response),
                    )?;
                    for call in &tool_calls {
                        self.append_session_or_fail(
                            &mut outcome,
                            tool_result_message(
                                &call.tool_call_id,
                                &call.tool_name,
                                &tool_error_execution(
                                    "model output was truncated before the tool call completed",
                                ),
                            ),
                        )?;
                    }
                    outcome.final_text = assistant_text;
                    continue;
                }
                if !tool_calls.is_empty() {
                    // 单次模型响应对应一条 Assistant 消息（包含思考、文本与全部 tool_call 块）。
                    self.append_session_or_fail(
                        &mut outcome,
                        assistant_response_message(&response),
                    )?;
                    // 查找、参数校验和执行模式判定先按 source order 完成；
                    // 未知工具/非法参数只生成模型可见失败，不进入并行线程。
                    let prepared_calls = tool_calls
                        .iter()
                        .map(|call| {
                            let prepared =
                                match self.registry.preflight(&call.tool_name, &call.arguments) {
                                    Ok(ToolPreflight::Ready(prepared)) => Prepared::Ready(prepared),
                                    Ok(ToolPreflight::Rejected(execution)) => {
                                        Prepared::Rejected(execution)
                                    }
                                    Err(error) => Prepared::Rejected(tool_error_execution(error)),
                                };
                            PreparedToolCall {
                                call: call.clone(),
                                prepared,
                            }
                        })
                        .collect::<Vec<_>>();
                    let executions = execute_tool_batch(
                        &self.registry,
                        &prepared_calls,
                        self.session.cwd(),
                        cancellation,
                        events,
                    )?;
                    // 持久的 toolResult 条目始终按 assistant source order 追加，
                    // 与完成/事件顺序无关。
                    for (call, execution) in tool_calls.iter().zip(executions.iter()) {
                        self.append_session_or_fail(
                            &mut outcome,
                            tool_result_message(&call.tool_call_id, &call.tool_name, execution),
                        )?;
                    }
                    if cancellation.is_cancelled() {
                        return self.abort_outcome(outcome);
                    }
                    continue;
                }
                // 无工具调用：终态 assistant 消息持久化并退出内层循环。
                self.append_session_or_fail(&mut outcome, assistant_response_message(&response))?;
                outcome.final_text = assistant_text;
                outcome.truncated = length_truncated;
                break;
            }
            // 代理将要停止：消费停止窗口内到达的转向输入后回到内层循环。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            for input in pending_inputs {
                self.append_session_or_fail(&mut outcome, user_message(&input))?;
            }
        }
    }

    /// 无条件执行一次 compaction（provider 明确返回 context overflow 时使用）。
    ///
    /// overflow 时不能保留正常近期窗口；强制路径把 retain ratio 压到 0，
    /// 只保留绝对必要的最近安全边界（toolResult 永不切）。
    fn force_compact(
        &mut self,
        cancellation: &CancellationToken,
        events: &mut AgentEvents,
    ) -> Result<CompactionOutcome> {
        let mut budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        // 强制溢出恢复是显式模式：provider 已拒绝该请求时，不保留正常
        // 近期内容比例。
        budget.retain_ratio = 0.0;
        let tokens_before = self.assembled_context_estimate()?;
        match self.compaction.compact_with_reason(
            &mut self.session,
            &budget,
            tokens_before,
            CompactionReason::ContextOverflow,
            cancellation,
        ) {
            Ok(result) => Ok(result),
            Err(crate::compaction::CompactionError::Session(error)) => {
                Err(AgentError::Session(error))
            }
            Err(error) => {
                emit_diagnostic(
                    events,
                    AgentDiagnostic::warning(
                        "compaction_failed",
                        "forced context compaction failed".to_string(),
                    ),
                );
                Err(AgentError::Compaction(error))
            }
        }
    }

    /// 用户显式请求的压缩：沿正常保留预算选择安全切点，但不要求上下文先
    /// 达到自动阈值。没有可摘要历史时返回 `NotNeeded`。
    pub fn compact_now(&mut self, cancellation: &CancellationToken) -> Result<CompactionOutcome> {
        let budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        let tokens_before = self.assembled_context_estimate()?;
        self.compaction
            .compact_with_reason(
                &mut self.session,
                &budget,
                tokens_before,
                CompactionReason::Manual,
                cancellation,
            )
            .map_err(AgentError::Compaction)
    }

    /// 以正常请求同一装配 seam 重建当前上下文并估算规模：压缩前记录的
    /// tokens_before 必须反映完整装配（消息、工具 schema、reasoning replay、
    /// 输出预算与固定余量），而非退化占位。
    fn assembled_context_estimate(&self) -> Result<u64> {
        let capabilities = self.provider.protocol_contract();
        let tools = self.tool_schemas(&capabilities);
        let (messages, replays) = self.assemble_messages()?;
        let max_output_tokens =
            effective_max_output_tokens(self.provider.as_ref(), self.config.max_output_tokens);
        Ok(self.estimate_assembled(&messages, &tools, &replays, max_output_tokens))
    }

    /// 上下文装配的单一 seam：指令消息 + compaction 感知会话历史 + reasoning
    /// replay 只在此一次完成。`build_request` 与 `assembled_context_estimate`
    /// 共用同一份装配成品，防止请求与估算各拼一遍产生不一致。
    fn assemble_messages(&self) -> Result<(Vec<ModelMessage>, Vec<ProviderReasoningReplay>)> {
        let entries = self.session.build_context_entries()?;
        let replays = self.reasoning_replays_from_entries(&entries);
        let mut messages = Vec::with_capacity(entries.len() + 1);
        if let Some(instruction) = instruction_message(&self.config.system_prompt) {
            messages.push(instruction);
        }
        messages.extend(entries.iter().flat_map(entry_to_llm_messages));
        Ok((messages, replays))
    }

    /// 对本轮装配结果做保守 Token 估算，供首轮或 provider usage 缺失时
    /// 的请求前压缩判定使用。
    ///
    /// 估算覆盖消息 content、工具调用标识与参数、工具 schema、provider
    /// reasoning replay 的序列化尺寸、输出预算及固定封装余量。
    fn estimate_assembled(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelToolSchema],
        replays: &[ProviderReasoningReplay],
        max_output_tokens: u32,
    ) -> u64 {
        let estimate = |text: &str| self.compaction.estimate_tokens(text);
        let message_tokens = messages
            .iter()
            .map(|message| {
                let mut tokens = estimate(&message.content);
                if let Some(tool_call_id) = &message.tool_call_id {
                    tokens = tokens.saturating_add(estimate(tool_call_id));
                }
                for call in &message.tool_calls {
                    tokens = tokens
                        .saturating_add(estimate(&call.tool_call_id))
                        .saturating_add(estimate(&call.tool_name))
                        .saturating_add(estimate(&call.raw_arguments));
                }
                tokens
            })
            .sum::<u64>();
        let tool_tokens =
            estimate(&serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_string()));
        let replay_tokens =
            estimate(&serde_json::to_string(replays).unwrap_or_else(|_| "[]".to_string()));
        message_tokens
            .saturating_add(tool_tokens)
            .saturating_add(replay_tokens)
            .saturating_add(max_output_tokens as u64)
            .saturating_add(32)
    }

    /// 按 `TurnRequestSpec` 组装单轮 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    ///
    /// 上下文条目只装配一次：messages、reasoning replay 与预算估算全部在
    /// 同一份装配成品上完成；返回 (请求, 装配成品估算)。
    fn build_request(&self, spec: &TurnRequestSpec) -> Result<(ModelTurnRequest, u64)> {
        let (messages, replays) = self.assemble_messages()?;
        let assembled_estimate =
            self.estimate_assembled(&messages, &spec.tools, &replays, spec.max_output_tokens);
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), spec.turn),
            messages,
        );
        request.tools = spec.tools.clone();
        request.tool_choice = spec.tool_choice.clone();
        request.provider_reasoning_history = replays;
        request.model_preferences = ModelPreferences {
            model_name: spec.preferences.model_name.clone(),
            max_output_tokens: Some(spec.max_output_tokens),
        };
        Ok((request, assembled_estimate))
    }

    /// 基于当前会话按同一装配 seam 重建请求；只返回请求本身（丢弃装配估算）。
    /// 主动压缩与溢出恢复在会话被修改后用它重建下一次发送的请求。
    fn rebuild_request(&self, spec: &TurnRequestSpec) -> Result<ModelTurnRequest> {
        let (request, _estimate) = self.build_request(spec)?;
        Ok(request)
    }

    /// 从 durable assistant entries 恢复 provider-private continuation。
    ///
    /// Responses replay 必须直接使用 JSONL 中保存的 opaque output items；
    /// reasoning summary 只作为可见投影，绝不用于重建 Responses state。
    /// Chat 旧条目若没有 private replay，仍可从 thinking block 重建
    /// `reasoning_content`，以保持已存在会话的兼容性。
    fn reasoning_replays_from_entries(
        &self,
        entries: &[SessionEntry],
    ) -> Vec<ProviderReasoningReplay> {
        let tool_reasoning_mode = self.provider.protocol_contract().tool_reasoning_mode;
        // (provider, model) 必须齐全；变体侧允许双侧同为空（Option 语义由
        // replay 兼容检查判定），无 #variant 的选择器不再静默丢弃 replay。
        let selector = {
            let parts = split_model_selector(&self.config.model);
            match (parts.provider, parts.model) {
                (Some(provider_name), Some(model_name)) => {
                    Some((provider_name, model_name, parts.effort))
                }
                _ => None,
            }
        };
        let mut replays = Vec::new();
        for entry in entries {
            let SessionEntryType::Message(message) = &entry.entry_type else {
                continue;
            };
            if message.role != AgentMessageRole::Assistant || !message.has_tool_calls() {
                continue;
            }
            if let Some(replay) = &message.provider_reasoning_replay {
                // provider/model 切换会使 opaque continuation 失效。保留会话中
                // 可见的 thinking/messages/tool results，但绝不跨不兼容的
                // provider 边界发送私有 replay。
                let Some((provider_name, model_name, variant)) = selector else {
                    continue;
                };
                if replay.is_compatible_with(
                    provider_name,
                    model_name,
                    variant,
                    tool_reasoning_mode,
                ) {
                    replays.push(replay.clone());
                }
                continue;
            }
            if tool_reasoning_mode != ProviderToolReasoningMode::ReplayReasoningContent {
                continue;
            }
            let Some((provider_name, model_name, variant)) = selector else {
                continue;
            };
            let thinking = message
                .thinking_blocks()
                .into_iter()
                .filter_map(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if thinking.is_empty() {
                continue;
            }
            let tool_call_ids: Vec<String> = message
                .tool_calls()
                .into_iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            if tool_call_ids.is_empty() {
                continue;
            }
            replays.push(ProviderReasoningReplay::Chat {
                provider_name: provider_name.to_string(),
                model_name: model_name.to_string(),
                reasoning_effort: variant.map(str::to_string),
                tool_call_ids,
                reasoning_content: thinking,
            });
        }
        replays
    }

    /// 注册表工具 → 模型可见 schema（不超过 provider 单请求上限）。
    fn tool_schemas(&self, capabilities: &ProviderProtocolContract) -> Vec<ModelToolSchema> {
        self.registry
            .names()
            .into_iter()
            .filter_map(|name| {
                self.registry
                    .get(name)
                    .map(|spec| (name, spec.description, spec.parameters.clone()))
            })
            .take(capabilities.max_tools_per_request as usize)
            .map(|(name, description, parameters)| ModelToolSchema {
                name: name.to_string(),
                description: description.to_string(),
                parameters_schema: parameters,
            })
            .collect()
    }

    /// 装配单轮请求一次，并在发送前按上一轮真实 usage（缺失时用装配估算）
    /// 判定是否主动压缩；实际压缩后基于压缩后的会话重建请求。非 session
    /// 压缩失败只发射诊断并跳过压缩，返回原始请求。
    fn prepare_request(
        &mut self,
        spec: &TurnRequestSpec,
        previous_context_tokens: Option<u64>,
        outcome: &mut AgentOutcome,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let (mut request, assembled_estimate) = self.build_request(spec)?;
        let budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        let compaction_tokens = previous_context_tokens.unwrap_or(assembled_estimate);
        if self.compaction.should_compact(compaction_tokens, &budget) {
            match self.compaction.compact_with_reason(
                &mut self.session,
                &budget,
                compaction_tokens,
                CompactionReason::Threshold,
                cancellation,
            ) {
                Ok(result) => {
                    record_compaction(outcome, &result);
                    if matches!(result, CompactionOutcome::Compacted { .. }) {
                        request = self.rebuild_request(spec)?;
                    }
                }
                Err(crate::compaction::CompactionError::Session(error)) => {
                    return Err(AgentError::Session(error));
                }
                Err(_error) => {
                    outcome.usage_complete = false;
                    emit_diagnostic(
                        events,
                        AgentDiagnostic::warning(
                            "compaction_skipped",
                            "automatic context compaction skipped".to_string(),
                        ),
                    );
                }
            }
        }
        Ok(request)
    }

    /// 单个轮步：先经 `prepare_request` 装配请求（含发送前主动压缩），再交给
    /// 采样层发送。provider 明确返回 ContextOverflow 时强制压缩并基于压缩后的
    /// 会话重建请求，恰好一次重发。
    fn run_turn(
        &mut self,
        spec: &TurnRequestSpec,
        previous_context_tokens: Option<u64>,
        outcome: &mut AgentOutcome,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        let mut request = match self.prepare_request(
            spec,
            previous_context_tokens,
            outcome,
            events,
            cancellation,
        ) {
            Ok(request) => request,
            Err(error) => return AttemptOutcome::Failed(error),
        };
        let mut overflow_retried = false;
        loop {
            match self.sample_request(&request, events, cancellation, model_turn_ordinal) {
                AttemptOutcome::Response(response) => return AttemptOutcome::Response(response),
                AttemptOutcome::Aborted => return AttemptOutcome::Aborted,
                AttemptOutcome::Failed(error) => {
                    if matches!(
                        &error,
                        AgentError::Provider(provider)
                            if is_context_overflow_error(&provider.error)
                    ) {
                        outcome.usage_complete = false;
                        if overflow_retried {
                            return AttemptOutcome::Failed(error);
                        }
                        overflow_retried = true;
                        let forced = match self.force_compact(cancellation, events) {
                            Ok(result) => result,
                            Err(_) => return AttemptOutcome::Failed(error),
                        };
                        record_compaction(outcome, &forced);
                        // 强制压缩只修改了 self.session；重试必须基于压缩后的
                        // 会话重新装配请求，否则仍携带被拒绝的超限上下文。
                        match self.rebuild_request(spec) {
                            Ok(rebuilt) => request = rebuilt,
                            Err(rebuild_error) => return AttemptOutcome::Failed(rebuild_error),
                        }
                        continue;
                    }
                    return AttemptOutcome::Failed(error);
                }
            }
        }
    }

    /// 采样层：对一次纯发送做 agent 层重试包装。可重试 provider 错误指数退避
    /// 重试，重试预算按次独立；ContextOverflow 原样上抛交给轮步层处理；退避
    /// 等待被取消时返回 Aborted。
    fn sample_request(
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        let mut retry_attempt = 0u32;
        loop {
            match self.attempt_request(request, events, cancellation, model_turn_ordinal) {
                Ok(response) => return AttemptOutcome::Response(Box::new(response)),
                Err(AgentError::Provider(error)) if is_context_overflow_error(&error.error) => {
                    return AttemptOutcome::Failed(AgentError::Provider(error));
                }
                Err(error) => {
                    if let AgentError::Provider(provider) = &error
                        && retry_attempt < self.config.retry.max_retries
                        && is_retryable_provider_error(provider)
                    {
                        retry_attempt += 1;
                        let delay_ms = retry_delay_ms(
                            self.config.retry.base_delay_ms,
                            retry_attempt,
                            provider.retry_after,
                        );
                        emit_diagnostic(
                            events,
                            AgentDiagnostic::info(
                                "provider_retry_scheduled",
                                format!(
                                    "provider retry {retry_attempt}/{max} in {delay_ms}ms: {}",
                                    provider.error.message,
                                    max = self.config.retry.max_retries,
                                ),
                            ),
                        );
                        if !sleep_abortable(delay_ms, cancellation) {
                            return AttemptOutcome::Aborted;
                        }
                        continue;
                    }
                    return AttemptOutcome::Failed(error);
                }
            }
        }
    }

    /// 发送层：纯发送，仅调用流式 completion（协议不支持流式时回退 complete）；
    /// 不感知压缩、重试与 ContextOverflow。
    fn attempt_request(
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> Result<ModelTurnResponse> {
        self.stream_completion(request, events, cancellation, model_turn_ordinal)
    }

    /// 流式调用；协议不支持流式（`provider_streaming_unsupported`）时回退 `complete`。
    fn stream_completion(
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> Result<ModelTurnResponse> {
        // provider 回调与 on_attempt 共享同一个事件出口；用本地 RefCell 承接
        // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
        // 而为，provider 结果不因投影失败丢弃。
        let events_cell = std::cell::RefCell::new(events);
        let events_ref = &events_cell;
        let mut on_stream = |event: ProviderStreamEvent| {
            let ProviderStreamEvent::OutputTextDelta { delta } = event;
            let mut events = events_ref.borrow_mut();
            emit(&mut events, AgentEvent::MessageUpdate { delta });
        };
        let on_attempt = |event: ProviderAttemptEvent| {
            let mut events = events_ref.borrow_mut();
            emit(
                &mut events,
                AgentEvent::ProviderAttempt {
                    model_turn_ordinal,
                    event,
                },
            );
        };
        let mut observed_attempt = |event: ProviderAttemptEvent| {
            on_attempt(event);
        };
        match self.provider.complete_stream_observed(
            request,
            cancellation,
            &mut on_stream,
            &mut observed_attempt,
        ) {
            Ok(response) => Ok(response),
            Err(error)
                if error.error.code.as_deref() == Some(PROVIDER_STREAMING_UNSUPPORTED_CODE) =>
            {
                self.provider
                    .complete_observed(request, cancellation, &mut observed_attempt)
                    .map_err(AgentError::Provider)
            }
            Err(error) => {
                // 保留传输层给出的类型、重放安全性与 Retry-After，交由调用处
                // 的唯一重试策略裁决。
                Err(AgentError::Provider(error))
            }
        }
    }

    /// 追加一条会话消息；失败时按「已积累 progress 则包装为 RunFailed」收敛并
    /// 返回错误。session 错误不可能触发 abort，故 `fail_after_progress` 必然
    /// 返回 Err。
    fn append_session_or_fail(
        &mut self,
        outcome: &mut AgentOutcome,
        message: AgentMessage,
    ) -> Result<()> {
        if let Err(error) = self.session.append_message(message) {
            return Err(self
                .fail_after_progress(AgentError::Session(error), outcome.clone())
                .expect_err("session errors cannot abort"));
        }
        Ok(())
    }

    /// 取消/中止的收敛出口：标记中止原因并关闭 inbox（消费者因此退出），
    /// 返回带中止语义的 outcome。循环内所有取消分支共用此出口。
    fn abort_outcome(&self, mut outcome: AgentOutcome) -> Result<AgentOutcome> {
        outcome.terminal_reason = AgentTerminalReason::Aborted;
        outcome.usage_complete = false;
        lock_inbox(&self.inbox).close();
        Ok(outcome)
    }

    fn fail_after_progress(
        &self,
        error: AgentError,
        outcome: AgentOutcome,
    ) -> Result<AgentOutcome> {
        if is_cancelled_agent_error(&error) {
            return self.abort_outcome(outcome);
        }
        let mut outcome = outcome;
        outcome.terminal_reason = AgentTerminalReason::Failed;
        outcome.usage_complete = false;
        lock_inbox(&self.inbox).close();
        if outcome.turns == 0 {
            Err(error)
        } else {
            Err(AgentError::RunFailed {
                error: Box::new(error),
                outcome: Box::new(outcome),
            })
        }
    }
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
