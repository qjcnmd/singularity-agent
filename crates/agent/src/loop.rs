//! Singularity 核心 Agent 执行循环：单一 Agent execution seam。
//!
//! 轮步编排驻留本文件：内层循环逐轮驱动，发送前基于 [`ContextView`] 的真实
//! usage 基线（缺失时用装配估算兜底）做主动压缩，调用采样层，并在 provider
//! 明确返回 ContextOverflow 时强制压缩重发——恢复预算按 turn 计，至多一次；
//! 再次溢出保留原始根因失败。外层循环在代理将要停止
//! 时消费停止窗口内到达的引导输入。
//!
//! 每个执行边界同时落盘 operation ledger 事实：模型 step 的 attempt、provider
//! 观测、工具启动（含 replay 分类与预分配结果 id）、已注入的转向控制。记录先
//! 于对应实时事件 durable；恢复据此重建事实，绝不重放未知副作用。
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
    ModelConfigurationSnapshot, ModelErrorKind, ModelUsage, Provider, ProviderError,
};
use thiserror::Error;

use self::events::diagnostic_code;
pub use self::events::{AgentDiagnostic, AgentEvent, AgentEvents};
pub(crate) use self::events::{emit, emit_diagnostic};
pub use self::inbox::{TurnInbox, TurnInboxHandle};
pub(crate) use self::request::{AttemptLedger, SendOutcome, instruction_message, send_with_retry};

use self::inbox::lock_inbox;
use self::request::{AttemptOutcome, TurnRequestSpec};
use crate::compaction::{CompactionConfig, CompactionEngine, CompactionOutcome};
use crate::message::{
    AgentMessage, ContentBlock, assistant_response_message, tool_result_message, user_message,
};
use crate::session::context::ContextView;
use crate::session::{
    CompactionReason, ControlDisposition, LedgerRecord, PendingWriteKind, SessionError,
    SessionWriter, StepKind, lock_writer,
};
use crate::tools::ToolRegistrySnapshot;
use crate::tools::batch::{PreparedToolCall, execute_tool_batch, tool_error_execution};

/// Agent 运行配置：一次 turn 冻结的提示词与模型/压缩事实。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// 模型静态声明的 context window（compaction 触发预算依据）。
    pub compaction: CompactionConfig,
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

/// 新 headless core 的 Agent：会话写者 + operation 范围 + compaction +
/// 工具注册表快照 + 模型提供方。
pub struct Agent {
    /// 共享会话写者：执行线程与协调器控制面共用同一 [`SessionManager`]
    /// 实例，各操作短暂加锁串行追加（`lock_writer`），绝不跨 provider/工具
    /// 调用持锁。控制接受与执行追加经同一实例落盘，不存在绕过
    /// [`SessionManager`] 的第二写者。
    session: SessionWriter,
    compaction: CompactionEngine,
    registry: ToolRegistrySnapshot,
    provider: Arc<dyn Provider + Send + Sync>,
    /// runtime 在 turn 边界解析并冻结的唯一模型配置事实。
    model: ModelConfigurationSnapshot,
    config: AgentConfig,
    /// 活动 turn 的实时转向输入箱；内存态不持久化。
    inbox: TurnInboxHandle,
    /// 请求前上下文规模的唯一计量（usage 基线 + 尾部增量）。
    context: ContextView,
    /// 本 turn 绑定的 durable operation 范围。
    operation_id: String,
    /// 本 operation 内 assistant step attempt 的单调计数：durable
    /// `step_attempt` 与 `provider_attempt` 的 attempt 序号唯一来源
    /// （operation 内连续，归约据此校验，不随模型轮次或重试重新起算）。
    assistant_step_attempts: u32,
    /// 本 operation 内 compaction step attempt 的单调计数。
    compaction_attempts: u32,
    /// 本 turn 的强制溢出恢复预算（data-model：at most once per turn）。
    /// 每次 run 恰好一个 turn；预算随 turn 起落，绝不跨 turn 携带。
    overflow_recovery_used: bool,
}

impl Agent {
    /// `inbox` 是本 Agent 的实时转向输入箱句柄：由生命周期所有者构造
    /// 控制面时创建并绑定，使注入窗口在 turn 开始前即已就绪。
    pub fn new(
        inbox: TurnInboxHandle,
        provider: Arc<dyn Provider + Send + Sync>,
        model: ModelConfigurationSnapshot,
        registry: ToolRegistrySnapshot,
        config: AgentConfig,
        session: SessionWriter,
        operation_id: String,
    ) -> Result<Self> {
        let compaction = CompactionEngine::new(Arc::clone(&provider), model.clone());
        let context = ContextView::derive(&lock_writer(&session))?;
        Ok(Self {
            session,
            compaction,
            registry,
            provider,
            model,
            config,
            inbox,
            context,
            operation_id,
            assistant_step_attempts: 0,
            compaction_attempts: 0,
            overflow_recovery_used: false,
        })
    }

