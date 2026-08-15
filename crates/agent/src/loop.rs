//! Pi 式 Agent 循环（新 headless core，Phase 2d）。
//!
//! 语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的 `dist/agent-loop.js`
//! `runAgentLoop` 双层循环：内层循环处理工具调用与 steer 注入，外层循环在
//! 代理将要停止时消费 follow-up 队列。会话、compaction、工具与模型边界分别由
//! `session.rs`/`compaction.rs`/`tools/`/`singularity_model` 提供。
//!
//! 与 Pi 的差异（Phase 2d 简化）：
//! - 事件回调仅保留最小子集（文本增量/工具开始/工具输出增量），无完整事件流。
//! - steer/follow-up 为内存队列（裁决 9：不持久化）。
//! - provider 流式不可用时回退 `complete`（旧 AgentLoop 同款 fallback）。
//! - 中断：外部 `CancellationToken` 取消时终止并返回已完成的文本（`aborted=true`）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelError, ModelErrorKind, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderError, ProviderProtocolContract,
    ProviderStreamEvent, ToolChoiceMode, ToolChoicePolicy, is_strict_tool_schema_compatible,
};
use thiserror::Error;
use uuid::Uuid;

use crate::compaction::{
    CompactionBudget, CompactionEngine, CompactionOutcome, DEFAULT_KEEP_RECENT_TOKENS,
    DEFAULT_RESERVE_TOKENS,
};
use crate::message::{AgentMessage, AgentMessageRole};
use crate::session::{SessionError, SessionManager};
use crate::tools::{ExecuteContext, ToolError, ToolExecution, ToolRegistry};

/// 工具执行回调签名：工具名、参数原文。
pub type ToolExecutionCallback<'a> = &'a mut dyn FnMut(&str, &str);

/// 核心事件回调（Pi 事件集的 Phase 2d 最小子集）。
pub struct AgentEvents<'a> {
    /// assistant 文本增量。
    pub on_message_update: Option<&'a mut dyn FnMut(&str)>,
    /// 工具开始执行（工具名、参数原文）。
    pub on_tool_execution_start: Option<ToolExecutionCallback<'a>>,
    /// 工具执行中的流式输出增量。
    pub on_tool_execution_update: Option<&'a mut dyn FnMut(&str)>,
}

