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
                    // 文本 + 全部 tool_call 块，对齐 Pi AssistantMessage.content 数组）；
                    // 随后逐个顺序执行并把结果按 toolCallId 写回会话。
                    self.session
                        .append_message(assistant_response_message(&response))?;
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

    /// N2：从会话上下文中最后一条带 thinking 块的 assistant 消息投影 provider
    /// reasoning replay（跨重启随会话 JSONL 恢复；同轮内工具续接同样覆盖）。
    ///
    /// 仅 Chat 协议（`ReplayReasoningContent`）可忠实重建——replay 绑定
    /// （provider/model/variant）由模型选择器解析，thinking 文本即
    /// `reasoning_content`；Responses 协议需要原始 opaque output items，
    /// 无法从文本块重建（记录的限制，同轮续接仍由适配器响应侧 replay 承担
    /// 仅限同一 provider 实例的进程内路径）。选择器不可解析时安全跳过
    /// （空 history 被 transport 接受）。
    fn reasoning_history_for_request(&self) -> Vec<ProviderReasoningReplay> {
        if self.provider.protocol_contract().tool_reasoning_mode
            != ProviderToolReasoningMode::ReplayReasoningContent
        {
            return Vec::new();
        }
        let Ok(context) = self.session.build_context_entries() else {
            return Vec::new();
        };
        // 每条带 thinking 块与工具调用的 assistant 消息各投影一个 replay：
        // transport 校验要求每条工具消息都有绑定 replay（防丢 reasoning 续接，
        // 与 DeepSeek 要求每条 tool_calls 消息携带 reasoning_content 一致），
        // 只投影最后一条会在多轮工具后校验失败。
        let Some((provider_name, model_name, variant)) = parse_model_selector(&self.config.model)
        else {
            return Vec::new();
        };
        let mut replays = Vec::new();
        for entry in &context {
            let SessionEntryType::Message(message) = &entry.entry_type else {
                continue;
            };
            if message.role != AgentMessageRole::Assistant || !message.has_tool_calls() {
                continue;
            }
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
                // 已耗尽：typed 传播（N3 单层归属裁决），不再转换为运行级重试。
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

/// 逐轮聚合 usage；cost_estimate 仅当所有轮都提供时求和。
fn aggregate_usage(aggregate: &mut ModelUsage, response: &ModelUsage) {
    aggregate.input_tokens += response.input_tokens;
    aggregate.output_tokens += response.output_tokens;
    aggregate.total_tokens += response.total_tokens;
    aggregate.cached_input_tokens += response.cached_input_tokens;
    aggregate.reasoning_tokens += response.reasoning_tokens;
    aggregate.usage_present |= response.usage_present;
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
    use crate::message::AgentMessage;
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
            usage_present: true,
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

    /// 失败脚本假 provider：按脚本顺序返回失败 turn（status=Failed）；`calls`
    /// 统计模型调用总次数，用于验证失败路径不重试。
    struct FailingProvider {
        steps: Mutex<VecDeque<FailStep>>,
        calls: std::sync::atomic::AtomicUsize,
        contract: ProviderProtocolContract,
    }

    #[derive(Clone)]
    enum FailStep {
        /// 返回 status=Failed + 给定 error 的 turn。
        Fail(ModelError),
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
                // 脚本耗尽：视作未知错误（非瞬时类文本不触发任何重试语义）。
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

    /// 瞬时类失败不再运行级重试（N3 单层归属）：一次调用后 typed 传播原错误。
    #[test]
    fn transient_failure_propagates_typed_after_single_call() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FailingProvider::new(
            fake_contract(),
            vec![FailStep::Fail(ModelError::new(
                ModelErrorKind::ProviderOverloaded,
                "provider overloaded",
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
        assert!(
            matches!(err, AgentError::Provider(_)),
            "transient provider failure must propagate typed, got: {err:?}"
        );
        assert!(err.to_string().contains("provider overloaded"));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no run-level retry after N3"
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
        assert_eq!(messages[0].content_text(), "task");
        assert_eq!(messages[1].role, AgentMessageRole::Assistant);
        let ContentBlock::ToolCall { id, name, args } =
            messages[1].tool_calls().first().expect("tool call block")
        else {
            panic!("expected tool call block");
        };
        assert_eq!(id, "call_1");
        assert_eq!(name, "write");
        assert_eq!(*args, json!({ "path": "out.txt", "content": "x" }));
        assert_eq!(messages[2].role, AgentMessageRole::ToolResult);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(messages[2].content_text().contains("Successfully wrote"));
        assert_eq!(messages[3].role, AgentMessageRole::Assistant);
        assert_eq!(messages[3].content_text(), "finished");
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
                        input_tokens: 19_000,
                        output_tokens: 1_000,
                        total_tokens: 20_000,
                        cached_input_tokens: 0,
                        reasoning_tokens: 0,
                        cost_estimate: None,
                        usage_present: true,
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
                context_window: 30_000,
                max_output_tokens: 1,
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

    /// 7b. compaction 摘要请求必须继承有效输出上限：不绑定时会按 reserve
    ///     派生超过模型 max_output_tokens 的请求（真实链路被 Provider 400 拒绝）。
    #[test]
    fn compaction_summarization_respects_model_output_limit() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let big_text = "x".repeat(100_000);
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                FakeStep {
                    text: big_text,
                    tool_calls: vec![tool_call(
                        "call_1",
                        "write",
                        json!({ "path": "out.txt", "content": "x" }),
                    )],
                    usage: ModelUsage {
                        input_tokens: 19_000,
                        output_tokens: 1_000,
                        total_tokens: 20_000,
                        cached_input_tokens: 0,
                        reasoning_tokens: 0,
                        cost_estimate: None,
                        usage_present: true,
                    },
                },
                FakeStep {
                    text: "## Goal\nsummary".to_string(),
                    tool_calls: Vec::new(),
                    usage: ModelUsage::default(),
                },
                FakeStep {
                    text: "done".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig {
                // 配置远大于 fake_contract 的 4096：有效上限必须取两者较小值。
                context_window: 30_000,
                max_output_tokens: 1_000_000,
                ..AgentConfig::default()
            },
            session,
        )
        .unwrap();
        let outcome = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        assert!(outcome.compacted);
        let requests = provider.requests.lock().unwrap();
        let summarization = requests
            .iter()
            .find(|request| request.request_id.starts_with("compaction-"))
            .expect("summarization request recorded");
        assert_eq!(
            summarization.model_preferences.max_output_tokens,
            Some(4_096),
            "summarization output limit must be capped by the model capability"
        );
    }

    #[test]
    fn instruction_message_adapts_to_provider_roles() {
        let developer = ProviderProtocolContract {
            supports_developer_message: true,
            supports_system_message: true,
            ..ProviderProtocolContract::default()
        };
        let system = ProviderProtocolContract {
            supports_developer_message: false,
            supports_system_message: true,
            ..ProviderProtocolContract::default()
        };
        let neither = ProviderProtocolContract {
            supports_developer_message: false,
            supports_system_message: false,
            ..ProviderProtocolContract::default()
        };
        assert_eq!(
            instruction_message(&developer, "x").unwrap().role,
            ModelRole::Developer
        );
        assert_eq!(
            instruction_message(&system, "x").unwrap().role,
            ModelRole::System
        );
        assert_eq!(
            instruction_message(&neither, "x").unwrap().role,
            ModelRole::User
        );
        assert!(instruction_message(&developer, "").is_none());
    }

    struct OverflowProvider {
        stream_calls: std::sync::atomic::AtomicUsize,
        complete_calls: std::sync::atomic::AtomicUsize,
        overflow_times: usize,
        /// true 时摘要生成（`complete`）直接失败，用于验证强制压缩失败的降级路径。
        fail_summary: bool,
        contract: ProviderProtocolContract,
    }

    impl Provider for OverflowProvider {
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
            let call = self
                .stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call < self.overflow_times {
                return Err(ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::ContextLengthExceeded,
                    "context window overflow",
                )));
            }
            let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
            assistant.content = "done after compact".to_string();
            Ok(ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: "overflow-ok".to_string(),
                status: ModelTurnStatus::Success,
                assistant_message: Some(assistant),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: Some("stop".to_string()),
                validation: None,
                error: None,
                provider_name: Some("overflow".to_string()),
                model_name: Some("overflow-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            })
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.complete_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_summary {
                return Err(ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::UnknownProviderError,
                    "summary generation failed",
                )));
            }
            let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
            assistant.content = "## Goal\ncompacted".to_string();
            Ok(ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: "compaction-ok".to_string(),
                status: ModelTurnStatus::Success,
                assistant_message: Some(assistant),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: Some("stop".to_string()),
                validation: None,
                error: None,
                provider_name: Some("overflow".to_string()),
                model_name: Some("overflow-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            })
        }
    }

    #[test]
    fn context_overflow_forces_one_compaction_retry_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "old user".to_string(),
                }],
                tool_call_id: None,
                tool_name: None,
                timestamp: None,
            })
            .unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "old assistant".to_string(),
                }],
                tool_call_id: None,
                tool_name: None,
                timestamp: None,
            })
            .unwrap();
        let provider = Arc::new(OverflowProvider {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            complete_calls: std::sync::atomic::AtomicUsize::new(0),
            overflow_times: 1,
            fail_summary: false,
            contract: fake_contract(),
        });
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let outcome = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.final_text, "done after compact");
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            provider
                .complete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            agent
                .session
                .build_context_entries()
                .unwrap()
                .iter()
                .any(|entry| matches!(entry.entry_type, SessionEntryType::Compaction(_)))
        );
    }

    #[test]
    fn second_context_overflow_fails_without_retrying_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "old user".to_string(),
                }],
                tool_call_id: None,
                tool_name: None,
                timestamp: None,
            })
            .unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "old assistant".to_string(),
                }],
                tool_call_id: None,
                tool_name: None,
                timestamp: None,
            })
            .unwrap();
        let provider = Arc::new(OverflowProvider {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            complete_calls: std::sync::atomic::AtomicUsize::new(0),
            overflow_times: 2,
            fail_summary: false,
            contract: fake_contract(),
        });
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let error = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .expect_err("second overflow fails");
        assert!(error.to_string().contains("context window overflow"));
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            provider
                .complete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn failed_force_compaction_returns_original_overflow_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "old user".to_string(),
                }],
                tool_call_id: None,
                tool_name: None,
                timestamp: None,
            })
            .unwrap();
        let provider = Arc::new(OverflowProvider {
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
            complete_calls: std::sync::atomic::AtomicUsize::new(0),
            overflow_times: 1,
            // 强制压缩的摘要生成失败：应保留原始上下文溢出错误（真实因果），
            // 不得把失败掩盖为压缩错误。
            fail_summary: true,
            contract: fake_contract(),
        });
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let error = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .expect_err("overflow with failed compaction must fail");
        assert!(
            matches!(error, AgentError::Provider(_)),
            "original overflow error must be preserved, got: {error:?}"
        );
        assert!(error.to_string().contains("context window overflow"));
        assert_eq!(
            provider
                .complete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn preflight_compacts_before_first_normal_request() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                FakeStep {
                    text: "summary".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
                FakeStep {
                    text: "never sent".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig {
                context_window: 500,
                max_output_tokens: 1,
                ..AgentConfig::default()
            },
            session,
        )
        .unwrap();
        let error = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .expect_err("request does not fit even after compaction");
        assert!(error.to_string().contains("still exceeds window"));
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.request_id.starts_with("compaction-")),
            "no normal turn request may be sent: {requests:?}"
        );
    }

    #[test]
    fn preflight_budgets_historical_tool_call_raw_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        // 历史 tool call：content 很小，raw_arguments 巨大。content-only 预算
        // 看不见它，但 provider 会按 wire 重放 id/name/raw_arguments。
        let big_arguments = "x".repeat(20_000);
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "call write".to_string(),
                    },
                    ContentBlock::ToolCall {
                        id: "call_big_1".to_string(),
                        name: "write".to_string(),
                        args: json!({ "path": "big.txt", "content": big_arguments }),
                    },
                ],
                tool_call_id: Some("call_big_1".to_string()),
                tool_name: Some("write".to_string()),
                timestamp: None,
            })
            .unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::ToolResult,
                content: vec![ContentBlock::Text {
                    text: "wrote".to_string(),
                }],
                tool_call_id: Some("call_big_1".to_string()),
                tool_name: Some("write".to_string()),
                timestamp: None,
            })
            .unwrap();
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![
                FakeStep {
                    text: "summary".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
                FakeStep {
                    text: "never sent".to_string(),
                    tool_calls: Vec::new(),
                    usage: usage(0, 0),
                },
            ],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig {
                context_window: 3000,
                max_output_tokens: 1,
                ..AgentConfig::default()
            },
            session,
        )
        .unwrap();
        let error = agent
            .run("task", &mut AgentEvents::new(), &CancellationToken::new())
            .expect_err("large tool-call arguments must be budgeted before the request");
        assert!(error.to_string().contains("still exceeds window"));
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.request_id.starts_with("compaction-")),
            "no normal turn request may carry un-budgeted tool arguments: {requests:?}"
        );
    }

    #[test]
    fn orphaned_tool_call_reopens_without_executing_tool_again() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let target = workspace.join("should-not-exist.txt");
        let mut session = SessionManager::create(&workspace, &dir.path().join("sessions")).unwrap();
        session
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "calling write".to_string(),
                    },
                    ContentBlock::ToolCall {
                        id: "orphan_write_1".to_string(),
                        name: "write".to_string(),
                        args: json!({"path": target, "content": "must not be written"}),
                    },
                ],
                tool_call_id: Some("orphan_write_1".to_string()),
                tool_name: Some("write".to_string()),
                timestamp: None,
            })
            .unwrap();
        drop(session);
        let file = dir.path().join("sessions").join(
            std::fs::read_dir(dir.path().join("sessions"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        );
        let mut session = SessionManager::open_existing(&file).unwrap();
        assert_eq!(session.repair_orphaned_tool_calls().unwrap(), 1);
        let provider = Arc::new(FakeProvider::new(
            fake_contract(),
            vec![FakeStep {
                text: "final".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            }],
        ));
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            AgentConfig::default(),
            session,
        )
        .unwrap();
        let outcome = agent
            .run("resume", &mut AgentEvents::new(), &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.final_text, "final");
        assert!(
            !target.exists(),
            "reopen repair must not execute the orphaned tool"
        );
        let entries = agent.session.build_context_entries().unwrap();
        assert!(entries.iter().any(|entry| {
            matches!(
                &entry.entry_type,
                SessionEntryType::Message(message)
                    if message.role == AgentMessageRole::ToolResult
                        && message.tool_call_id.as_deref() == Some("orphan_write_1")
                        && message.content_text().contains("do not retry")
            )
        }));
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
