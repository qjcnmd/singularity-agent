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
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider,
    ProviderError, ProviderProtocolContract, ProviderStreamEvent, ToolChoiceMode, ToolChoicePolicy,
    is_strict_tool_schema_compatible,
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

/// 核心事件回调（Pi 事件集的 Phase 2d 最小子集）。
pub struct AgentEvents<'a> {
    /// assistant 文本增量。
    pub on_message_update: Option<&'a mut dyn FnMut(&str)>,
    /// 工具开始执行（工具名、参数原文）。
    pub on_tool_execution_start: Option<&'a mut dyn FnMut(&str, &str)>,
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
                let steer_messages =
                    std::mem::take(&mut *lock_queue(&self.steer_queue));
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
                outcome.turns += 1;
                aggregate_usage(&mut outcome.usage, &response.usage);
                if response.status != ModelTurnStatus::Success {
                    let detail = response
                        .error
                        .as_ref()
                        .map(|error| error.message.as_str())
                        .unwrap_or("unknown provider error");
                    return Err(AgentError::Loop(format!("model turn failed: {detail}")));
                }
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
                    self.maybe_compact(&mut outcome.compacted, Some(&response.usage))?;
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
                self.maybe_compact(&mut outcome.compacted, Some(&response.usage))?;
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
            Err(error) => Err(AgentError::Provider(error)),
        }
    }

    /// 每轮 provider 调用后检查 compaction：budget = context_window +
    /// Pi 默认 reserve（16384）/keep_recent（20000）；触发则生成摘要并追加
    /// CompactionEntry（后续上下文经 build_session_context 自动使用新基线）。
    fn maybe_compact(
        &mut self,
        compacted: &mut bool,
        last_usage: Option<&ModelUsage>,
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
        if matches!(
            self.compaction
                .compact(&mut self.session, &budget, context_tokens)?,
            CompactionOutcome::Compacted { .. }
        ) {
            *compacted = true;
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
    queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
    use singularity_model::{
        ModelError, ModelErrorKind, ModelToolCall, ModelToolParseStatus,
        ProviderStreamingCapability,
    };

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
    /// → run 中出现 CompactionEntry。
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
