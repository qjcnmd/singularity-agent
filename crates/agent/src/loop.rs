//! Singularity 核心 Agent 执行循环。
//!
//! 轮步编排驻留本文件：内层循环逐轮驱动，发送前基于上一轮真实 usage（缺失
//! 时用装配估算）做主动压缩，调用采样层，并在 provider 明确返回
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

use std::sync::{Arc, Mutex};

use singularity_core::CancellationToken;
use singularity_model::{
    DEFAULT_MAX_TOOLS_PER_REQUEST, ModelError, ModelErrorKind, ModelPreferences, ModelTurnStatus,
    ModelUsage, Provider, ProviderError, ToolChoicePolicy,
};
use thiserror::Error;

use self::events::diagnostic_code;
pub use self::events::{AgentDiagnostic, AgentDiagnosticSeverity, AgentEvent, AgentEvents};
pub(crate) use self::events::{emit, emit_diagnostic};
pub use self::inbox::{TurnInbox, TurnInboxHandle};
pub use self::request::TurnRetryConfig;
pub(crate) use self::request::{SendOutcome, instruction_message, send_with_retry};

use self::inbox::lock_inbox;
use self::request::{AttemptOutcome, TurnRequestSpec, effective_max_output_tokens};
use crate::compaction::{
    CompactionBudget, CompactionConfig, CompactionEngine, CompactionOutcome, ContextLedger,
};
use crate::message::{AgentMessage, assistant_response_message, tool_result_message, user_message};
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

fn is_cancelled_agent_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::Provider(provider) if provider.error.kind == ModelErrorKind::Cancelled
    )
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

/// 新 headless core 的 Agent：会话 + compaction + 工具注册表 + 模型提供方。
pub struct Agent {
    session: SessionManager,
    compaction: CompactionEngine,
    registry: ToolRegistry,
    provider: Arc<dyn Provider + Send + Sync>,
    config: AgentConfig,
    /// 工具 schema 的序列化串：注册表与 provider 契约在 Agent 生命周期内
    /// 不变，构造时序列化一次供逐请求的 Token 估算复用。
    tools_json: String,
    /// 活动 turn 的实时转向输入箱；内存态不持久化。
    inbox: TurnInboxHandle,
    /// 请求前上下文规模的唯一计量（usage 基线 + 尾部增量）。
    ledger: ContextLedger,
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
            .with_summary_max_tokens(config.compaction.summary_max_tokens)
            .with_retry(config.retry);
        // 不变量：tool_schemas_from 返回本仓静态类型 Vec<ModelToolSchema>，
        // serde 序列化仅在其类型定义错误时失败。
        #[allow(clippy::expect_used)]
        let tools_json = serde_json::to_string(&Self::tool_schemas_from(
            &registry,
            &provider.protocol_contract(),
        ))
        .expect("tool schemas serialize");
        Ok(Self {
            session,
            compaction,
            registry,
            provider,
            config,
            tools_json,
            inbox: Arc::new(Mutex::new(TurnInbox::default())),
            ledger: ContextLedger::new(),
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
        // ledger 按轮独立：每轮以空 usage 基线起步，首轮触发点由装配估算全额兜底。
        self.ledger = ContextLedger::new();
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
            strict_tool_schema: false,
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
                                &call.arguments,
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
                    self.append_session_or_fail(
                        &mut outcome,
                        assistant_response_message(&response),
                    )?;
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
                            tool_result_message(
                                &call.tool_call_id,
                                &call.tool_name,
                                &call.arguments,
                                execution,
                            ),
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
    /// overflow 时不能保留正常近期窗口；强制路径把近期保留预算压到 0，
    /// 只保留绝对必要的最近安全边界（toolResult 永不切）。
    fn force_compact(
        &mut self,
        cancellation: &CancellationToken,
        events: &mut AgentEvents,
    ) -> Result<CompactionOutcome> {
        let mut budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        // 强制溢出恢复是显式模式：provider 已拒绝该请求时，不保留正常
        // 近期内容。
        budget.keep_recent_tokens = 0;
        let tokens_before = self
            .ledger
            .estimate()
            .unwrap_or(self.assembled_context_estimate()?);
        match self
            .compaction
            .compact(&mut self.session, &budget, tokens_before, cancellation)
        {
            Ok(result) => {
                self.ledger.invalidate();
                Ok(result)
            }
            Err(crate::compaction::CompactionError::Session(error)) => {
                Err(AgentError::Session(error))
            }
            Err(error) => {
                emit_diagnostic(
                    events,
                    AgentDiagnostic::warning(
                        diagnostic_code::COMPACTION_FAILED,
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
        let tokens_before = self
            .ledger
            .estimate()
            .unwrap_or(self.assembled_context_estimate()?);
        let result = self
            .compaction
            .compact(&mut self.session, &budget, tokens_before, cancellation)
            .map_err(AgentError::Compaction)?;
        self.ledger.invalidate();
        Ok(result)
    }

    /// 以正常请求同一装配 seam 重建当前上下文并估算规模：压缩前记录的
    /// tokens_before 必须反映完整装配（消息、工具 schema、reasoning replay、
    /// 输出预算与固定余量），而非退化占位。
    fn assembled_context_estimate(&self) -> Result<u64> {
        let (messages, replays) = self.assemble_messages()?;
        let max_output_tokens =
            effective_max_output_tokens(self.provider.as_ref(), self.config.max_output_tokens);
        Ok(self.estimate_assembled(&messages, &replays, max_output_tokens))
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
                            if request::is_context_overflow_error(&provider.error)
                    ) {
                        outcome.usage_complete = false;
                        if overflow_retried {
                            return AttemptOutcome::Failed(error);
                        }
                        overflow_retried = true;
                        let forced = match self.force_compact(cancellation, events) {
                            Ok(result) => result,
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

    /// 已积累 progress 的失败收敛：终态标记为 Failed 并关闭 inbox；
    /// turns == 0 时原样返回根因，否则包装为 RunFailed。
    fn to_run_failed(&self, error: AgentError, mut outcome: AgentOutcome) -> AgentError {
        outcome.terminal_reason = AgentTerminalReason::Failed;
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

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
