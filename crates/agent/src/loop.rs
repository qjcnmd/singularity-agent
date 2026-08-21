//! Singularity 核心 Agent 执行循环。
//!
//! 采用双层状态机循环结构：
//! - **内层循环**：处理单轮任务执行中的模型流式请求、工具批次安全并发执行、中间引导（Steer）注入与上下文压缩；
//! - **外层循环**：当模型完成当前阶段工作（返回纯文本且无工具调用）准备收尾时，消费跟进（FollowUp）队列继续执行下一阶段目标。
//!
//! 会话状态持久化、上下文压缩、工具注册分发与模型调用分别由
//! `session/` facade、`compaction.rs`、`tools/` 与 `singularity_model` 模块提供支持。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;
use singularity_core::CancellationToken;
use singularity_model::{
    ModelError, ModelErrorKind, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptEvent, ProviderAttemptMetadata,
    ProviderError, ProviderProtocolContract, ProviderReasoningReplay, ProviderStreamEvent,
    ProviderToolReasoningMode, ToolChoiceMode, ToolChoicePolicy, is_strict_tool_schema_compatible,
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

/// 工具开始执行时的回调签名：接收工具名称、调用 ID 与结构化参数。
pub type ToolExecutionCallback<'a> = &'a mut dyn FnMut(&str, &str, &Value);
/// 工具执行输出增量更新时的回调签名：接收工具名称、调用 ID、结构化参数与部分输出文本。
pub type ToolExecutionUpdateCallback<'a> = &'a mut dyn FnMut(&str, &str, &Value, &str);
/// 工具执行结束时的回调签名：接收工具名称、调用 ID 与最终执行结果。
pub type ToolExecutionEndCallback<'a> = &'a mut dyn FnMut(&str, &str, &ToolExecution);

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

/// Provider-attempt observer.  The callback is deliberately non-vetoing: its
/// return value is ignored by the AgentLoop and cannot turn a provider success
/// into a transport failure.
pub type ProviderAttemptCallback<'a> = &'a mut dyn FnMut(ProviderAttemptEvent);
pub type AgentDiagnosticCallback<'a> = &'a mut dyn FnMut(&AgentDiagnostic);

/// Agent 运行生命周期事件监听回调集合。
pub struct AgentEvents<'a> {
    /// 模型流式文本输出增量更新。
    pub on_message_update: Option<&'a mut dyn FnMut(&str)>,
    /// 工具开始执行事件。
    pub on_tool_execution_start: Option<ToolExecutionCallback<'a>>,
    /// 工具执行中产生的流式增量输出事件。
    pub on_tool_execution_update: Option<ToolExecutionUpdateCallback<'a>>,
    /// 工具执行完成事件。
    pub on_tool_execution_end: Option<ToolExecutionEndCallback<'a>>,
    /// 非致命、脱敏 Agent 诊断；不会写入 Session JSONL。
    pub on_diagnostic: Option<AgentDiagnosticCallback<'a>>,
    /// provider HTTP attempt 生命周期观测；回调不能否决 provider 结果。
    pub on_provider_attempt: Option<ProviderAttemptCallback<'a>>,
}

