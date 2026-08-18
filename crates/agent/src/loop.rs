//! Pi 式 Agent 循环（新 headless core，Phase 2d）。
//!
//! 语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的 `dist/agent-loop.js`
//! `runAgentLoop` 双层循环：内层循环处理工具调用与 steer 注入，外层循环在
//! 代理将要停止时消费 follow-up 队列。会话、compaction、工具与模型边界分别由
//! `session.rs`/`compaction.rs`/`tools/`/`singularity_model` 提供。
//!
//! 与 Pi 的差异（Phase 2d 简化）：
//! - 事件回调仅保留文本增量与工具生命周期子集，无完整扩展事件流。
//! - steer/follow-up 为内存队列（裁决 9：不持久化）。
//! - provider 流式不可用时回退 `complete`（旧 AgentLoop 同款 fallback）。
//! - 中断：外部 `CancellationToken` 取消时终止并返回已完成的文本（`aborted=true`）。

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
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderError, ProviderProtocolContract,
    ProviderReasoningReplay, ProviderStreamEvent, ProviderToolReasoningMode, ToolChoiceMode,
    ToolChoicePolicy, is_strict_tool_schema_compatible,
};
use thiserror::Error;
use uuid::Uuid;

use crate::compaction::{
    CompactionBudget, CompactionEngine, CompactionOutcome, DEFAULT_KEEP_RECENT_TOKENS,
    DEFAULT_RESERVE_TOKENS,
};
use crate::message::{
    AgentMessageRole, ContentBlock, assistant_response_message, tool_result_message, user_message,
};
use crate::session::{SessionEntryType, SessionError, SessionManager};
use crate::tools::{
    ExecuteContext, PreparedTool, ToolError, ToolExecution, ToolExecutionMode, ToolPreflight,
    ToolRegistry,
};

/// 工具开始回调签名：工具名、tool call id、结构化参数。
pub type ToolExecutionCallback<'a> = &'a mut dyn FnMut(&str, &str, &Value);
/// 工具更新回调签名：工具名、tool call id、结构化参数、partial result。
pub type ToolExecutionUpdateCallback<'a> = &'a mut dyn FnMut(&str, &str, &Value, &str);
/// 工具结束回调签名：工具名、tool call id、最终执行结果。
pub type ToolExecutionEndCallback<'a> = &'a mut dyn FnMut(&str, &str, &ToolExecution);

/// 核心事件回调（Pi 事件集的 Phase 2d 最小子集）。
pub struct AgentEvents<'a> {
    /// assistant 文本增量。
    pub on_message_update: Option<&'a mut dyn FnMut(&str)>,
    /// 工具开始执行（工具名、tool call id、结构化参数）。
    pub on_tool_execution_start: Option<ToolExecutionCallback<'a>>,
    /// 工具执行中的流式输出增量（工具名、tool call id、参数、partial result）。
    pub on_tool_execution_update: Option<ToolExecutionUpdateCallback<'a>>,
    /// 工具执行结束（工具名、tool call id、最终结果）。
    pub on_tool_execution_end: Option<ToolExecutionEndCallback<'a>>,
}

impl<'a> AgentEvents<'a> {
    pub fn new() -> Self {
        Self {
            on_message_update: None,
            on_tool_execution_start: None,
            on_tool_execution_update: None,
            on_tool_execution_end: None,
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
    /// 单次任务允许的最大模型轮数；达到后以轮数预算耗尽结束。
    pub max_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system_prompt: String::new(),
            context_window: 128_000,
            max_output_tokens: 4_096,
            max_turns: 50,
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
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// 一次 `run` 的最终结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最后一次无工具调用的 assistant 文本（中断/轮数上限时可能为空）。
    pub final_text: String,
    pub turns: u32,
    /// 各轮 provider 调用的聚合 usage。
    pub usage: ModelUsage,
    pub compacted: bool,
    /// 因外部取消而提前终止时为 true。
    pub aborted: bool,
}

/// steer 注入的线程安全句柄：`Agent::steer_handle` 的返回类型，供进程边界
/// （app-server turn/input）在 `run` 期间向队列注入消息；`run` 每轮开始时 drain。
pub type SteerHandle = Arc<Mutex<VecDeque<String>>>;

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
            mutation_queue: None,
        },
    ) {
        Ok(execution) => execution,
        Err(error) => tool_error_execution(error),
    }
}

