//! Singularity 核心 Agent 执行循环。
//!
//! 采用双层状态机循环结构：
//! - **内层循环**：处理单轮任务执行中的模型流式请求、工具批次按模型给定顺序串行执行、中间引导（Steer）注入与上下文压缩；
//! - **外层循环**：当模型完成当前阶段工作（返回纯文本且无工具调用）准备收尾时，消费跟进（FollowUp）队列继续执行下一阶段目标。
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
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptEvent, ProviderAttemptMetadata,
    ProviderError, ProviderProtocolContract, ProviderReasoningReplay, ProviderStreamEvent,
    ProviderToolReasoningMode, ToolChoiceMode, ToolChoicePolicy, is_strict_tool_schema_compatible,
    split_model_selector,
};
use thiserror::Error;
use uuid::Uuid;

use crate::compaction::{
    CompactionBudget, CompactionConfig, CompactionEngine, CompactionOutcome, CompactionReason,
};
use crate::message::{
    AgentMessageRole, ContentBlock, assistant_response_message, tool_result_message, user_message,
};
use crate::session::context::entry_to_llm_messages;
use crate::session::{SessionEntry, SessionEntryType, SessionError, SessionManager};
use crate::tools::{
    ExecuteContext, PreparedTool, ToolError, ToolExecution, ToolPreflight, ToolRegistry,
};

/// Typed severity for non-fatal runtime diagnostics emitted by the AgentLoop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// A safe, non-persistent diagnostic.  The code is stable for projections;
/// message text is intentionally kept free of raw provider payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDiagnostic {
    pub severity: AgentDiagnosticSeverity,
    pub code: String,
    pub message: String,
}

impl AgentDiagnostic {
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
/// tool 事件按执行完成顺序投递；持久化的 toolResult 顺序不受影响。
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
    /// 投影失败即中止本轮，并丢弃当轮 provider 结果。
    ProviderAttempt {
        model_turn_ordinal: u32,
        event: ProviderAttemptEvent,
    },
}

/// Agent 运行生命周期事件出口。
///
/// 单一回调统一承载全部事件。诊断事件为尽力而为：投递失败被忽略，
/// 不改变轮次结果；其余事件的投影失败会使循环立即中止本轮并丢弃
/// 当轮结果，错误经 `run` 返回。
pub struct AgentEvents<'a> {
    pub on_event: Option<&'a mut dyn FnMut(AgentEvent) -> Result<()>>,
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system_prompt: String::new(),
            context_window: 128_000,
            max_output_tokens: 4_096,
            compaction: CompactionConfig::default(),
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
        config.compaction.validate(provider_max_output_tokens)?;
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
    /// A provider/session failure after the run has already accumulated
    /// durable facts.  The inner error remains the authoritative cause while
    /// `outcome` carries the lower-bound turns/usage observed before it.
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
    pub turns: u32,
    /// 各轮 provider 调用的聚合 usage。
    pub usage: ModelUsage,
    pub compacted: bool,
    /// `true` 表示每个已发出的 provider 请求都带有可确认的 usage；
    /// 取消/失败时未知的末次请求保持 `false`，不得估算成精确值。
    pub usage_complete: bool,
    pub terminal_reason: AgentTerminalReason,
    /// Aggregated terminal provider-attempt telemetry, when the provider
    /// exposed it. Detailed occurrences are runtime-only and never persisted
    /// in Session JSONL.
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
}

/// turn 输入的类别；两个类别共享同一个原子 inbox。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnInputKind {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnInput {
    kind: TurnInputKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TurnInboxState {
    #[default]
    Open,
    Closed,
}

/// 活动 turn 的单一输入箱。
///
/// `enqueue_*`、`drain_steer` 与 `take_at_stop` 都在调用方持有的同一把
/// Mutex 内运行。自然终止点调用 `take_at_stop` 时，箱内已有输入会被取出
/// 并继续执行；只有箱为空时才原子地转为 Closed，之后的输入明确拒绝。
/// 这保证不存在“已接受但丢失”的中间状态，也不引入持久队列或 grace period。
#[derive(Debug, Default)]
pub struct TurnInbox {
    state: TurnInboxState,
    entries: VecDeque<TurnInput>,
}

