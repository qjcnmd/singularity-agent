//! Singularity 核心 Agent 执行循环。
//!
//! 轮步编排驻留本文件：内层循环逐轮驱动，发送前基于上一轮真实 usage（缺失
//! 时用上下文条目估算求和兜底）做主动压缩，调用采样层，并在 provider 明确返回
//! ContextOverflow 时强制压缩、恰好一次重发；外层循环在代理将要停止时消费
//! 停止窗口内到达的引导输入。
//!
//! 请求管线（装配、压缩判定、重试包装、纯发送）在 [`self::request`]；事件
//! 出口类型在 [`self::events`]；turn 转向输入箱在 [`self::inbox`]。会话状态
//! 持久化、上下文压缩、工具注册分发与模型调用分别由 `session/` facade、
//! `compaction.rs`、`tools/` 与 `singularity_model` 模块提供支持。

#[path = "events.rs"]
mod events;
#[path = "inbox.rs"]
mod inbox;
#[path = "request.rs"]
mod request;

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelErrorKind, ModelPreferences, ModelUsage, Provider, ProviderError, split_model_selector,
};
use thiserror::Error;

use self::events::diagnostic_code;
pub use self::events::{AgentDiagnostic, AgentEvent, AgentEvents};
pub(crate) use self::events::{emit, emit_diagnostic};
pub use self::inbox::{TurnInbox, TurnInboxHandle};
pub use self::request::TurnRetryConfig;
pub(crate) use self::request::{SendOutcome, instruction_message, send_with_retry};

use self::inbox::lock_inbox;
use self::request::{AttemptOutcome, TurnRequestSpec, effective_max_output_tokens};
use crate::compaction::{CompactionConfig, CompactionEngine, CompactionOutcome, ContextLedger};
use crate::message::{
    AgentMessage, ContentBlock, assistant_response_message, tool_result_message, user_message,
};
use crate::session::{SessionError, SessionManager};
use crate::tools::ToolRegistry;
use crate::tools::batch::{PreparedToolCall, execute_tool_batch, tool_error_execution};

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

/// Agent 循环错误。
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("session error: {0}")]
    Session(#[from] SessionError),
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
}

/// 一次 `run` 的最终结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最后一次无工具调用的 assistant 文本（中断时可能为空）。
    pub final_text: String,
    /// 最终 assistant 响应是否因 provider 输出预算耗尽而截断。
    pub truncated: bool,
    pub turns: u32,
    /// 各轮 provider 调用的聚合 usage。
    pub usage: ModelUsage,
    /// `true` 表示每个已发出的 provider 请求都带有可确认的 usage；
    /// 取消/失败时未知的末次请求保持 `false`，不得估算成精确值。
    pub usage_complete: bool,
    pub terminal_reason: AgentTerminalReason,
}

fn is_cancelled_agent_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::Provider(provider) if provider.error.kind == ModelErrorKind::Cancelled
    ) || matches!(
        error,
        AgentError::Compaction(crate::compaction::CompactionError::Aborted)
    )
}

/// 逐轮聚合 provider 返回的真实 token/cache usage。
fn record_usage(outcome: &mut AgentOutcome, response: &ModelUsage) {
    outcome.usage.merge(response);
    if !response.usage_present {
        outcome.usage_complete = false;
    }
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
    /// 请求前上下文规模的唯一计量（usage 基线 + 尾部增量）。
    ledger: ContextLedger,
}

impl Agent {
    /// `inbox` 是本 Agent 的实时转向输入箱句柄：由生命周期所有者构造
    /// 控制面时创建并绑定，使注入窗口在 turn 开始前即已就绪。
    pub fn new(
        inbox: TurnInboxHandle,
        provider: Arc<dyn Provider + Send + Sync>,
        registry: ToolRegistry,
        config: AgentConfig,
        session: SessionManager,
    ) -> Self {
        // 摘要请求复用 provider/model 选择；输出上限由引擎按默认摘要预算与
        // provider 上限自行收敛，不与正常 turn 的输出预算共用通道。
        let mut compaction_preferences = ModelPreferences::default();
        if !config.model.is_empty() {
            compaction_preferences.model_name = Some(config.model.clone());
        }
        let compaction =
            CompactionEngine::new(Arc::clone(&provider), compaction_preferences, config.retry);
        Self {
            session,
            compaction,
            registry,
            provider,
            config,
            inbox,
            ledger: ContextLedger::new(),
        }
    }