fn execute_tool_batch_sequential(
    registry: &ToolRegistry,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    events: &mut AgentEvents<'_>,
) -> Vec<ToolExecution> {
    calls
        .iter()
        .map(|item| {
            let execution = if let Some(execution) = &item.preflight_execution {
                execution.clone()
            } else {
                let prepared = item
                    .prepared
                    .expect("prepared tool call must have a preflight result");
                execute_prepared_tool(registry, prepared, &item.call, cwd, cancellation, |text| {
                    emit_tool_update(events, &item.call, text);
                })
            };
            emit_tool_end(events, &item.call, &execution);
            execution
        })
        .collect()
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
    let worker_limit = usize::try_from(max_parallel_tool_calls.max(1)).unwrap_or(usize::MAX);
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
                    let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
    /// 转向队列：下一轮（工具执行后交付）注入，内存态不持久化。
    steer_queue: SteerHandle,
    /// 跟进队列：代理将要停止时注入，内存态不持久化。
    follow_up_queue: SteerHandle,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider + Send + Sync>,
        registry: ToolRegistry,
        config: AgentConfig,
        session: SessionManager,
    ) -> Result<Self> {
        // 摘要请求与正常请求使用同一模型偏好：不绑定时 max_output_tokens 缺省
        // 会让摘要按 reserve 推导出超过模型输出上限的 max_tokens（真实链路已被
        // Provider HTTP 400 拒绝），模型选择也会回落到 provider 默认。
        let mut compaction_preferences = ModelPreferences::default();
        if !config.model.is_empty() {
            compaction_preferences.model_name = Some(config.model.clone());
        }
        compaction_preferences.max_output_tokens = Some(effective_max_output_tokens(
            provider.as_ref(),
            config.max_output_tokens,
        ));
        let compaction = CompactionEngine::new(Arc::clone(&provider))
            .with_model_preferences(compaction_preferences);
        Ok(Self {
            session,
            compaction,
            registry,
            provider,
            config,
            steer_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// 返回 steer 队列的线程安全句柄；`run` 每轮开始时 drain 队列内容。
    pub fn steer_handle(&self) -> SteerHandle {
        Arc::clone(&self.steer_queue)
    }

    /// 返回 follow-up 队列的线程安全句柄；`run` 在代理准备停止时 drain。
    pub fn follow_up_handle(&self) -> SteerHandle {
        Arc::clone(&self.follow_up_queue)
    }

    /// 注入转向：下一轮 provider 调用前作为 user 消息追加到会话上下文。
    pub fn steer(&mut self, text: &str) {
        lock_queue(&self.steer_queue).push_back(text.to_string());
    }

    /// 注入跟进：代理将要停止（无工具调用且文本非空）时继续一轮再停止。
    pub fn follow_up(&mut self, text: &str) {
        lock_queue(&self.follow_up_queue).push_back(text.to_string());
    }

    /// 运行一个完整 Agent 循环：输入持久化为 user 消息，内层循环处理工具调用与
    /// steer，外层循环消费 follow-up；停止后返回聚合结果。
    ///
    /// `cancellation` 取消时终止并返回已完成文本（`aborted=true`，不视为错误）。
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
            aborted: false,
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
                    outcome.aborted = true;
                    return Ok(outcome);
                }
                if outcome.turns >= self.config.max_turns {
                    return Ok(outcome);
                }
                // 注入 steer 队列全部消息（作为 user 消息追加到本轮上下文）。
                let steer_messages = std::mem::take(&mut *lock_queue(&self.steer_queue));
                for text in steer_messages {
                    self.session.append_message(user_message(&text))?;
                }
                self.compact_before_request(
                    &capabilities,
                    &tools,
                    max_output_tokens,
                    cancellation,
                )?;
                let request = self.build_request(
                    &preferences,
                    &capabilities,
                    &tools,
                    &tool_choice,
                    max_output_tokens,
                    outcome.turns,
                )?;
                let response = match self.stream_completion(&request, events, cancellation) {
                    Ok(response) => response,
                    Err(AgentError::Provider(error)) if is_context_overflow_error(&error.error) => {
                        if context_overflow_retried {
                            return Err(AgentError::Provider(error));
                        }
                        context_overflow_retried = true;
                        // 强制压缩失败时返回原始上下文溢出错误（保留真实因果，
                        // 与 H4 typed 错误语义一致），不掩盖为压缩错误。
                        if self.force_compact(cancellation).is_err() {
                            return Err(AgentError::Provider(error));
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                // 非 Success 响应（校验失败 Invalid 等）：typed 传播，不重试
                // （N3 裁决：瞬时失败只归传输层，循环层不再做整轮重试）。
                if response.status != ModelTurnStatus::Success {
                    let model_error = response.error.clone().unwrap_or_else(|| {
                        ModelError::new(
                            ModelErrorKind::UnknownProviderError,
                            "unknown provider error",
                        )
                    });
                    return Err(AgentError::Provider(ProviderError::from_model_error(
                        model_error,
                    )));
                }
                outcome.turns += 1;
                context_overflow_retried = false;
                aggregate_usage(&mut outcome.usage, &response.usage);
                let assistant_text = response
                    .assistant_message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let tool_calls = response.tool_calls.clone();
                if !tool_calls.is_empty() {
                    // 一次模型响应 = 一条 assistant 消息（v4 内容块：thinking +
                    // 文本 + 全部 tool_call 块，对齐 Pi AssistantMessage.content 数组）。
                    self.session
                        .append_message(assistant_response_message(&response))?;
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
                    let sequential = prepared_calls.iter().any(|item| {
                        item.prepared
                            .is_some_and(|prepared| prepared.mode == ToolExecutionMode::Sequential)
                    });
                    let executions = if sequential {
                        execute_tool_batch_sequential(
                            &self.registry,
                            &prepared_calls,
                            self.session.cwd(),
                            cancellation,
                            events,
                        )
                    } else {
                        execute_tool_batch_parallel(
                            &self.registry,
                            &prepared_calls,
                            self.session.cwd(),
                            cancellation,
                            effective_max_parallel_tool_calls,
                            events,
                        )
                    };
                    // Durable toolResult entries are always appended in assistant source order,
                    // regardless of completion/event order.
                    for (call, execution) in tool_calls.iter().zip(executions.iter()) {
                        self.session.append_message(tool_result_message(
                            &call.tool_call_id,
                            &call.tool_name,
                            execution,
                        ))?;
                    }
                    if cancellation.is_cancelled() {
                        outcome.aborted = true;
                        return Ok(outcome);
                    }
                    self.maybe_compact(
                        &mut outcome.compacted,
                        Some(&response.usage),
                        cancellation,
                    )?;
                    continue;
                }
                // 无工具调用：终态 assistant 消息持久化并退出内层循环。
                self.session
                    .append_message(assistant_response_message(&response))?;
                outcome.final_text = assistant_text;
                self.maybe_compact(&mut outcome.compacted, Some(&response.usage), cancellation)?;
                break;
            }
            // 代理将要停止：消费 follow-up 队列后回到内层循环。
            let follow_ups = std::mem::take(&mut *lock_queue(&self.follow_up_queue));
            if follow_ups.is_empty() {
                return Ok(outcome);
            }
            for text in follow_ups {
                self.session.append_message(user_message(&text))?;
            }
        }
    }

    /// 每轮模型请求前的 compaction preflight：把 system/developer 指令、会话
    /// 消息、tool schema 和 max output reserve 都计入预算；超窗则先强制 compact。
    fn compact_before_request(
        &mut self,
        capabilities: &ProviderProtocolContract,
        tools: &[ModelToolSchema],
        max_output_tokens: u32,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let estimated = self.estimated_request_tokens(capabilities, tools, max_output_tokens)?;
        if estimated <= self.config.context_window {
            return Ok(());
        }
        let budget = CompactionBudget {
            context_window: self.config.context_window,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        };
        let context_tokens = self.estimate_context_tokens(None)?;
        self.compaction
            .compact(&mut self.session, &budget, context_tokens, cancellation)?;
        let after = self.estimated_request_tokens(capabilities, tools, max_output_tokens)?;
        if after > self.config.context_window {
            return Err(AgentError::Loop(format!(
                "request context still exceeds window after compaction (estimated {after} > {})",
                self.config.context_window
            )));
        }
        Ok(())
    }

    /// 无条件执行一次 compaction（provider 明确返回 context overflow 时使用）。
    ///
    /// overflow 时不能保留正常 20000-token 近期窗口；强制路径把 keep/recent
    /// reserve 都压到 0，只保留绝对必要的最近安全边界（toolResult 永不切）。
    fn force_compact(&mut self, cancellation: &CancellationToken) -> Result<()> {
        let budget = CompactionBudget {
            context_window: self.config.context_window,
            reserve_tokens: 0,
            keep_recent_tokens: 0,
        };
        // 强制路径不受 should_compact 的窗口判定限制；usage_or_estimate 传
        // u64::MAX 使 compact 进入真正的摘要/切点流程。
        self.compaction
            .compact(&mut self.session, &budget, u64::MAX, cancellation)?;
        Ok(())
    }

    fn estimated_request_tokens(
        &self,
        capabilities: &ProviderProtocolContract,
        tools: &[ModelToolSchema],
        max_output_tokens: u32,
    ) -> Result<u64> {
        let estimate = |text: &str| self.compaction.estimate_tokens(text);
        let instruction_tokens = instruction_message(capabilities, &self.config.system_prompt)
            .map(|message| estimate(&message.content))
            .unwrap_or(0);
        let messages = self.session.build_session_context()?.messages;
        // 预算必须覆盖最终 wire 请求：除 content 外，provider 还会重放每条
        // tool 消息的 tool_call_id 与 assistant tool_calls 的 id/name/raw_arguments。
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
        Ok(instruction_tokens
            .saturating_add(message_tokens)
            .saturating_add(tool_tokens)
            .saturating_add(max_output_tokens as u64)
            .saturating_add(32))
    }

    /// 组装单轮 provider 请求：system prompt（按能力选择 developer/system 角色，
    /// 均不支持时以 user 前缀注入）+ 会话历史（compaction 感知）。
    fn build_request(
        &self,
        preferences: &ModelPreferences,
        capabilities: &ProviderProtocolContract,
        tools: &[ModelToolSchema],
        tool_choice: &ToolChoicePolicy,
        max_output_tokens: u32,
        turn: u32,
    ) -> Result<ModelTurnRequest> {
        let mut messages = Vec::new();
        if let Some(instruction) = instruction_message(capabilities, &self.config.system_prompt) {
            messages.push(instruction);
        }
        messages.extend(self.session.build_session_context()?.messages);
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), turn),
            messages,
        );
        request.tools = tools.to_vec();
        request.tool_choice = tool_choice.clone();
        request.provider_reasoning_history = self.reasoning_history_for_request();
        request.model_preferences = ModelPreferences {
            model_name: preferences.model_name.clone(),
            max_output_tokens: Some(max_output_tokens),
            ..ModelPreferences::default()
        };
        Ok(request)
    }

    /// 从 durable assistant entries 恢复 provider-private continuation。
    ///
    /// Responses replay 必须直接使用 JSONL 中保存的 opaque output items；
    /// reasoning summary 只作为可见投影，绝不用于重建 Responses state。
    /// Chat 旧条目若没有 private replay，仍可从 thinking block 重建
    /// `reasoning_content`，以保持已存在会话的兼容性。
    fn reasoning_history_for_request(&self) -> Vec<ProviderReasoningReplay> {
        let tool_reasoning_mode = self.provider.protocol_contract().tool_reasoning_mode;
        let Ok(context) = self.session.build_context_entries() else {
            return Vec::new();
        };
        let selector = parse_model_selector(&self.config.model);
        let mut replays = Vec::new();
        for entry in &context {
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
    ) -> Result<ModelTurnResponse> {
        let mut ignore_attempt = |_attempt: singularity_model::ProviderAttemptEvent| true;
        let mut on_stream = |event: ProviderStreamEvent| match event {
            ProviderStreamEvent::OutputTextDelta { delta } => {
                if let Some(on_update) = events.on_message_update.as_deref_mut() {
                    on_update(&delta);
                }
            }
        };
        match self.provider.complete_stream_observed(
            request,
            cancellation,
            &mut on_stream,
            &mut ignore_attempt,
        ) {
            Ok(response) => Ok(response),
            Err(error)
                if error.error.code.as_deref() == Some(PROVIDER_STREAMING_UNSUPPORTED_CODE) =>
            {
                self.provider
                    .complete(request, cancellation)
                    .map_err(AgentError::Provider)
            }
            Err(error) => {
                // 传输层重试（`MAX_PROVIDER_ATTEMPTS`，对齐 Codex stream 5 次重试）
                // 已耗尽：typed 传播（N3 单层归属裁决），不再转换为整轮重试。
                Err(AgentError::Provider(error))
            }
        }
    }

    /// 每轮 provider 调用后检查 compaction：budget = context_window +
    /// Pi 默认 reserve（16384）/keep_recent（20000）；触发则生成摘要并追加
    /// CompactionEntry（后续上下文经 build_session_context 自动使用新基线）。
    ///
    /// 摘要生成失败（provider 瞬时错误/无效响应）**降级**为记录后继续：已完成的
    /// assistant 结果已持久化，不应因摘要失败丢弃整轮；会话写入失败（真实存储
    /// 错误）保持传播。
    fn maybe_compact(
        &mut self,
        compacted: &mut bool,
        last_usage: Option<&ModelUsage>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let budget = CompactionBudget {
            context_window: self.config.context_window,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        };
        let context_tokens = self.estimate_context_tokens(last_usage)?;
        if !self.compaction.should_compact(context_tokens, &budget) {
            return Ok(());
        }
        match self
            .compaction
            .compact(&mut self.session, &budget, context_tokens, cancellation)
        {
            Ok(CompactionOutcome::Compacted { .. }) => *compacted = true,
            Ok(_) => {}
            Err(crate::compaction::CompactionError::Session(error)) => {
                return Err(AgentError::Session(error));
            }
            Err(error) => {
                eprintln!("[singularity-agent] compaction skipped: {error}");
            }
        }
        Ok(())
    }

    /// 上下文 token 估算（Pi `estimateContextTokens`）：有 usage 时用最近一次
    /// provider 调用的 total_tokens 加其之后追加消息的估算；否则全量估算。
    fn estimate_context_tokens(&self, last_usage: Option<&ModelUsage>) -> Result<u64> {
        let messages = self.session.build_session_context()?.messages;
        let estimate_all: u64 = messages
            .iter()
            .map(|message| self.compaction.estimate_tokens(&message.content))
            .sum();
        let Some(usage) = last_usage.filter(|usage| usage.total_tokens > 0) else {
            return Ok(estimate_all);
        };
        let mut trailing = 0u64;
        for message in messages.iter().rev() {
            if message.role == ModelRole::Assistant {
                break;
            }
            trailing += self.compaction.estimate_tokens(&message.content);
        }
        Ok(usage.total_tokens + trailing)
    }
}

/// 加锁 steer/follow-up 队列；中毒时恢复（工具执行中 panic 不应使注入通道永久不可用）。
fn lock_queue(queue: &Mutex<VecDeque<String>>) -> std::sync::MutexGuard<'_, VecDeque<String>> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
fn aggregate_usage(aggregate: &mut ModelUsage, response: &ModelUsage) {
    aggregate.input_tokens += response.input_tokens;
    aggregate.output_tokens += response.output_tokens;
    aggregate.total_tokens += response.total_tokens;
    aggregate.cached_input_tokens += response.cached_input_tokens;
    aggregate.reasoning_tokens += response.reasoning_tokens;
    aggregate.usage_present |= response.usage_present;
}

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