impl TurnInbox {
    pub fn enqueue(&mut self, kind: TurnInputKind, text: impl Into<String>) -> bool {
        if self.state == TurnInboxState::Closed {
            return false;
        }
        self.entries.push_back(TurnInput {
            kind,
            text: text.into(),
        });
        true
    }

    pub fn enqueue_steer(&mut self, text: impl Into<String>) -> bool {
        self.enqueue(TurnInputKind::Steer, text)
    }

    pub fn enqueue_follow_up(&mut self, text: impl Into<String>) -> bool {
        self.enqueue(TurnInputKind::FollowUp, text)
    }

    fn drain_steer(&mut self) -> Vec<String> {
        let mut steer = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(input) = self.entries.pop_front() {
            if input.kind == TurnInputKind::Steer {
                steer.push(input.text);
            } else {
                retained.push_back(input);
            }
        }
        self.entries = retained;
        steer
    }

    /// Atomic natural-stop barrier.  A non-empty box remains open and hands
    /// all accepted inputs to the next loop; an empty box closes permanently.
    fn take_at_stop(&mut self) -> Option<Vec<TurnInput>> {
        if self.entries.is_empty() {
            self.state = TurnInboxState::Closed;
            None
        } else {
            Some(self.entries.drain(..).collect())
        }
    }

    fn close(&mut self) {
        self.state = TurnInboxState::Closed;
    }
}

/// steer/follow-up 共用的线程安全句柄。
pub type TurnInboxHandle = Arc<Mutex<TurnInbox>>;

struct PreparedToolCall {
    call: singularity_model::ModelToolCall,
    prepared: Option<PreparedTool>,
    preflight_execution: Option<ToolExecution>,
}

/// 通过单一事件出口投递一个事件；回调返回错误时向上传播以中止本轮。
fn emit(events: &mut AgentEvents<'_>, event: AgentEvent) -> Result<()> {
    if let Some(callback) = events.on_event.as_deref_mut() {
        callback(event)?;
    }
    Ok(())
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
        if let Some(execution) = &item.preflight_execution {
            emit(
                events,
                AgentEvent::ToolExecutionEnded {
                    tool_name: item.call.tool_name.clone(),
                    tool_call_id: item.call.tool_call_id.clone(),
                    execution: execution.clone(),
                },
            )?;
            results.push(execution.clone());
            continue;
        }
        let prepared = item
            .prepared
            .expect("runnable tool call must have a prepared tool");
        let call = item.call.clone();
        // 与事件回调同一合同：首个投影错误中止本轮，其后的 Update/End 不再
        // 投递，避免向客户端继续写出无效事件。
        let projected_error = std::cell::RefCell::new(None::<AgentError>);
        let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_prepared_tool(registry, prepared, &call, cwd, cancellation, |text| {
                let mut slot = projected_error.borrow_mut();
                if slot.is_none()
                    && let Err(error) = emit(
                        events,
                        AgentEvent::ToolExecutionUpdate {
                            tool_name: call.tool_name.clone(),
                            tool_call_id: call.tool_call_id.clone(),
                            arguments: call.arguments.clone(),
                            partial_result: text.to_string(),
                        },
                    )
                {
                    *slot = Some(error);
                }
            })
        }))
        .unwrap_or_else(|_| tool_error_execution("tool execution panicked"));
        if projected_error.borrow().is_none() {
            emit(
                events,
                AgentEvent::ToolExecutionEnded {
                    tool_name: call.tool_name.clone(),
                    tool_call_id: call.tool_call_id.clone(),
                    execution: execution.clone(),
                },
            )?;
        }
        results.push(execution);
        if let Some(error) = projected_error.into_inner() {
            return Err(error);
        }
    }
    Ok(results)
}

/// 按 provider 声明把系统/开发者指令投影为请求首条消息。
///
/// compaction 与普通请求共用同一 seam：supports developer → Developer；
/// 否则 supports system → System；两者都不支持时投影为 user 前缀。
pub(crate) fn instruction_message(
    capabilities: &ProviderProtocolContract,
    instruction: &str,
) -> Option<ModelMessage> {
    if instruction.is_empty() {
        return None;
    }
    if capabilities.supports_developer_message {
        Some(ModelMessage::text(ModelRole::Developer, instruction))
    } else if capabilities.supports_system_message {
        Some(ModelMessage::text(ModelRole::System, instruction))
    } else {
        Some(ModelMessage::text(ModelRole::User, instruction))
    }
}