impl<'a> AgentEvents<'a> {
    pub fn new() -> Self {
        Self {
            on_message_update: None,
            on_tool_execution_start: None,
            on_tool_execution_update: None,
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
    /// 最大模型轮数（旧实现基线，防失控）。
    pub max_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system_prompt: String::new(),
            context_window: 128_000,
            max_output_tokens: 4_096,
            max_turns: 16,
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

/// 一次失败的模型 turn 的**运行级重试**：单轮最多尝试次数（首次 + 最多 4 次重试）。
/// 这是传输层重试（`model/src/transport.rs`）耗尽后的第二层，只处理"整轮仍失败"。
pub const MAX_MODEL_RUN_ATTEMPTS: u32 = 5;
/// 最多重试次数（= 总尝试 - 1）。
pub const MAX_MODEL_RUN_RETRIES: u32 = MAX_MODEL_RUN_ATTEMPTS - 1;
/// 指数退避基底秒数：2s/4s/8s/16s。
const RETRY_BACKOFF_BASE_SECONDS: u64 = 2;

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
        let compaction = CompactionEngine::new(Arc::clone(&provider));
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
        // 本轮失败的瞬时类重试计数；成功时（`outcome.turns += 1` 处）归零，重试不递增 turns。
        let mut attempts = 0u32;
        self.session.append_message(user_message(input))?;

        let mut preferences = ModelPreferences::default();
        if !self.config.model.is_empty() {
            preferences.model_name = Some(self.config.model.clone());
        }
        // 静态能力声明决定 system prompt 角色、输出上限与 tool 策略（旧 AgentLoop 同款）。
        let capabilities = self.provider.protocol_contract();
        let max_output_tokens = u32::try_from(
            self.config
                .max_output_tokens
                .min(capabilities.max_output_tokens as u64),
        )
        .unwrap_or(u32::MAX);
        let tools = self.tool_schemas(&capabilities);
        let tool_choice = ToolChoicePolicy {
            mode: ToolChoiceMode::Auto,
            // 请求上限对齐 provider 静态声明的并行工具能力（无声明或声明不支持
            // 并行时回退 1）；执行仍逐个顺序完成（Pi 顺序执行基线）。请求上限
            // 低于 provider 声明会导致合法多调用响应被响应校验拒绝。
            max_tool_calls: if capabilities.supports_parallel_tool_calls {
                capabilities.max_parallel_tool_calls
            } else {
                1
            },
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
                let request = self.build_request(
                    &preferences,
                    &capabilities,
                    &tools,
                    &tool_choice,
                    max_output_tokens,
                    outcome.turns,
                )?;
                let response = self.stream_completion(&request, events, cancellation)?;
                // 模型调用整体失败（传输层重试已耗尽）：按瞬时类做运行级重试。
                // 重试是同一轮模型的重新请求，不递增 outcome.turns，也不计入 usage。
                if response.status != ModelTurnStatus::Success {
                    let model_error = response.error.clone().unwrap_or_else(|| {
                        ModelError::new(
                            ModelErrorKind::UnknownProviderError,
                            "unknown provider error",
                        )
                    });
                    // 仅 `Failed` 状态且判定为瞬时类才重试；`Invalid`（校验失败）不重试。
                    let retryable = response.status == ModelTurnStatus::Failed
                        && is_retryable_run_error(&model_error);
                    if retryable && attempts < MAX_MODEL_RUN_RETRIES {
                        attempts += 1;
                        let delay = Duration::from_secs(
                            RETRY_BACKOFF_BASE_SECONDS.saturating_mul(1u64 << (attempts - 1)),
                        );
                        // 退避等待期间可取消：取消则立即停止重试并返回原错误（app-server
                        // 侧会把取消态收敛为 aborted，不视为模型失败）。
                        if cancellable_sleep(cancellation, delay) {
                            return Err(AgentError::Loop(format!(
                                "model turn failed: {}",
                                model_error.message
                            )));
                        }
                        continue;
                    }
                    return Err(AgentError::Loop(format!(
                        "model turn failed: {}",
                        model_error.message
                    )));
                }
                outcome.turns += 1;
                attempts = 0;
                aggregate_usage(&mut outcome.usage, &response.usage);
                let assistant_text = response
                    .assistant_message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let tool_calls = response.tool_calls.clone();
                if !tool_calls.is_empty() {
                    // 每个 tool call 一条 assistant 消息（Phase 2a 会话 schema 单调用），
                    // 文本只挂在第一条上；随后逐个执行并把结果写回会话。
                    for (index, call) in tool_calls.iter().enumerate() {
                        self.session.append_message(assistant_tool_call_message(
                            if index == 0 {
                                assistant_text.clone()
                            } else {
                                String::new()
                            },
                            call,
                        ))?;
                    }
                    for call in &tool_calls {
                        if let Some(on_start) = events.on_tool_execution_start.as_deref_mut() {
                            on_start(&call.tool_name, &call.raw_arguments);
                        }
                        // 用短生命周期闭包包装 update 回调：`&mut dyn FnMut` 的 reborrow
                        // 会保留原对象生命周期，直接传入会把 ExecuteContext 的 cwd 借用
                        // 绑到回调生命周期上，导致与后续 session 写冲突。
                        let mut on_update = |text: &str| {
                            if let Some(callback) = events.on_tool_execution_update.as_deref_mut() {
                                callback(text);
                            }
                        };
                        let execution = match self.registry.execute(
                            &call.tool_name,
                            ExecuteContext {
                                args: call.arguments.clone(),
                                cwd: self.session.cwd(),
                                signal: Some(cancellation),
                                on_update: Some(&mut on_update),
                            },
                        ) {
                            Ok(execution) => execution,
                            // 未知工具/注册层错误按工具失败写入结果，不终止循环。
                            Err(error) => ToolExecution {
                                content: format!("tool execution failed: {error}"),
                                is_error: true,
                            },
                        };
                        if cancellation.is_cancelled() {
                            outcome.aborted = true;
                            return Ok(outcome);
                        }
                        self.session.append_message(tool_result_message(
                            &call.tool_call_id,
                            &call.tool_name,
                            &execution,
                        ))?;
                    }
                    self.maybe_compact(
                        &mut outcome.compacted,
                        Some(&response.usage),
                        cancellation,
                    )?;
                    continue;
                }
                // 无工具调用：终态 assistant 消息持久化并退出内层循环。
                self.session.append_message(AgentMessage {
                    role: AgentMessageRole::Assistant,
                    content: assistant_text.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    args: None,
                    timestamp: None,
                })?;
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
        let system_prompt_role = if capabilities.supports_developer_message {
            Some(ModelRole::Developer)
        } else if capabilities.supports_system_message {
            Some(ModelRole::System)
        } else {
            None
        };
        let mut messages = Vec::new();
        match system_prompt_role {
            Some(role) if !self.config.system_prompt.is_empty() => {
                messages.push(ModelMessage::text(role, self.config.system_prompt.clone()));
            }
            None if !self.config.system_prompt.is_empty() => {
                messages.push(ModelMessage::text(
                    ModelRole::User,
                    self.config.system_prompt.clone(),
                ));
            }
            _ => {}
        }
        messages.extend(self.session.build_session_context()?.messages);
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), turn),
            messages,
        );
        request.tools = tools.to_vec();
        request.tool_choice = tool_choice.clone();
        request.model_preferences = ModelPreferences {
            model_name: preferences.model_name.clone(),
            max_output_tokens: Some(max_output_tokens),
            ..ModelPreferences::default()
        };
        Ok(request)
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
                // 传输层重试（`MAX_PROVIDER_ATTEMPTS`）耗尽后的 `Err(ProviderError)`：
                // 瞬时类错误转成 `Ok(ModelTurnResponse::Failed)`，交由运行级重试
                // （loop.rs 调用方）按其既有 2s/4s/8s/16s 退避逻辑接管；不可重试
                // 错误（取消/挂起超时/认证/限额/校验/上下文溢出等）原样传播。
                if is_retryable_run_error(&error.error) {
                    Ok(ModelTurnResponse {
                        request_id: request.request_id.clone(),
                        response_id: format!("fail-{}", Uuid::new_v4().simple()),
                        status: ModelTurnStatus::Failed,
                        assistant_message: None,
                        tool_calls: Vec::new(),
                        usage: ModelUsage::default(),
                        finish_reason: None,
                        validation: None,
                        error: Some(*error.error),
                        provider_name: None,
                        model_name: None,
                        provider_attempt_metadata: None,
                        provider_reasoning_history: Vec::new(),
                    })
                } else {
                    Err(AgentError::Provider(error))
                }
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

/// 判断一次失败的模型 turn（`ModelTurnStatus::Failed`）是否应做运行级重试。
///
/// 已裁决边界：
/// - **可重试**（瞬时类）：`NetworkError`/`RateLimited`/`ProviderOverloaded` 类型、HTTP
///   429/5xx 状态码，以及 provider 文本含 overloaded/rate-limit/too many requests/5xx/
///   服务不可用/内部错误/网络错误等瞬时信号（类型分类优先，文本仅作补充）。
/// - **不可重试**：取消、挂起超时（`Timeout`，120s fail-fast）、认证/账户限额
///   （quota/billing/insufficient_quota/usage limit/balance）、校验失败、上下文溢出等。
///   `ModelTurnStatus::Invalid` 不在本函数内，调用方已排除。
fn is_retryable_run_error(error: &ModelError) -> bool {
    // 账户限额文本守卫最先执行：任何 kind（含瞬时类 RateLimited 等）命中
    // quota/billing 文本即不可重试——限额是账户状态，重试无意义。
    let lower = error.message.to_lowercase();
    if [
        "quota",
        "billing",
        "insufficient_quota",
        "usage limit",
        "balance",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return false;
    }
    // 明确的不可重试类型：优先级最高，即使文本命中瞬时模式也忽略。
    match error.kind {
        ModelErrorKind::Cancelled
        | ModelErrorKind::Timeout
        | ModelErrorKind::AuthError
        | ModelErrorKind::BudgetExceeded
        | ModelErrorKind::ContextLengthExceeded
        | ModelErrorKind::InvalidRequest
        | ModelErrorKind::ToolCallParseError
        | ModelErrorKind::JsonSchemaViolation
        | ModelErrorKind::ContentFilter
        | ModelErrorKind::UnsupportedCapability => return false,
        // 瞬时类：网络、限流、过载。
        ModelErrorKind::NetworkError
        | ModelErrorKind::RateLimited
        | ModelErrorKind::ProviderOverloaded => return true,
        // 其余（如 UnknownProviderError）继续依据状态码与文本判断。
        ModelErrorKind::UnknownProviderError => {}
    }
    // HTTP 429 或 5xx（类型可能已被归类为非瞬时的 UnknownProviderError 等）。
    if matches!(error.http_status, Some(429 | 500 | 502 | 503 | 504)) {
        return true;
    }
    // 瞬时类文本信号（补充，用于 provider 未归类的情况）。
    [
        "overloaded",
        "rate limit",
        "too many requests",
        "500",
        "502",
        "503",
        "504",
        "服务不可用",
        "内部错误",
        "网络错误",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

/// 可取消的退避等待：以短步进轮询取消标志，取消时立即返回 `true`
/// （调用方停止重试并返回原有错误）。非异步上下文（`run` 运行在阻塞线程）。
fn cancellable_sleep(cancellation: &CancellationToken, total: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(40);
    let mut remaining = total;
    while !remaining.is_zero() {
        if cancellation.is_cancelled() {
            return true;
        }
        let chunk = remaining.min(STEP);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
    false
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::User,
        content: text.to_string(),
        tool_call_id: None,
        tool_name: None,
        args: None,
        timestamp: None,
    }
}

fn assistant_tool_call_message(
    content: String,
    call: &singularity_model::ModelToolCall,
) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content,
        tool_call_id: Some(call.tool_call_id.clone()),
        tool_name: Some(call.tool_name.clone()),
        args: Some(call.arguments.clone()),
        timestamp: None,
    }
}

fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    execution: &ToolExecution,
) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: execution.content.clone(),
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        args: None,
        timestamp: None,
    }
}