impl<'a> AgentEvents<'a> {
    pub fn new() -> Self {
        Self {
            on_message_update: None,
            on_tool_execution_start: None,
            on_tool_execution_update: None,
            on_tool_execution_end: None,
            on_diagnostic: None,
            on_provider_attempt: None,
        }
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

enum ToolRuntimeEvent {
    Update {
        index: usize,
        text: String,
    },
    End {
        index: usize,
        execution: ToolExecution,
    },
}

fn emit_tool_start(events: &mut AgentEvents<'_>, call: &singularity_model::ModelToolCall) {
    if let Some(callback) = events.on_tool_execution_start.as_deref_mut() {
        callback(&call.tool_name, &call.tool_call_id, &call.arguments);
    }
}

fn emit_tool_update(
    events: &mut AgentEvents<'_>,
    call: &singularity_model::ModelToolCall,
    text: &str,
) {
    if let Some(callback) = events.on_tool_execution_update.as_deref_mut() {
        callback(&call.tool_name, &call.tool_call_id, &call.arguments, text);
    }
}

fn emit_tool_end(
    events: &mut AgentEvents<'_>,
    call: &singularity_model::ModelToolCall,
    execution: &ToolExecution,
) {
    if let Some(callback) = events.on_tool_execution_end.as_deref_mut() {
        callback(&call.tool_name, &call.tool_call_id, execution);
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

fn execute_tool_batch_parallel(
    registry: &ToolRegistry,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    max_parallel_tool_calls: u32,
    events: &mut AgentEvents<'_>,
) -> Vec<ToolExecution> {
    let mut results = vec![None; calls.len()];
    let mut runnable_indices = Vec::with_capacity(calls.len());

    // Preflight rejections are already complete and do not enter a worker.
    for (index, item) in calls.iter().enumerate() {
        if let Some(execution) = &item.preflight_execution {
            emit_tool_end(events, &item.call, execution);
            results[index] = Some(execution.clone());
        } else {
            runnable_indices.push(index);
        }
    }

    // 批内含任一 sequential 工具（supports_parallel=false）时，整批按模型原始
    // 顺序串行执行，不创建 worker 线程。
    let force_serial = runnable_indices.iter().any(|&index| {
        !calls[index]
            .prepared
            .expect("runnable tool call must have a prepared tool")
            .supports_parallel
    });
    if force_serial {
        for &index in &runnable_indices {
            let item = &calls[index];
            let prepared = item
                .prepared
                .expect("runnable tool call must have a prepared tool");
            let call = &item.call;
            let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execute_prepared_tool(registry, prepared, call, cwd, cancellation, |text| {
                    emit_tool_update(events, call, text);
                })
            }))
            .unwrap_or_else(|_| tool_error_execution("tool worker panicked"));
            emit_tool_end(events, call, &execution);
            results[index] = Some(execution);
        }
    } else {
        let worker_limit = usize::try_from(max_parallel_tool_calls.max(1)).unwrap_or(usize::MAX);
        // 只为当前窗口创建 worker；窗口之间顺序推进，避免模型一次返回大量
        // tool call 时不受控地创建线程。preflight 项不在 runnable_indices 中，
        // 因此不会占用并发名额。
        for window in runnable_indices.chunks(worker_limit) {
            let (sender, receiver): (Sender<ToolRuntimeEvent>, Receiver<ToolRuntimeEvent>) =
                mpsc::channel();
            thread::scope(|scope| {
                let mut handles = Vec::with_capacity(window.len());
                for &index in window {
                    let prepared = calls[index]
                        .prepared
                        .expect("runnable tool call must have a prepared tool");
                    let sender = sender.clone();
                    let call = calls[index].call.clone();
                    handles.push(scope.spawn(move || {
                        let execution =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                execute_prepared_tool(
                                    registry,
                                    prepared,
                                    &call,
                                    cwd,
                                    cancellation,
                                    |text| {
                                        let _ = sender.send(ToolRuntimeEvent::Update {
                                            index,
                                            text: text.to_string(),
                                        });
                                    },
                                )
                            }))
                            .unwrap_or_else(|_| tool_error_execution("tool worker panicked"));
                        let _ = sender.send(ToolRuntimeEvent::End { index, execution });
                    }));
                }
                drop(sender);

                let mut finished = 0usize;
                while finished < window.len() {
                    match receiver.recv() {
                        Ok(ToolRuntimeEvent::Update { index, text }) => {
                            emit_tool_update(events, &calls[index].call, &text);
                        }
                        Ok(ToolRuntimeEvent::End { index, execution }) => {
                            emit_tool_end(events, &calls[index].call, &execution);
                            results[index] = Some(execution);
                            finished += 1;
                        }
                        Err(_) => break,
                    }
                }
                for handle in handles {
                    let _ = handle.join();
                }
            });
        }
    }

    // A worker must always send End, but preserve a fail-closed result if a
    // thread or channel failure violates that invariant.
    for (index, result) in results.iter_mut().enumerate() {
        if result.is_none() {
            let execution = tool_error_execution("tool worker did not produce a result");
            emit_tool_end(events, &calls[index].call, &execution);
            *result = Some(execution);
        }
    }
    results
        .into_iter()
        .map(|result| result.expect("tool batch result must be present"))
        .collect()
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
        let mut config = config;
        // The documented default is 8192, but a provider with a smaller
        // declared output limit may safely clamp that implicit default. An
        // explicit non-default cap remains fail-closed below.
        if config.compaction == CompactionConfig::default()
            && provider_max_output_tokens < config.compaction.summary_max_tokens
        {
            config.compaction.summary_max_tokens = provider_max_output_tokens;
        }
        config
            .compaction
            .validate(provider_max_output_tokens)
            .map_err(AgentError::Compaction)?;
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

    /// 轮次运行结束后借出会话，供终态 metadata / usage 落盘复用同一个写者。
    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
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
        let effective_max_parallel_tool_calls = if capabilities.supports_parallel_tool_calls {
            capabilities.max_parallel_tool_calls.max(1)
        } else {
            1
        };
        let tool_choice = ToolChoicePolicy {
            mode: ToolChoiceMode::Auto,
            // 请求上限对齐 provider 静态声明的并行工具能力（无声明或声明不支持
            // 并行时回退 1）；本地 worker 窗口使用同一有效上限。请求上限
            // 低于 provider 声明会导致合法多调用响应被响应校验拒绝。
            max_tool_calls: effective_max_parallel_tool_calls,
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
                        emit_tool_start(events, &item.call);
                    }
                    let executions = execute_tool_batch_parallel(
                        &self.registry,
                        &prepared_calls,
                        self.session.cwd(),
                        cancellation,
                        effective_max_parallel_tool_calls,
                        events,
                    );
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
        let selector = parse_model_selector(&self.config.model);
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
                reasoning_effort: variant.to_string(),
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
        let on_message_update = &mut events.on_message_update;
        let on_provider_attempt = &mut events.on_provider_attempt;
        let mut on_stream = |event: ProviderStreamEvent| match event {
            ProviderStreamEvent::OutputTextDelta { delta } => {
                if let Some(on_update) = on_message_update.as_deref_mut() {
                    on_update(&delta);
                }
            }
        };
        let mut on_attempt = |event: ProviderAttemptEvent| {
            let event = bind_attempt_ordinal(event, model_turn_ordinal);
            if let Some(on_attempt) = on_provider_attempt.as_deref_mut() {
                on_attempt(event);
            }
        };
        let mut observed_attempt = |event: ProviderAttemptEvent| {
            on_attempt(event);
            true
        };
        match self.provider.complete_stream_observed(
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

/// 解析 `provider/model#variant` 模型选择器。仅当 provider/model/variant 三段
/// 都显式存在时返回（transport 对 reasoning replay 做绑定校验，缺 variant 时
/// 无法可靠重建绑定，安全跳过投影）。
fn parse_model_selector(selector: &str) -> Option<(&str, &str, &str)> {
    let (provider_name, model_and_effort) = selector.split_once('/')?;
    if provider_name.is_empty() || model_and_effort.is_empty() {
        return None;
    }
    let (model_name, variant) = model_and_effort.rsplit_once('#')?;
    if model_name.is_empty() || variant.is_empty() {
        return None;
    }
    Some((provider_name, model_name, variant))
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

fn bind_attempt_ordinal(
    event: ProviderAttemptEvent,
    model_turn_ordinal: u32,
) -> ProviderAttemptEvent {
    match event {
        ProviderAttemptEvent::Finished(mut occurrence) => {
            occurrence.model_turn_ordinal = Some(model_turn_ordinal);
            ProviderAttemptEvent::Finished(occurrence)
        }
        other => other,
    }
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
    if let Some(callback) = events.on_diagnostic.as_deref_mut() {
        callback(&diagnostic);
    }
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