/// 有效输出上限 = min(配置值, provider 静态能力声明)；正常请求与
/// compaction 摘要请求共用，避免摘要派生出超过模型上限的 max_tokens。
fn effective_max_output_tokens(provider: &dyn Provider, configured: u64) -> u32 {
    u32::try_from(configured.min(provider.protocol_contract().max_output_tokens as u64))
        .unwrap_or(u32::MAX)
}

/// 新 headless core 的 Agent：会话 + compaction + 工具注册表 + 模型提供方。
pub struct Agent {
    session: SessionManager,
    compaction: CompactionEngine,
    registry: ToolRegistry,
    provider: Arc<dyn Provider + Send + Sync>,
    config: AgentConfig,
    /// steer/follow-up 共用的活动 turn 输入箱；内存态不持久化。
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

    /// 返回 turn 实时输入队列（steer / follow-up）的线程安全句柄。
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
        lock_inbox(&self.inbox).enqueue_steer(text.to_string())
    }

    /// 注入跟进：代理将要停止（无工具调用且文本非空）时继续一轮再停止。
    pub fn follow_up(&mut self, text: &str) -> bool {
        lock_inbox(&self.inbox).enqueue_follow_up(text.to_string())
    }

    /// 运行一个完整 Agent 循环：输入持久化为 user 消息，内层循环处理工具调用与
    /// steer，外层循环消费 follow-up；停止后返回聚合结果。
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
            turns: 0,
            usage: ModelUsage::default(),
            compacted: false,
            usage_complete: true,
            terminal_reason: AgentTerminalReason::Completed,
            provider_attempt_metadata: None,
        };
        // 显式上下文溢出每轮只允许一次强制压缩重试（N3：循环层不再做整轮瞬时重试）。
        let mut context_overflow_retried = false;
        self.session.append_message(user_message(input))?;

        let mut preferences = ModelPreferences::default();
        if !self.config.model.is_empty() {
            preferences.model_name = Some(self.config.model.clone());
        }
        // 静态能力声明决定 system prompt 角色、输出上限与 tool 策略（旧 AgentLoop 同款）。
        let capabilities = self.provider.protocol_contract();
        let max_output_tokens =
            effective_max_output_tokens(self.provider.as_ref(), self.config.max_output_tokens);
        let tools = self.tool_schemas(&capabilities);
        let tool_choice = ToolChoicePolicy {
            mode: ToolChoiceMode::Auto,
            // 单条 assistant 消息允许的工具调用数上限；本地按模型给定顺序
            // 串行执行全部调用，wire 侧 parallel_tool_calls 恒为 false。
            max_tool_calls: DEFAULT_MAX_TOOLS_PER_REQUEST,
            strict_tool_schema: capabilities.supports_strict_tool_schema
                && tools
                    .iter()
                    .all(|tool| is_strict_tool_schema_compatible(&tool.parameters_schema)),
        };

        // 外层循环：代理将要停止时消费 follow-up 队列。
        loop {
            // 内层循环：工具调用与 steer 注入。
            loop {
                if cancellation.is_cancelled() {
                    outcome.terminal_reason = AgentTerminalReason::Aborted;
                    outcome.usage_complete = false;
                    lock_inbox(&self.inbox).close();
                    return Ok(outcome);
                }
                // 注入 steer 队列全部消息（作为 user 消息追加到本轮上下文）。
                let steer_messages = lock_inbox(&self.inbox).drain_steer();
                for text in steer_messages {
                    if let Err(error) = self.session.append_message(user_message(&text)) {
                        return self.fail_after_progress(AgentError::Session(error), outcome);
                    }
                }
                let (request, assembled_estimate, assembled_entries) = match self.build_request(
                    &preferences,
                    &capabilities,
                    &tools,
                    &tool_choice,
                    max_output_tokens,
                    outcome.turns,
                ) {
                    Ok(request) => request,
                    Err(error) => return self.fail_after_progress(error, outcome),
                };
                let model_turn_ordinal = outcome.turns.saturating_add(1);
                let response = match self.stream_completion(
                    &request,
                    events,
                    cancellation,
                    model_turn_ordinal,
                ) {
                    Ok(response) => response,
                    Err(AgentError::Provider(error)) if is_context_overflow_error(&error.error) => {
                        record_provider_attempts(&mut outcome, &error, model_turn_ordinal);
                        // The rejected request has no complete usage record;
                        // later successful turns cannot make this aggregate
                        // exact again.
                        outcome.usage_complete = false;
                        let original_error = AgentError::Provider(error);
                        if context_overflow_retried {
                            return self.fail_after_progress(original_error, outcome);
                        }
                        context_overflow_retried = true;
                        // 强制压缩失败时向上传播原始上下文溢出错误，保留真实失败根因。
                        let forced = match self.force_compact(cancellation, events) {
                            Ok(result) => result,
                            Err(_) => {
                                return self.fail_after_progress(original_error, outcome);
                            }
                        };
                        record_compaction(&mut outcome, &forced);
                        continue;
                    }
                    Err(error) => {
                        record_error_attempts(&mut outcome, &error, model_turn_ordinal);
                        return self.fail_after_progress(error, outcome);
                    }
                };
                record_response_attempts(
                    &mut outcome,
                    response.provider_attempt_metadata.as_ref(),
                    model_turn_ordinal,
                );
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
                context_overflow_retried = false;
                record_usage(&mut outcome, &response.usage);
                let assistant_text = response
                    .assistant_message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let tool_calls = response.tool_calls.clone();
                let length_truncated = response.is_length_truncated();
                if length_truncated && !tool_calls.is_empty() {
                    // A length-truncated response may contain only partially
                    // parsed tool calls. Persist the assistant turn and a
                    // model-visible failure for each call, but never execute
                    // those calls or surface them as successful tool events.
                    if let Err(error) = self
                        .session
                        .append_message(assistant_response_message(&response))
                    {
                        return self.fail_after_progress(AgentError::Session(error), outcome);
                    }
                    for call in &tool_calls {
                        if let Err(error) = self.session.append_message(tool_result_message(
                            &call.tool_call_id,
                            &call.tool_name,
                            &tool_error_execution(
                                "model output was truncated before the tool call completed",
                            ),
                        )) {
                            return self.fail_after_progress(AgentError::Session(error), outcome);
                        }
                    }
                    outcome.final_text = assistant_text;
                    if let Err(error) = self.maybe_compact(
                        &mut outcome,
                        Some(&response.usage),
                        assembled_estimate,
                        assembled_entries,
                        cancellation,
                        events,
                    ) {
                        return self.fail_after_progress(error, outcome);
                    }
                    continue;
                }
                if !tool_calls.is_empty() {
                    // 单次模型响应对应一条 Assistant 消息（包含思考、文本与全部 tool_call 块）。
                    if let Err(error) = self
                        .session
                        .append_message(assistant_response_message(&response))
                    {
                        return self.fail_after_progress(AgentError::Session(error), outcome);
                    }
                    // 查找、参数校验和执行模式判定先按 source order 完成；
                    // 未知工具/非法参数只生成模型可见失败，不进入并行线程。
                    let prepared_calls = tool_calls
                        .iter()
                        .map(|call| {
                            let (prepared, preflight_execution) = match self
                                .registry
                                .preflight(&call.tool_name, &call.arguments)
                            {
                                Ok(ToolPreflight::Ready(prepared)) => (Some(prepared), None),
                                Ok(ToolPreflight::Rejected(execution)) => (None, Some(execution)),
                                Err(error) => (None, Some(tool_error_execution(error))),
                            };
                            PreparedToolCall {
                                call: call.clone(),
                                prepared,
                                preflight_execution,
                            }
                        })
                        .collect::<Vec<_>>();
                    for item in &prepared_calls {
                        if let Err(error) = emit(
                            events,
                            AgentEvent::ToolExecutionStarted {
                                tool_name: item.call.tool_name.clone(),
                                tool_call_id: item.call.tool_call_id.clone(),
                                arguments: item.call.arguments.clone(),
                            },
                        ) {
                            return self.fail_after_progress(error, outcome);
                        }
                    }
                    let executions = execute_tool_batch(
                        &self.registry,
                        &prepared_calls,
                        self.session.cwd(),
                        cancellation,
                        events,
                    )?;
                    // Durable toolResult entries are always appended in assistant source order,
                    // regardless of completion/event order.
                    for (call, execution) in tool_calls.iter().zip(executions.iter()) {
                        if let Err(error) = self.session.append_message(tool_result_message(
                            &call.tool_call_id,
                            &call.tool_name,
                            execution,
                        )) {
                            return self.fail_after_progress(AgentError::Session(error), outcome);
                        }
                    }
                    if cancellation.is_cancelled() {
                        outcome.terminal_reason = AgentTerminalReason::Aborted;
                        outcome.usage_complete = false;
                        lock_inbox(&self.inbox).close();
                        return Ok(outcome);
                    }
                    if let Err(error) = self.maybe_compact(
                        &mut outcome,
                        Some(&response.usage),
                        assembled_estimate,
                        assembled_entries,
                        cancellation,
                        events,
                    ) {
                        return self.fail_after_progress(error, outcome);
                    }
                    continue;
                }
                // 无工具调用：终态 assistant 消息持久化并退出内层循环。
                if let Err(error) = self
                    .session
                    .append_message(assistant_response_message(&response))
                {
                    return self.fail_after_progress(AgentError::Session(error), outcome);
                }
                outcome.final_text = assistant_text;
                if let Err(error) = self.maybe_compact(
                    &mut outcome,
                    Some(&response.usage),
                    assembled_estimate,
                    assembled_entries,
                    cancellation,
                    events,
                ) {
                    return self.fail_after_progress(error, outcome);
                }
                break;
            }
            // 代理将要停止：消费 follow-up 队列后回到内层循环。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            for input in pending_inputs {
                if let Err(error) = self.session.append_message(user_message(&input.text)) {
                    return self.fail_after_progress(AgentError::Session(error), outcome);
                }
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
        // Forced overflow recovery is an explicit mode: do not preserve the
        // normal recent-content ratio when the provider has already rejected
        // the request for exceeding its context window.
        budget.retain_ratio = 0.0;
        let tokens_before = self.estimate_context_tokens(None, 0, 0)?;
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

    /// 在本轮已装配的请求成品上做 Token 估算（压缩判定兜底基线）。
    ///
    /// 覆盖最终 wire 请求：除 content 外，provider 还会重放每条 tool 消息的
    /// tool_call_id 与 assistant tool_calls 的 id/name/raw_arguments。
    fn estimate_assembled(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelToolSchema],
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
        message_tokens
            .saturating_add(tool_tokens)
            .saturating_add(max_output_tokens as u64)
            .saturating_add(32)
    }

    /// 组装单轮 provider 请求：system prompt（按能力选择 developer/system 角色，
    /// 均不支持时以 user 前缀注入）+ 会话历史（compaction 感知）。
    ///
    /// 上下文条目只装配一次：messages、reasoning replay 与预算估算全部在
    /// 同一份装配成品上完成；返回 (请求, 装配成品估算, 装配时的条目数)。
    fn build_request(
        &self,
        preferences: &ModelPreferences,
        capabilities: &ProviderProtocolContract,
        tools: &[ModelToolSchema],
        tool_choice: &ToolChoicePolicy,
        max_output_tokens: u32,
        turn: u32,
    ) -> Result<(ModelTurnRequest, u64, usize)> {
        let entries = self.session.build_context_entries()?;
        let context_messages = entries
            .iter()
            .flat_map(entry_to_llm_messages)
            .collect::<Vec<_>>();
        let replays = self.reasoning_replays_from_entries(&entries);
        let mut messages = Vec::with_capacity(context_messages.len() + 1);
        if let Some(instruction) = instruction_message(capabilities, &self.config.system_prompt) {
            messages.push(instruction);
        }
        messages.extend(context_messages);
        let assembled_estimate = self.estimate_assembled(&messages, tools, max_output_tokens);
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), turn),
            messages,
        );
        request.tools = tools.to_vec();
        request.tool_choice = tool_choice.clone();
        request.provider_reasoning_history = replays;
        request.model_preferences = ModelPreferences {
            model_name: preferences.model_name.clone(),
            max_output_tokens: Some(max_output_tokens),
            ..ModelPreferences::default()
        };
        Ok((request, assembled_estimate, entries.len()))
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
                // A provider/model switch invalidates the opaque continuation. Keep
                // visible thinking/messages/tool results in Session, but never send a
                // private replay across an incompatible provider boundary.
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

    /// 流式调用；协议不支持流式（`provider_streaming_unsupported`）时回退 `complete`。
    fn stream_completion(
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> Result<ModelTurnResponse> {
        // provider 回调与 on_attempt 共享同一个事件出口与投影错误位；用本地
        // RefCell 承接两个异签名回调的可变借用（单线程 turn 内串行使用）。
        // 全部本地状态装进一个块：块结束时回调与其 RefCell 借用一并释放，
        // 随后消费投影错误位。
        let (result, projected_error) = {
            let events_cell = std::cell::RefCell::new(events);
            let events_ref = &events_cell;
            let projected_error = std::cell::RefCell::new(None::<AgentError>);
            let projected_ref = &projected_error;
            let mut on_stream = |event: ProviderStreamEvent| {
                let ProviderStreamEvent::OutputTextDelta { delta } = event;
                if projected_ref.borrow().is_none() {
                    let mut events = events_ref.borrow_mut();
                    if let Err(error) = emit(&mut events, AgentEvent::MessageUpdate { delta }) {
                        *projected_ref.borrow_mut() = Some(error);
                    }
                }
            };
            let on_attempt = |event: ProviderAttemptEvent| {
                if projected_ref.borrow().is_none() {
                    let mut events = events_ref.borrow_mut();
                    if let Err(error) = emit(
                        &mut events,
                        AgentEvent::ProviderAttempt {
                            model_turn_ordinal,
                            event,
                        },
                    ) {
                        *projected_ref.borrow_mut() = Some(error);
                    }
                }
            };
            let mut observed_attempt = |event: ProviderAttemptEvent| {
                on_attempt(event);
                true
            };
            let result = match self.provider.complete_stream_observed(
                request,
                cancellation,
                &mut on_stream,
                &mut observed_attempt,
            ) {
                Ok(response) => Ok(bind_response_attempt_ordinal(response, model_turn_ordinal)),
                Err(error)
                    if error.error.code.as_deref() == Some(PROVIDER_STREAMING_UNSUPPORTED_CODE) =>
                {
                    self.provider
                        .complete_observed(request, cancellation, &mut observed_attempt)
                        .map(|response| bind_response_attempt_ordinal(response, model_turn_ordinal))
                        .map_err(AgentError::Provider)
                }
                Err(error) => {
                    // 传输层重试耗尽后向上传播错误，避免循环层进行无意义的整轮盲目重试。
                    Err(AgentError::Provider(error))
                }
            };
            (result, projected_error.into_inner())
        };
        match projected_error {
            Some(error) => Err(error),
            None => result,
        }
    }

    /// 每轮模型调用后评估是否需要触发上下文压缩。
    ///
    /// 触发条件满足时调用 CompactionEngine 生成历史结构化摘要并追加 CompactionEntry 节点；
    /// 后续请求将自动以该压缩节点作为上下文构建基线。
    /// 若摘要模型调用遭遇瞬时错误，降级记录警告并继续会话，避免中断已完成的执行。
    fn maybe_compact(
        &mut self,
        outcome: &mut AgentOutcome,
        last_usage: Option<&ModelUsage>,
        assembled_estimate: u64,
        assembled_entries: usize,
        cancellation: &CancellationToken,
        events: &mut AgentEvents,
    ) -> Result<()> {
        let budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        let context_tokens =
            self.estimate_context_tokens(last_usage, assembled_estimate, assembled_entries)?;
        if !self.compaction.should_compact(context_tokens, &budget) {
            return Ok(());
        }
        match self
            .compaction
            .compact(&mut self.session, &budget, context_tokens, cancellation)
        {
            Ok(result) => record_compaction(outcome, &result),
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
        Ok(())
    }

    /// 估算压缩判定用的上下文 Token 总量。
    ///
    /// 以 Provider 对上一请求的实测 usage 为基线，叠加装配后新增的尾部条目
    /// 估算；usage 缺失时以本轮装配成品的估算兜底（首轮/无用量场景）。尾部
    /// 从线性条目末尾反向累计至最近一条 assistant 消息为止（其内容已计入
    /// provider 输出侧 usage）。估算直接在装配成品与条目上完成，不做第二次
    /// 上下文装配。
    fn estimate_context_tokens(
        &self,
        last_usage: Option<&ModelUsage>,
        assembled_estimate: u64,
        assembled_entries: usize,
    ) -> Result<u64> {
        let entries = self.session.entries();
        let trailing = entries
            .iter()
            .rev()
            .take(entries.len().saturating_sub(assembled_entries))
            .take_while(|entry| {
                !matches!(
                    &entry.entry_type,
                    SessionEntryType::Message(message)
                        if message.role == AgentMessageRole::Assistant
                )
            })
            .flat_map(entry_to_llm_messages)
            .map(|message| self.compaction.estimate_tokens(&message.content))
            .sum::<u64>();
        match last_usage.filter(|usage| usage.total_tokens > 0) {
            Some(usage) => Ok(usage.total_tokens.saturating_add(trailing)),
            None => Ok(assembled_estimate.saturating_add(trailing)),
        }
    }

    fn fail_after_progress(
        &self,
        error: AgentError,
        mut outcome: AgentOutcome,
    ) -> Result<AgentOutcome> {
        if is_cancelled_agent_error(&error) {
            outcome.terminal_reason = AgentTerminalReason::Aborted;
            outcome.usage_complete = false;
            lock_inbox(&self.inbox).close();
            return Ok(outcome);
        }
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

/// 加锁活动 turn inbox；中毒时恢复，避免一次工具 panic 使输入通道永久不可用。
fn lock_inbox(queue: &Mutex<TurnInbox>) -> std::sync::MutexGuard<'_, TurnInbox> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    aggregate_usage(&mut outcome.usage, response);
    if !response.usage_present {
        outcome.usage_complete = false;
    }
}

fn aggregate_usage(aggregate: &mut ModelUsage, response: &ModelUsage) {
    aggregate.input_tokens = aggregate.input_tokens.saturating_add(response.input_tokens);
    aggregate.output_tokens = aggregate
        .output_tokens
        .saturating_add(response.output_tokens);
    aggregate.total_tokens = aggregate.total_tokens.saturating_add(response.total_tokens);
    aggregate.cached_input_tokens = aggregate
        .cached_input_tokens
        .saturating_add(response.cached_input_tokens);
    aggregate.reasoning_tokens = aggregate
        .reasoning_tokens
        .saturating_add(response.reasoning_tokens);
    aggregate.usage_present |= response.usage_present;
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
    aggregate_usage(&mut outcome.usage, usage);
    outcome.usage_complete &= *usage_complete;
}

fn record_response_attempts(
    outcome: &mut AgentOutcome,
    metadata: Option<&ProviderAttemptMetadata>,
    model_turn_ordinal: u32,
) {
    let Some(metadata) = metadata else {
        return;
    };
    let aggregate = outcome
        .provider_attempt_metadata
        .get_or_insert_with(ProviderAttemptMetadata::default);
    aggregate.attempt_count = aggregate
        .attempt_count
        .saturating_add(metadata.attempt_count);
    aggregate.retry_count = aggregate.retry_count.saturating_add(metadata.retry_count);
    aggregate.latency_ms = aggregate.latency_ms.saturating_add(metadata.latency_ms);
    for mut occurrence in metadata.occurrences.clone() {
        occurrence.model_turn_ordinal = Some(model_turn_ordinal);
        aggregate.occurrences.push(occurrence);
    }
}

fn record_error_attempts(outcome: &mut AgentOutcome, error: &AgentError, model_turn_ordinal: u32) {
    let AgentError::Provider(provider) = error else {
        return;
    };
    record_provider_attempts(outcome, provider, model_turn_ordinal);
}

fn record_provider_attempts(
    outcome: &mut AgentOutcome,
    provider: &ProviderError,
    model_turn_ordinal: u32,
) {
    record_response_attempts(
        outcome,
        provider.provider_attempt_metadata.as_ref(),
        model_turn_ordinal,
    );
}

fn bind_response_attempt_ordinal(
    mut response: ModelTurnResponse,
    model_turn_ordinal: u32,
) -> ModelTurnResponse {
    if let Some(metadata) = response.provider_attempt_metadata.as_mut() {
        for occurrence in &mut metadata.occurrences {
            occurrence.model_turn_ordinal = Some(model_turn_ordinal);
        }
    }
    response
}

fn emit_diagnostic(events: &mut AgentEvents, diagnostic: AgentDiagnostic) {
    // 诊断是尽力而为的观测侧信道：回调失败不中止本轮。
    let _ = emit(events, AgentEvent::Diagnostic(diagnostic));
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