    fn append_record(&mut self, record: LedgerRecord) -> std::result::Result<(), SessionError> {
        lock_writer(&self.session).append_record(record).map(|_| ())
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
        lock_writer(&self.session).append_message(user_message(input))?;
        self.track_last_entry();

        let capabilities = self.model.capabilities.clone();
        let max_output_tokens = self.model.capabilities.max_output_tokens;
        let tools = self.registry.provider_schemas(&capabilities);
        let mut spec = TurnRequestSpec {
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
                // 注入转向队列全部消息（作为 user 消息追加到本轮上下文），
                // 每条以 durable control_accepted 记录其接受顺序与归宿。
                let drained = lock_inbox(&self.inbox).drain();
                for request in drained {
                    let text = request.text.clone().unwrap_or_default();
                    self.append_session_or_fail(&mut outcome, user_message(&text))?;
                    self.append_record(request.disposition_record(ControlDisposition::Injected))
                        .map_err(AgentError::Session)?;
                }
                let model_turn_ordinal = outcome.turns.saturating_add(1);
                spec.turn = outcome.turns;
                let (response, assistant_result_entry_id) = match self.run_turn(
                    &spec,
                    &mut outcome,
                    events,
                    cancellation,
                    model_turn_ordinal,
                ) {
                    AttemptOutcome::Response(response, result_entry_id) => {
                        (*response, result_entry_id)
                    }
                    AttemptOutcome::Aborted => return self.abort_outcome(outcome),
                    AttemptOutcome::Failed(error) => {
                        return self.fail_after_progress(error, outcome);
                    }
                };
                outcome.turns += 1;
                self.context.record_usage(&response.usage);
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
                    self.append_session_or_fail_with_id(
                        &mut outcome,
                        &assistant_result_entry_id,
                        assistant.clone(),
                    )?;
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
                    self.append_session_or_fail_with_id(
                        &mut outcome,
                        &assistant_result_entry_id,
                        assistant.clone(),
                    )?;
                    Self::emit_thinking(&assistant, events);
                    // 查找、参数校验和执行模式判定先按 source order 完成；
                    // 未知工具/非法参数只生成模型可见失败，不进入并行线程。
                    let prepared_calls = tool_calls
                        .iter()
                        .map(|call| PreparedToolCall {
                            call: call.clone(),
                            prepared: self.registry.preflight(&call.tool_name, &call.arguments),
                            result_entry_id: lock_writer(&self.session).reserve_entry_id(),
                        })
                        .collect::<Vec<_>>();
                    // 每个调用在执行前落盘 tool_started（副作用可能已发生），
                    // 携带恢复重放分类与预分配结果条目 id。
                    for (source_order, prepared) in prepared_calls.iter().enumerate() {
                        if matches!(prepared.prepared, crate::tools::ToolPreflight::Ready(_)) {
                            self.append_record(LedgerRecord::WriteDeferred {
                                operation_id: self.operation_id.clone(),
                                entry_id: prepared.result_entry_id.clone(),
                                kind: PendingWriteKind::ToolResult,
                            })
                            .map_err(AgentError::Session)?;
                            self.append_record(LedgerRecord::ToolStarted {
                                operation_id: self.operation_id.clone(),
                                tool_call_id: prepared.call.tool_call_id.clone(),
                                tool_name: prepared.call.tool_name.clone(),
                                source_order: source_order as u32,
                                effective_args: prepared.call.arguments.clone(),
                                result_entry_id: prepared.result_entry_id.clone(),
                                replay: self.registry.replay_class(&prepared.call.tool_name),
                            })
                            .map_err(AgentError::Session)?;
                        }
                    }
                    // 会话写者锁只用于读取 cwd，随即释放——绝不在工具执行期间
                    // 持有（执行线程与协调器控制面共享同一写者，跨工具执行持锁
                    // 会阻塞控制接受与终态落盘）。
                    let cwd = lock_writer(&self.session).cwd().to_path_buf();
                    let executions = execute_tool_batch(
                        &self.registry,
                        &prepared_calls,
                        &cwd,
                        cancellation,
                        events,
                    );
                    // 持久的 toolResult 条目始终按 assistant source order 追加，
                    // 与完成/事件顺序无关；结果落在 tool_started 预分配的条目 id 上。
                    for (prepared, execution) in prepared_calls.iter().zip(executions.iter()) {
                        self.append_session_or_fail_with_id(
                            &mut outcome,
                            &prepared.result_entry_id,
                            tool_result_message(
                                &prepared.call.tool_call_id,
                                &prepared.call.tool_name,
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
                let assistant = assistant_response_message(&response);
                self.append_session_or_fail_with_id(
                    &mut outcome,
                    &assistant_result_entry_id,
                    assistant.clone(),
                )?;
                Self::emit_thinking(&assistant, events);
                outcome.final_text = assistant_text;
                outcome.truncated = length_truncated;
                break;
            }
            // 代理将要停止：消费停止窗口内到达的转向输入后回到内层循环。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            for request in pending_inputs {
                let text = request.text.clone().unwrap_or_default();
                self.append_session_or_fail(&mut outcome, user_message(&text))?;
                self.append_record(request.disposition_record(ControlDisposition::Injected))
                    .map_err(AgentError::Session)?;
            }
        }
    }

    /// 无条件执行一次 compaction（provider 明确返回 context overflow 时使用）。
    fn force_compact(&mut self, cancellation: &CancellationToken) -> Result<CompactionOutcome> {
        let tokens_before = self
            .context
            .effective_tokens()
            .unwrap_or_else(|| self.context.estimated_tokens());
        match self.compact_with_record(CompactionReason::Overflow, tokens_before, cancellation) {
            Ok(result) => {
                self.context.rebuild(&lock_writer(&self.session))?;
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
            .context
            .effective_tokens()
            .unwrap_or_else(|| self.context.estimated_tokens());
        let result =
            self.compact_with_record(CompactionReason::Manual, tokens_before, cancellation)?;
        self.context.rebuild(&lock_writer(&self.session))?;
        Ok(result)
    }

    /// 带 durable step attempt 的压缩：摘要请求是 run operation 内的
    /// compaction step，attempt 由 [`AttemptLedger`] 先落盘再发送，重试时
    /// 每条实际出站请求对应一条连续 attempt。
    pub(super) fn compact_with_record(
        &mut self,
        reason: CompactionReason,
        tokens_before: u64,
        cancellation: &CancellationToken,
    ) -> std::result::Result<CompactionOutcome, crate::compaction::CompactionError> {
        let mut ledger = AttemptLedger::new(
            &self.session,
            &self.operation_id,
            StepKind::Compaction,
            Some(reason),
            &mut self.compaction_attempts,
        );
        self.compaction.compact(
            &mut ledger,
            self.context.entries(),
            &self.config.compaction,
            tokens_before,
            cancellation,
        )
    }

    /// 单个轮步：先经 `prepare_request` 装配请求（含发送前主动压缩），再交给
    /// 采样层发送。provider 明确返回 ContextOverflow 时强制压缩并基于压缩后的
    /// 会话重建请求；恢复预算是 turn 级单点（`overflow_recovery_used`）：一个
    /// turn 至多一次强制压缩重发，后续轮步再次溢出直接以原始根因失败。
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
        loop {
            match self.sample_request(&request, events, cancellation, model_turn_ordinal) {
                AttemptOutcome::Response(response, result_entry_id) => {
                    return AttemptOutcome::Response(response, result_entry_id);
                }
                AttemptOutcome::Aborted => return AttemptOutcome::Aborted,
                AttemptOutcome::Failed(error) => {
                    if matches!(
                        &error,
                        AgentError::Provider(provider)
                            if provider.error.is_context_overflow()
                    ) {
                        outcome.usage_complete = false;
                        if self.overflow_recovery_used {
                            return AttemptOutcome::Failed(error);
                        }
                        self.overflow_recovery_used = true;
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
                        match self.build_request(spec) {
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
        let appended = lock_writer(&self.session).append_message(message);
        if let Err(error) = appended {
            return Err(self.run_failed(AgentError::Session(error), outcome.clone()));
        }
        self.track_last_entry();
        Ok(())
    }

    /// 以预分配 id 追加会话消息（工具结果闭合 tool_started 的引用）。
    fn append_session_or_fail_with_id(
        &mut self,
        outcome: &mut AgentOutcome,
        id: &str,
        message: AgentMessage,
    ) -> Result<()> {
        let appended = lock_writer(&self.session).append_message_with_id(id, message);
        if let Err(error) = appended {
            return Err(self.run_failed(AgentError::Session(error), outcome.clone()));
        }
        self.track_last_entry();
        Ok(())
    }

    /// turn 内追加的条目并入上下文视图（模型可见历史与计量同步推进）。
    fn track_last_entry(&mut self) {
        if let Some(entry) = lock_writer(&self.session).entries().last().cloned() {
            self.context.append_entry(&entry);
        }
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
    fn abort_outcome(&mut self, mut outcome: AgentOutcome) -> Result<AgentOutcome> {
        outcome.terminal_reason = AgentTerminalReason::Aborted;
        outcome.usage_complete = false;
        lock_inbox(&self.inbox).close();
        Ok(outcome)
    }

    fn fail_after_progress(
        &mut self,
        error: AgentError,
        outcome: AgentOutcome,
    ) -> Result<AgentOutcome> {
        if is_cancelled_agent_error(&error) {
            return self.abort_outcome(outcome);
        }
        Err(self.run_failed(error, outcome))
    }

    /// 已积累 progress 的失败收敛：关闭注入箱；
    /// turns == 0 时原样返回根因，否则包装为 RunFailed。
    fn run_failed(&mut self, error: AgentError, mut outcome: AgentOutcome) -> AgentError {
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
mod loop_tests;