/// 逐轮聚合 usage；cost_estimate 仅当所有轮都提供时求和。
fn aggregate_usage(aggregate: &mut ModelUsage, response: &ModelUsage) {
    aggregate.input_tokens += response.input_tokens;
    aggregate.output_tokens += response.output_tokens;
    aggregate.total_tokens += response.total_tokens;
    aggregate.cached_input_tokens += response.cached_input_tokens;
    aggregate.reasoning_tokens += response.reasoning_tokens;
    aggregate.cost_estimate = match (aggregate.cost_estimate, response.cost_estimate) {
        (Some(left), Some(right)) => Some(left + right),
        // 初始聚合值（None）不是缺轮；只有响应侧缺值才置 None。
        (None, Some(right)) => Some(right),
        _ => None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{CompactionEntry, SessionEntryType};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use singularity_model::{ModelToolCall, ModelToolParseStatus, ProviderStreamingCapability};

    /// 脚本化 FakeProvider：按脚本顺序弹出响应；`complete_stream` 以单次文本增量
    /// 投递 assistant 文本（覆盖流式路径），`complete` 无增量（覆盖回退/compaction 路径）。
    struct FakeProvider {
        steps: Mutex<VecDeque<FakeStep>>,
        requests: Mutex<Vec<ModelTurnRequest>>,
        contract: ProviderProtocolContract,
    }

    #[derive(Clone)]
    struct FakeStep {
        text: String,
        tool_calls: Vec<ModelToolCall>,
        usage: ModelUsage,
    }

    impl FakeProvider {
        fn new(contract: ProviderProtocolContract, steps: Vec<FakeStep>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                requests: Mutex::new(Vec::new()),
                contract,
            }
        }

        fn pop(&self) -> std::result::Result<FakeStep, ProviderError> {
            self.steps.lock().unwrap().pop_front().ok_or_else(|| {
                ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::UnknownProviderError,
                    "no scripted steps remaining",
                ))
            })
        }

        fn respond(&self, request: &ModelTurnRequest, step: &FakeStep) -> ModelTurnResponse {
            let mut assistant = ModelMessage::assistant_tool_calls(step.tool_calls.clone());
            assistant.content = step.text.clone();
            ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: format!("resp-{}", Uuid::new_v4().simple()),
                status: ModelTurnStatus::Success,
                assistant_message: Some(assistant),
                tool_calls: step.tool_calls.clone(),
                usage: step.usage.clone(),
                finish_reason: Some(if step.tool_calls.is_empty() {
                    "stop".to_string()
                } else {
                    "tool_calls".to_string()
                }),
                validation: None,
                error: None,
                provider_name: Some("fake".to_string()),
                model_name: Some("fake-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            }
        }
    }

    impl Provider for FakeProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            self.contract.clone()
        }

        fn streaming_capability(
            &self,
            _selected_protocol: singularity_model::ProviderApiProtocol,
        ) -> ProviderStreamingCapability {
            ProviderStreamingCapability::OutputTextDelta
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
            on_event: &mut dyn FnMut(ProviderStreamEvent),
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.requests.lock().unwrap().push(request.clone());
            let step = self.pop()?;
            if !step.text.is_empty() {
                on_event(ProviderStreamEvent::OutputTextDelta {
                    delta: step.text.clone(),
                });
            }
            Ok(self.respond(request, &step))
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.requests.lock().unwrap().push(request.clone());
            let step = self.pop()?;
            Ok(self.respond(request, &step))
        }
    }

    fn fake_contract() -> ProviderProtocolContract {
        ProviderProtocolContract {
            supports_tools: true,
            supports_parallel_tool_calls: true,
            supports_required_tool_choice: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: singularity_model::ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: 8,
            supports_json_mode: false,
            supports_system_message: false,
            supports_developer_message: true,
            max_parallel_tool_calls: 1,
            max_context_tokens: Some(128_000),
            max_output_tokens: 4_096,
        }
    }

    fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
        ModelToolCall {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            arguments: args.clone(),
            raw_arguments: serde_json::to_string(&args).unwrap(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        }
    }

    fn usage(input: u64, output: u64) -> ModelUsage {
        ModelUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
            cost_estimate: None,
        }
    }

    fn setup(steps: Vec<FakeStep>) -> (Agent, tempfile::TempDir, Arc<FakeProvider>) {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FakeProvider::new(fake_contract(), steps));
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        (agent, dir, provider)
    }

    /// 失败脚本假 provider：按脚本顺序返回失败 turn 或成功文本 turn；
    /// `calls` 统计本轮模型尝试总次数（首次 + 重试），用于验证重试计数。
    struct FailingProvider {
        steps: Mutex<VecDeque<FailStep>>,
        calls: std::sync::atomic::AtomicUsize,
        contract: ProviderProtocolContract,
    }

    #[derive(Clone)]
    enum FailStep {
        /// 返回 status=Failed + 给定 error 的 turn（触发运行级重试判定）。
        Fail(ModelError),
        /// 返回一次成功文本 turn（"重试后成功"场景）。
        Success(String),
    }

    impl FailingProvider {
        fn new(contract: ProviderProtocolContract, steps: Vec<FailStep>) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
                contract,
            }
        }

        fn try_respond(&self, request: &ModelTurnRequest) -> ModelTurnResponse {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(FailStep::Fail(error)) => ModelTurnResponse {
                    request_id: request.request_id.clone(),
                    response_id: format!("fail-{}", Uuid::new_v4().simple()),
                    status: ModelTurnStatus::Failed,
                    assistant_message: None,
                    tool_calls: Vec::new(),
                    usage: ModelUsage::default(),
                    finish_reason: None,
                    validation: None,
                    error: Some(error),
                    provider_name: Some("failing".to_string()),
                    model_name: Some("fake-model".to_string()),
                    provider_attempt_metadata: None,
                    provider_reasoning_history: Vec::new(),
                },
                Some(FailStep::Success(text)) => ModelTurnResponse::completed(
                    request.request_id.clone(),
                    format!("ok-{}", Uuid::new_v4().simple()),
                    text,
                ),
                // 脚本耗尽：视作未知瞬时错误，触发重试直至次数耗尽。
                None => ModelTurnResponse {
                    request_id: request.request_id.clone(),
                    response_id: format!("empty-{}", Uuid::new_v4().simple()),
                    status: ModelTurnStatus::Failed,
                    assistant_message: None,
                    tool_calls: Vec::new(),
                    usage: ModelUsage::default(),
                    finish_reason: None,
                    validation: None,
                    error: Some(ModelError::new(
                        ModelErrorKind::UnknownProviderError,
                        "no scripted steps remaining",
                    )),
                    provider_name: Some("failing".to_string()),
                    model_name: Some("fake-model".to_string()),
                    provider_attempt_metadata: None,
                    provider_reasoning_history: Vec::new(),
                },
            }
        }
    }

    impl Provider for FailingProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            self.contract.clone()
        }

        fn streaming_capability(
            &self,
            _selected_protocol: singularity_model::ProviderApiProtocol,
        ) -> ProviderStreamingCapability {
            ProviderStreamingCapability::OutputTextDelta
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
            _on_event: &mut dyn FnMut(ProviderStreamEvent),
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            Ok(self.try_respond(request))
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            Ok(self.try_respond(request))
        }
    }

    /// 从 `complete_stream` 直接返回 `Err(ProviderError)` 的假 provider：
    /// 模拟传输层重试（`MAX_PROVIDER_ATTEMPTS`）耗尽后仍失败的路径——在修复前这部分
    /// 是死代码，`stream_completion` 直接以 `Err(AgentError::Provider)` 向外传播，
    /// 运行级重试永远不触发。脚本按序在若干次失败后返回一次成功。
    struct ErrReturningProvider {
        /// 每次 `complete_stream` 弹出的结果：`Err(model_error)` 或 `Ok(text)`。
        steps: Mutex<VecDeque<std::result::Result<String, ModelError>>>,
        calls: std::sync::atomic::AtomicUsize,
        contract: ProviderProtocolContract,
    }

    impl ErrReturningProvider {
        fn new(
            contract: ProviderProtocolContract,
            steps: Vec<std::result::Result<String, ModelError>>,
        ) -> Self {
            Self {
                steps: Mutex::new(steps.into()),
                calls: std::sync::atomic::AtomicUsize::new(0),
                contract,
            }
        }

        fn try_respond(
            &self,
            request: &ModelTurnRequest,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.steps.lock().unwrap().pop_front() {
                Some(Err(error)) => Err(ProviderError::from_model_error(error)),
                Some(Ok(text)) => Ok(ModelTurnResponse::completed(
                    request.request_id.clone(),
                    format!("ok-{}", Uuid::new_v4().simple()),
                    text,
                )),
                // 脚本耗尽：视作瞬时类网络错误，触发重试直至次数耗尽。
                None => Err(ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::NetworkError,
                    "no scripted steps remaining",
                ))),
            }
        }
    }

    impl Provider for ErrReturningProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            self.contract.clone()
        }

        fn streaming_capability(
            &self,
            _selected_protocol: singularity_model::ProviderApiProtocol,
        ) -> ProviderStreamingCapability {
            ProviderStreamingCapability::OutputTextDelta
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
            _on_event: &mut dyn FnMut(ProviderStreamEvent),
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.try_respond(request)
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.try_respond(request)
        }
    }

    /// 恒返回 `ModelTurnStatus::Invalid` 的假 provider：校验失败绝不做运行级重试。
    struct InvalidStatusProvider {
        contract: ProviderProtocolContract,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl InvalidStatusProvider {
        fn new(contract: ProviderProtocolContract) -> Self {
            Self {
                contract,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Provider for InvalidStatusProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            self.contract.clone()
        }

        fn streaming_capability(
            &self,
            _selected_protocol: singularity_model::ProviderApiProtocol,
        ) -> ProviderStreamingCapability {
            ProviderStreamingCapability::OutputTextDelta
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
            _on_event: &mut dyn FnMut(ProviderStreamEvent),
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: format!("invalid-{}", Uuid::new_v4().simple()),
                status: ModelTurnStatus::Invalid,
                assistant_message: None,
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: None,
                validation: Some(singularity_model::ModelValidationResult {
                    valid: false,
                    errors: vec!["dropped duplicate tool call".to_string()],
                    warnings: Vec::new(),
                }),
                error: Some(ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
                    "response validation failed: dropped duplicate tool call",
                )),
                provider_name: Some("failing".to_string()),
                model_name: Some("fake-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            })
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: format!("invalid-{}", Uuid::new_v4().simple()),
                status: ModelTurnStatus::Invalid,
                assistant_message: None,
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: None,
                validation: Some(singularity_model::ModelValidationResult {
                    valid: false,
                    errors: vec!["dropped duplicate tool call".to_string()],
                    warnings: Vec::new(),
                }),
                error: Some(ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
                    "response validation failed",
                )),
                provider_name: Some("failing".to_string()),
                model_name: Some("fake-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            })
        }
    }

    /// 运行级重试：瞬时类失败后重试成功，总尝试数正确，且重试不消费 max_turns。
    #[test]
    fn retry_transient_failure_then_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![
                FailStep::Fail(ModelError::new(
                    ModelErrorKind::NetworkError,
                    "network flap",
                )),
                FailStep::Fail(ModelError::new(
                    ModelErrorKind::RateLimited,
                    "rate limit hit",
                )),
                FailStep::Success("recovered".to_string()),
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let outcome = agent
            .run("hello", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        // 3 次尝试（2 次失败重试 + 1 次成功），但只算 1 轮。
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "recovered");
        assert!(!outcome.aborted);
    }

    /// 运行级重试经真实 provider 失败路径：`complete_stream` 直接返回
    /// `Err(ProviderError)`（传输层重试耗尽后的形态），`stream_completion` 把瞬时类
    /// 错误转成 `Ok(ModelTurnResponse::Failed)`，运行级重试据此接管并在退避后重试成功。
    /// 修复前该 `Err` 路径是死代码——`stream_completion` 直接向外传播错误，这里会立即失败。
    #[test]
    fn retry_transient_err_provider_error_then_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(ErrReturningProvider::new(
            fake_contract(),
            vec![
                Err(ModelError::new(
                    ModelErrorKind::NetworkError,
                    "network flap after transport retries",
                )),
                Err(ModelError::new(
                    ModelErrorKind::ProviderOverloaded,
                    "provider overloaded",
                )),
                Ok("recovered".to_string()),
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let outcome = agent
            .run("hello", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        // 3 次尝试（2 次 Err 失败重试 + 1 次成功），但只算 1 轮。
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "Err(NetworkError/ProviderOverloaded) must be converted to Ok(Failed) and retried"
        );
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "recovered");
        assert!(!outcome.aborted);
    }

    /// 运行级重试经 provider 失败路径：`Err(ProviderError)` 中不可重试错误（挂起超时）
    /// 不被转换为 `Ok(Failed)`，保持 `Err(AgentError::Provider)` 传播，agent 直接失败且一次尝试。
    #[test]
    fn non_retryable_err_provider_error_fails_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(ErrReturningProvider::new(
            fake_contract(),
            vec![Err(ModelError::new(
                ModelErrorKind::Timeout,
                "request hung and timed out",
            ))],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let err = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        // 不可重试错误原样传播为 Provider 错误（含原 kind 消息）。
        assert!(
            err.to_string().contains("request hung and timed out"),
            "non-retryable Err must propagate as provider error, got: {}",
            err
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Timeout Err(ProviderError) must not be retried"
        );
    }

    /// 运行级重试：非瞬时类（挂起超时、账户限额、校验失败）不重试，直接失败。
    #[test]
    fn non_retryable_errors_fail_immediately_without_retry() {
        // 挂起超时（120s fail-fast 决策）：不重试。
        let timeout_dir = tempfile::tempdir().unwrap();
        let timeout_session =
            SessionManager::create(timeout_dir.path(), &timeout_dir.path().join("sessions"))
                .unwrap();
        let timeout_provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![FailStep::Fail(ModelError::new(
                ModelErrorKind::Timeout,
                "request hung and timed out",
            ))],
        ));
        let mut timeout_agent = Agent::new(
            timeout_provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            timeout_session,
        )
        .unwrap();
        let timeout_err = timeout_agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        assert!(timeout_err.to_string().contains("request hung"));
        assert_eq!(
            timeout_provider
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "timeout must not retry"
        );

        // 账户限额文本（quota）：不重试。
        let quota_dir = tempfile::tempdir().unwrap();
        let quota_session =
            SessionManager::create(quota_dir.path(), &quota_dir.path().join("sessions")).unwrap();
        let quota_provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![FailStep::Fail(ModelError::new(
                ModelErrorKind::UnknownProviderError,
                "insufficient_quota: account balance exhausted",
            ))],
        ));
        let mut quota_agent = Agent::new(
            quota_provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            quota_session,
        )
        .unwrap();
        let quota_err = quota_agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        assert!(quota_err.to_string().contains("insufficient_quota"));
        assert_eq!(
            quota_provider
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "quota must not retry"
        );

        // 瞬时 kind（RateLimited）携带 quota 文本：限额守卫优先，不重试。
        let mixed_dir = tempfile::tempdir().unwrap();
        let mixed_session =
            SessionManager::create(mixed_dir.path(), &mixed_dir.path().join("sessions")).unwrap();
        let mixed_provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![FailStep::Fail(ModelError::new(
                ModelErrorKind::RateLimited,
                "429 insufficient_quota: monthly usage limit reached",
            ))],
        ));
        let mut mixed_agent = Agent::new(
            mixed_provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            mixed_session,
        )
        .unwrap();
        let mixed_err = mixed_agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        assert!(mixed_err.to_string().contains("insufficient_quota"));
        assert_eq!(
            mixed_provider
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "RateLimited kind with quota text must not retry"
        );

        // 校验失败（Invalid 状态）：不重试。
        let invalid_dir = tempfile::tempdir().unwrap();
        let invalid_session =
            SessionManager::create(invalid_dir.path(), &invalid_dir.path().join("sessions"))
                .unwrap();
        let invalid_provider = Arc::new(InvalidStatusProvider::new(fake_contract()));
        let mut invalid_agent = Agent::new(
            invalid_provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            invalid_session,
        )
        .unwrap();
        let invalid_err = invalid_agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        assert!(invalid_err.to_string().contains("response validation"));
        assert_eq!(
            invalid_provider
                .calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "invalid status must not retry"
        );
    }

    /// 运行级重试：达到 5 次总尝试上限后返回原有错误。
    #[test]
    fn retry_exhausts_max_attempts_then_returns_original_error() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![
                FailStep::Fail(ModelError::new(
                    ModelErrorKind::ProviderOverloaded,
                    "provider overloaded",
                ));
                MAX_MODEL_RUN_ATTEMPTS as usize
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let err = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap_err();
        assert!(err.to_string().contains("provider overloaded"));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_MODEL_RUN_ATTEMPTS as usize
        );
    }

    /// 运行级重试：退避等待期间取消 → 立即停止重试并返回原错误。
    #[test]
    fn cancel_during_backoff_stops_retry() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        // 脚本只给一次成功；若未取消会继续重试并最终成功，用于对照。
        let provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![FailStep::Fail(ModelError::new(
                ModelErrorKind::NetworkError,
                "transient network hiccup",
            ))],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        // 首次调用立即失败后进入 2s 退避；之后短延迟取消，退避轮询应观察到并停止。
        let canceller = cancellation.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let err = agent
            .run("task", &mut AgentEvents::new(), &cancellation)
            .unwrap_err();
        handle.join().unwrap();
        assert!(
            err.to_string().contains("transient network hiccup"),
            "must return the original model error, got: {}",
            err
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation during backoff must stop before a retry call"
        );
        // 不应等待完整 2s 退避。
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "backoff must be cancellable, elapsed {:?}",
            started.elapsed()
        );
    }

    /// 1. 单轮文本响应 → 停止，usage 聚合正确。
    #[test]
    fn single_text_turn_stops_with_usage() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![FakeStep {
                text: "hello from model".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 50),
            }],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig {
                system_prompt: "be helpful".to_string(),
                ..AgentConfig::default()
            },
            session,
        )
        .unwrap();
        let mut events = AgentEvents::new();
        let mut deltas = String::new();
        let mut on_message_update = |delta: &str| deltas.push_str(delta);
        events.on_message_update = Some(&mut on_message_update);
        let outcome = agent
            .run("hi", &mut events, &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "hello from model");
        assert_eq!(outcome.usage.input_tokens, 100);
        assert_eq!(outcome.usage.output_tokens, 50);
        assert_eq!(outcome.usage.total_tokens, 150);
        assert!(!outcome.compacted);
        assert!(!outcome.aborted);
        assert_eq!(deltas, "hello from model");
        // 请求包含 system prompt（developer 角色）+ user 输入。
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
        assert_eq!(requests[0].messages[0].content, "be helpful");
        assert_eq!(requests[0].messages[1].role, ModelRole::User);
        assert_eq!(requests[0].messages[1].content, "hi");
    }

    /// 2. 工具调用序列：tool call → 工具执行 → 结果回写 session → 下一轮。
    #[test]
    fn tool_call_executes_and_results_feed_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                FakeStep {
                    text: String::new(),
                    tool_calls: vec![tool_call(
                        "call_1",
                        "write",
                        json!({ "path": "hello.txt", "content": "hello" }),
                    )],
                    usage: usage(50, 10),
                },
                FakeStep {
                    text: "done".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(120, 20),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let mut events = AgentEvents::new();
        let mut started: Vec<(String, String)> = Vec::new();
        let mut on_tool_execution_start =
            |name: &str, args: &str| started.push((name.to_string(), args.to_string()));
        events.on_tool_execution_start = Some(&mut on_tool_execution_start);
        let outcome = agent
            .run("create hello.txt", &mut events, &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.final_text, "done");
        assert_eq!(outcome.usage.input_tokens, 170);
        assert_eq!(outcome.usage.output_tokens, 30);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "write");
        // 工具真实执行：文件已创建。
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hello"
        );
        // 第二轮请求上下文重放 assistant tool_calls（session 投影）→ 真实 wire 形态。
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        assert_eq!(second.messages[1].role, ModelRole::Assistant);
        assert_eq!(second.messages[1].tool_calls.len(), 1);
        assert_eq!(second.messages[1].tool_calls[0].tool_call_id, "call_1");
        assert_eq!(second.messages[1].tool_calls[0].tool_name, "write");
        assert_eq!(second.messages[2].role, ModelRole::Tool);
        assert_eq!(second.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(second.messages[2].content.contains("Successfully wrote"));
    }

    /// 3. steer 注入：运行前队列注入 → 会话上下文持久化 → 后续轮次上下文中出现。
    #[test]
    fn steer_message_appears_in_following_turn_context() {
        let (mut agent, _dir, provider) = setup(vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call("call_1", "bash", json!({ "command": "echo hi" }))],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "final".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 10),
            },
        ]);
        agent.steer("please use a different approach");
        let outcome = agent
            .run(
                "do the task",
                &mut AgentEvents::new(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(outcome.final_text, "final");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        // 第一轮请求：user(input) 后紧跟 steer 消息。
        let texts: Vec<&str> = requests[0]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(texts, &["do the task", "please use a different approach"]);
        // 第二轮请求（工具执行后）：上下文重放中仍包含 steer 消息。
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.content == "please use a different approach")
        );
    }

    /// 4. follow_up：文本响应后 follow_up 队列非空 → 继续一轮再停止。
    #[test]
    fn follow_up_continues_one_more_turn() {
        let (mut agent, _dir, provider) = setup(vec![
            FakeStep {
                text: "first answer".to_string(),
                tool_calls: Vec::new(),
                usage: usage(10, 5),
            },
            FakeStep {
                text: "second answer".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
        ]);
        agent.follow_up("please continue");
        let outcome = agent
            .run(
                "question",
                &mut AgentEvents::new(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.final_text, "second answer");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.content == "please continue")
        );
    }

    /// 4b. steer_handle：run 期间经共享队列注入 → 下一轮上下文出现。
    #[test]
    fn steer_handle_injects_during_run() {
        let (mut agent, _dir, provider) = setup(vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call("call_1", "bash", json!({ "command": "echo hi" }))],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "final".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 10),
            },
        ]);
        let handle = agent.steer_handle();
        let mut events = AgentEvents::new();
        // 工具执行开始时（run 期间）从外部句柄注入转向消息。
        let mut on_tool_execution_start = |_name: &str, _args: &str| {
            handle
                .lock()
                .unwrap()
                .push_back("steer during run".to_string());
        };
        events.on_tool_execution_start = Some(&mut on_tool_execution_start);
        let outcome = agent
            .run("do the task", &mut events, &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.final_text, "final");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        // 工具执行后的下一轮请求包含运行中注入的消息。
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.content == "steer during run")
        );
    }

    /// 5. max_turns 上限：达到后终止，不再发起 provider 调用。
    #[test]
    fn max_turns_stops_the_loop() {
        let (mut agent, _dir, provider) = setup(vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call("call_1", "bash", json!({ "command": "echo a" }))],
                usage: usage(10, 5),
            },
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call("call_2", "bash", json!({ "command": "echo b" }))],
                usage: usage(10, 5),
            },
        ]);
        agent.config.max_turns = 2;
        let outcome = agent
            .run("go", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.turns, 2);
        // 两条脚本全部消费；若循环试图第三轮会因脚本耗尽而报错。
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert_eq!(outcome.final_text, "");
    }

    /// 6. 会话落盘：run 后 session 文件可重开，消息完整（树链正确）。
    #[test]
    fn session_file_roundtrip_after_run() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = SessionManager::create(dir.path(), &sessions).unwrap();
        let file = session.path().to_path_buf();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                FakeStep {
                    text: String::new(),
                    tool_calls: vec![tool_call(
                        "call_1",
                        "write",
                        json!({ "path": "out.txt", "content": "x" }),
                    )],
                    usage: usage(10, 5),
                },
                FakeStep {
                    text: "finished".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(20, 5),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        drop(agent);

        let reopened = SessionManager::open(&file).unwrap();
        let entries = reopened.build_context_entries().unwrap();
        let messages: Vec<&AgentMessage> = entries
            .iter()
            .filter_map(|entry| match &entry.entry_type {
                SessionEntryType::Message(message) => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, AgentMessageRole::User);
        assert_eq!(messages[0].content, "task");
        assert_eq!(messages[1].role, AgentMessageRole::Assistant);
        assert_eq!(messages[1].tool_name.as_deref(), Some("write"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            messages[1].args,
            Some(json!({ "path": "out.txt", "content": "x" }))
        );
        assert_eq!(messages[2].role, AgentMessageRole::ToolResult);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(messages[2].content.contains("Successfully wrote"));
        assert_eq!(messages[3].role, AgentMessageRole::Assistant);
        assert_eq!(messages[3].content, "finished");
        // 树链：每条 parent = 前一条 id，首条为根。
        for (index, entry) in entries.iter().enumerate() {
            if index == 0 {
                assert_eq!(entry.parent_id, "");
            } else {
                assert_eq!(entry.parent_id, entries[index - 1].id);
            }
        }
    }

    /// 7. compaction 触发：极小 context_window + 超过 keep_recent 的上下文
    ///    → run 中出现 CompactionEntry。
    #[test]
    fn tiny_context_window_triggers_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        // 第一轮带工具调用且 assistant 文本超大（> keep_recent 20000 tokens ≈ 80000 字符），
        // 使切点落在第一条消息之后、存在可摘要内容。
        let big_text = "x".repeat(100_000);
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                // 第一轮：工具调用 + 大 usage → 触发 compaction → 消费一条摘要脚本。
                FakeStep {
                    text: big_text.clone(),
                    tool_calls: vec![tool_call(
                        "call_1",
                        "write",
                        json!({ "path": "out.txt", "content": "x" }),
                    )],
                    usage: ModelUsage {
                        input_tokens: 900,
                        output_tokens: 100,
                        total_tokens: 1000,
                        cached_input_tokens: 0,
                        reasoning_tokens: 0,
                        cost_estimate: None,
                    },
                },
                // compaction 摘要调用（CompactionEngine 走 complete）。
                FakeStep {
                    text: "## Goal\ncompacted summary".to_string(),
                    tool_calls: Vec::new(),
                    usage: ModelUsage::default(),
                },
                // 第二轮：小 usage，compaction 后不再产生新摘要。
                FakeStep {
                    text: "second".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig {
                context_window: 100,
                ..AgentConfig::default()
            },
            session,
        )
        .unwrap();
        let outcome = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        assert!(outcome.compacted);
        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.final_text, "second");
        // 会话中出现 CompactionEntry，且上下文以摘要包裹的 user 消息开头。
        let entries = agent.session.build_context_entries().unwrap();
        let compaction_entries: Vec<&CompactionEntry> = entries
            .iter()
            .filter_map(|entry| match &entry.entry_type {
                SessionEntryType::Compaction(compaction) => Some(compaction),
                _ => None,
            })
            .collect();
        assert_eq!(compaction_entries.len(), 1);
        assert!(compaction_entries[0].summary.contains("compacted summary"));
        assert!(compaction_entries[0].first_kept_entry_id.is_some());
        let context = agent.session.build_session_context().unwrap();
        assert_eq!(context.messages[0].role, ModelRole::User);
        assert!(
            context.messages[0]
                .content
                .starts_with(crate::message::COMPACTION_SUMMARY_PREFIX)
        );
    }

    /// 8. 中断：取消令牌 → 终止并返回已完成的文本（aborted 语义，不报错）。
    #[test]
    fn cancelled_run_returns_aborted_outcome() {
        let (mut agent, _dir, provider) = setup(vec![FakeStep {
            text: "never used".to_string(),
            tool_calls: Vec::new(),
            usage: usage(10, 5),
        }]);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = agent
            .run("task", &mut AgentEvents::new(), &cancellation)
            .unwrap();
        assert!(outcome.aborted);
        assert_eq!(outcome.turns, 0);
        // 已取消时不发起任何 provider 调用。
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    /// 工具执行中途取消：bash 完成前观察到取消 → aborted。
    #[test]
    fn cancellation_during_tool_execution_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call(
                    "call_1",
                    "bash",
                    json!({ "command": "echo should-not-run" }),
                )],
                usage: usage(10, 5),
            }],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        // 在工具执行回调中取消：bash 工具在信号检查点观察到取消。
        let cancellation = CancellationToken::new();
        let mut events = AgentEvents::new();
        let canceller = cancellation.clone();
        let mut on_tool_execution_start = move |_name: &str, _args: &str| canceller.cancel();
        events.on_tool_execution_start = Some(&mut on_tool_execution_start);
        let outcome = agent.run("go", &mut events, &cancellation).unwrap();
        assert!(outcome.aborted);
        assert_eq!(outcome.turns, 1);
    }
}