    /// 移交本轮持有的会话写者。一轮 turn 只打开一次会话文件，终态落盘
    /// 必须复用这里返回的同一 `SessionManager`：再次全量打开会被写者锁
    /// 拒绝（WriterConflict）。
    pub fn into_session(self) -> SessionManager {
        self.session
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
            usage_complete: true,
            terminal_reason: AgentTerminalReason::Completed,
        };
        // ledger 按轮独立：每轮以空 usage 基线起步，首轮触发点由装配估算全额兜底。
        self.ledger = ContextLedger::new();
        self.session.append_message(user_message(input))?;

        let mut preferences = ModelPreferences::default();
        if !self.config.model.is_empty() {
            // 装配期一次性物化：请求只携带裸 model id，发送路径直接取用。
            preferences.model_name = split_model_selector(&self.config.model)
                .model
                .map(str::to_string);
        }
        // 静态能力声明决定 system prompt 角色、输出上限与 tool 策略。
        // tool 数量上限由协议合同声明；本地按模型给定顺序串行执行全部调用，
        // wire 侧 parallel_tool_calls 恒为 false。
        let capabilities = self.provider.protocol_contract();
        let max_output_tokens =
            effective_max_output_tokens(self.provider.as_ref(), self.config.max_output_tokens);
        let tools = self.tool_schemas(&capabilities);
        let mut spec = TurnRequestSpec {
            preferences,
            tools,
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
                outcome.turns += 1;
                self.ledger.record_usage(&response.usage);
                record_usage(&mut outcome, &response.usage);
                let assistant_text = response
                    .assistant_message
                    .as_ref()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                let tool_calls = response.tool_calls().to_vec();
                let length_truncated = response.is_length_truncated();
                if length_truncated && !tool_calls.is_empty() {
                    // 截断的响应可能含有仅部分解析的工具调用。持久化 assistant
                    // 消息并为每个调用生成模型可见失败，但绝不执行这些调用或将
                    // 它们显示为成功的工具事件。
                    let assistant = assistant_response_message(&response);
                    self.append_session_or_fail(&mut outcome, assistant.clone())?;
                    Self::emit_thinking(&assistant, events);
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
                    outcome.truncated = true;
                    outcome.final_text = assistant_text;
                    continue;
                }
                if !tool_calls.is_empty() {
                    // 单次模型响应对应一条 Assistant 消息（包含思考、文本与全部 tool_call 块）。
                    let assistant = assistant_response_message(&response);
                    self.append_session_or_fail(&mut outcome, assistant.clone())?;
                    Self::emit_thinking(&assistant, events);
                    // 查找、参数校验和执行模式判定先按 source order 完成；
                    // 未知工具/非法参数只生成模型可见失败，不进入并行线程。
                    let prepared_calls = tool_calls
                        .iter()
                        .map(|call| PreparedToolCall {
                            call: call.clone(),
                            prepared: self.registry.preflight(&call.tool_name, &call.arguments),
                        })
                        .collect::<Vec<_>>();
                    let executions = execute_tool_batch(
                        &self.registry,
                        &prepared_calls,
                        self.session.cwd(),
                        cancellation,
                        events,
                    );
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
                let assistant = assistant_response_message(&response);
                self.append_session_or_fail(&mut outcome, assistant.clone())?;
                Self::emit_thinking(&assistant, events);
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
    /// 与自动压缩共用同一固定保留预算；溢出恢复只保证安全切点（toolResult
    /// 永不切），保留预算内的最近上下文照常保留。失败经溢出恢复路径以
    /// `context_overflow_recovery_failed` 诊断收敛，此处不重复发诊断。
    fn force_compact(&mut self, cancellation: &CancellationToken) -> Result<CompactionOutcome> {
        let tokens_before = self
            .ledger
            .estimate()
            .unwrap_or(self.assembled_context_estimate()?);
        match self.compaction.compact(
            &mut self.session,
            &self.config.compaction,
            tokens_before,
            cancellation,
        ) {
            Ok(result) => {
                self.ledger.invalidate();
                Ok(result)
            }
            Err(crate::compaction::CompactionError::Session(error)) => {
                Err(AgentError::Session(error))
            }
            Err(error) => Err(AgentError::Compaction(error)),
        }
    }

    /// 用户显式请求的压缩：沿正常保留预算选择安全切点，但不要求上下文先
    /// 达到自动阈值。没有可摘要历史时返回 `NotNeeded`。
    pub fn compact_now(&mut self, cancellation: &CancellationToken) -> Result<CompactionOutcome> {
        let tokens_before = self
            .ledger
            .estimate()
            .unwrap_or(self.assembled_context_estimate()?);
        let result = self.compaction.compact(
            &mut self.session,
            &self.config.compaction,
            tokens_before,
            cancellation,
        )?;
        self.ledger.invalidate();
        Ok(result)
    }

    /// 以正常请求同一装配 seam 重建当前上下文的内容计量：压缩前记录的
    /// tokens_before 反映上下文条目的估算规模（只合计消息）。
    fn assembled_context_estimate(&self) -> Result<u64> {
        Ok(self.assemble_messages()?.token_estimate)
    }

    /// 单个轮步：先经 `prepare_request` 装配请求（含发送前主动压缩），再交给
    /// 采样层发送。provider 明确返回 ContextOverflow 时强制压缩并基于压缩后的
    /// 会话重建请求，恰好一次重发。
    fn run_turn(
        &mut self,
        spec: &TurnRequestSpec,
        outcome: &mut AgentOutcome,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        let mut request = match self.prepare_request(spec, outcome, events, cancellation) {
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
                            if provider.error.is_context_overflow()
                    ) {
                        outcome.usage_complete = false;
                        if overflow_retried {
                            return AttemptOutcome::Failed(error);
                        }
                        overflow_retried = true;
                        match self.force_compact(cancellation) {
                            Ok(_) => {}
                            Err(AgentError::Compaction(
                                crate::compaction::CompactionError::Aborted,
                            )) => {
                                return AttemptOutcome::Aborted;
                            }
                            Err(recovery_error) => {
                                // 强制压缩失败以压缩真因为主因上抛；原始
                                // overflow 经诊断保留上下文，不覆盖真因。
                                emit_diagnostic(
                                    events,
                                    AgentDiagnostic::warning(
                                        diagnostic_code::CONTEXT_OVERFLOW_RECOVERY_FAILED,
                                        "forced compaction failed to recover from context overflow"
                                            .to_string(),
                                    ),
                                );
                                return AttemptOutcome::Failed(recovery_error);
                            }
                        }
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

    /// 追加一条会话消息；失败时按「已积累 progress 则包装为 RunFailed」收敛并
    /// 返回错误。session 错误不可能触发 abort，直接走错误转换。
    fn append_session_or_fail(
        &mut self,
        outcome: &mut AgentOutcome,
        message: AgentMessage,
    ) -> Result<()> {
        if let Err(error) = self.session.append_message(message) {
            return Err(self.to_run_failed(AgentError::Session(error), outcome.clone()));
        }
        // 上报之后追加的条目进入 ledger 尾部增量（下一轮请求前压缩判定计入）。
        if let Some(entry) = self.session.entries().last() {
            self.ledger.record_appended(entry);
        }
        Ok(())
    }

    /// 持久化后的 assistant 消息内的思考块作为事实上报：每块一条事件，
    /// 供客户端实时展示，替代持久层回查。
    fn emit_thinking(message: &AgentMessage, events: &mut AgentEvents) {
        for block in message.thinking_blocks() {
            if let ContentBlock::Thinking { thinking, .. } = block
                && !thinking.trim().is_empty()
            {
                emit(
                    events,
                    AgentEvent::Thinking {
                        text: thinking.clone(),
                    },
                );
            }
        }
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
        Err(self.to_run_failed(error, outcome))
    }

    /// 已积累 progress 的失败收敛：关闭注入箱；
    /// turns == 0 时原样返回根因，否则包装为 RunFailed。
    fn to_run_failed(&self, error: AgentError, mut outcome: AgentOutcome) -> AgentError {
        outcome.usage_complete = false;
        lock_inbox(&self.inbox).close();
        if outcome.turns == 0 {
            error
        } else {
            AgentError::RunFailed {
                error: Box::new(error),
                outcome: Box::new(outcome),
            }
        }
    }
}
