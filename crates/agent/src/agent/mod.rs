//! Singularity 核心 Agent 执行循环：单一 Agent execution seam。
//!
//! 轮步编排驻留本文件：内层循环逐轮驱动，发送前基于 ContextView 的真实
//! usage 基线（缺失时用装配估算兜底）做主动压缩，调用采样层，并在 provider
//! 明确返回 ContextOverflow 时强制压缩重发——恢复预算按 turn 计，至多一次；
//! 再次溢出保留原始根因失败。外层循环在代理将要停止
//! 时消费停止窗口内到达的引导输入。
//!
//! 模型请求观测、消息、工具结果和转向控制写入同一会话日志。工具结果落盘后
//! 才发布完成事件；恢复依据 assistant 的工具调用及后续结果闭合记录，
//! 绝不重放结果未知的副作用。
//!
//! 请求装配与压缩判定在 self::request；共用请求执行在 crate::request_execution；
//! 事件出口类型在 crate::events，turn 转向输入箱在 self::inbox。会话状态
//! 持久化、上下文压缩、工具注册分发与模型调用分别由 session/ facade、
//! compaction.rs、tools/ 与 singularity_model 模块提供支持。

mod inbox;
mod request;

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelToolSchema, ModelUsage, Provider,
    ProviderError,
};
use thiserror::Error;

pub use self::inbox::{TurnInbox, TurnInboxHandle};
use crate::events::diagnostic_code;
pub use crate::events::{AgentDiagnostic, AgentEvent, AgentEvents};
pub(crate) use crate::events::{emit, emit_diagnostic};
use crate::request_execution::{RequestAccounting, RequestExecutionError};

use self::inbox::lock_inbox;
use self::request::AttemptOutcome;
use crate::compaction::{CompactionConfig, CompactionOutcome};
use crate::message::{
    AgentMessage, ContentBlock, assistant_response_message, tool_result_message, user_message,
};
use crate::session::context::ContextView;
use crate::session::{ControlDisposition, LedgerRecord, SessionError, SessionWriter, lock_writer};
use crate::tools::batch::{PreparedToolCall, execute_tool_batch};
use crate::tools::{ToolRegistrySnapshot, error_result};

/// Agent 运行配置：一次 turn 冻结的提示词与模型/压缩事实。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// 文件指令的用户数据根；测试或无文件上下文的消费者可省略。
    pub instruction_home: Option<std::path::PathBuf>,
    /// 自动压缩的窗口占用阈值与近期历史保留比例。
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
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 的终止原因。错误细节继续由 AgentError 携带，避免在 outcome 中
/// 复制第二套错误事实源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
}

/// 一次 run 的最终结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最后一次无工具调用的 assistant 文本（中断时可能为空）。
    pub final_text: String,
    /// 最终 assistant 响应是否因 provider 输出预算耗尽而截断。
    pub truncated: bool,
    pub turns: u32,
    pub terminal_reason: AgentTerminalReason,
}

fn is_cancelled_agent_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::Provider(provider) if provider.kind == ModelErrorKind::Cancelled
    ) || matches!(
        error,
        AgentError::Compaction(crate::compaction::CompactionError::Aborted)
    )
}

/// 新 headless core 的 Agent：会话写者 + operation 范围 + compaction +
/// 工具注册表快照 + 模型提供方。
pub struct Agent {
    /// 共享会话写者：turn 执行与控制面共用同一 SessionManager
    /// 实例，各操作短暂加锁串行追加（lock_writer），绝不跨 provider/工具
    /// 调用持锁。控制接受与执行追加经同一实例落盘，不存在绕过
    /// SessionManager 的第二写者。
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    provider: Arc<dyn Provider + Send + Sync>,
    /// runtime 在 turn 边界解析并冻结的唯一模型配置事实。
    model: ModelConfigurationSnapshot,
    config: AgentConfig,
    /// 活动 turn 的实时转向输入箱；内存态不持久化。
    inbox: TurnInboxHandle,
    /// 请求前上下文规模的唯一计量（usage 基线 + 尾部增量）。
    context: ContextView,
    /// All generation, retry and summary requests in this operation.
    accounting: RequestAccounting,
    /// 本 turn 的强制溢出恢复预算：至多一次。
    /// 每次 run 恰好一个 turn；预算随 turn 起落，绝不跨 turn 携带。
    overflow_recovery_used: bool,
}

impl Agent {
    /// inbox 是本 Agent 的实时转向输入箱句柄：由生命周期所有者构造
    /// 控制面时创建并绑定，使注入窗口在 turn 开始前即已就绪。
    pub fn new(
        inbox: TurnInboxHandle,
        provider: Arc<dyn Provider + Send + Sync>,
        model: ModelConfigurationSnapshot,
        mut registry: ToolRegistrySnapshot,
        config: AgentConfig,
        session: SessionWriter,
    ) -> Result<Self> {
        let context = ContextView::derive(&lock_writer(&session))?;
        if let Some(home) = &config.instruction_home {
            let cwd = lock_writer(&session).cwd().to_path_buf();
            registry.skills = singularity_core::skills::SkillCatalog::discover(&cwd, home);
        }
        Ok(Self {
            session,
            registry,
            provider,
            model,
            config,
            inbox,
            context,
            accounting: RequestAccounting::default(),
            overflow_recovery_used: false,
        })
    }

    fn append_record(&mut self, record: LedgerRecord) -> std::result::Result<String, SessionError> {
        Self::append_to_context(&self.session, &mut self.context, |writer| {
            writer.append_record(record)
        })
    }

    /// 运行一个完整 Agent 循环：输入持久化为 user 消息，内层循环处理工具调用，
    /// 运行中注入的转向输入在后续轮次生效；停止后返回聚合结果。
    ///
    /// cancellation 取消时终止并返回已完成文本（terminal_reason=Aborted，不视为错误）。
    pub fn run(
        &mut self,
        input: &str,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let result = self.run_loop(input, events, cancellation);
        lock_inbox(&self.inbox).close();
        result
    }

    fn run_loop(
        &mut self,
        input: &str,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let mut outcome = AgentOutcome {
            final_text: String::new(),
            truncated: false,
            turns: 0,
            terminal_reason: AgentTerminalReason::Completed,
        };
        let input_entry = self.append_message(None, user_message(input))?;
        crate::events::emit(
            events,
            AgentEvent::UserMessage {
                entry_id: input_entry,
                text: input.to_string(),
            },
        );

        self.refresh_instructions(events)?;
        self.load_manual_skill(input)?;

        let tools = self.registry.provider_schemas();

        // 外层循环：代理将要停止时消费停止前到达的转向输入。
        loop {
            // 内层循环：工具调用与 steer 注入。
            loop {
                if cancellation.is_cancelled() {
                    return Ok(self.abort_outcome(outcome));
                }
                // 注入转向队列全部消息（作为 user 消息追加到本轮上下文），
                // 按接受顺序保存为用户消息，再通知输入已消费。
                let drained = lock_inbox(&self.inbox).drain();
                self.inject_controls(drained, events)?;
                let model_turn_ordinal = outcome.turns.saturating_add(1);
                let (response, assistant_result_entry_id) =
                    match self.run_turn(&tools, events, cancellation, model_turn_ordinal) {
                        AttemptOutcome::Response(response, result_entry_id) => {
                            (*response, result_entry_id)
                        }
                        AttemptOutcome::Aborted => return Ok(self.abort_outcome(outcome)),
                        AttemptOutcome::Failed(error) => {
                            return if is_cancelled_agent_error(&error) {
                                Ok(self.abort_outcome(outcome))
                            } else {
                                Err(error)
                            };
                        }
                    };
                outcome.turns += 1;
                let assistant = assistant_response_message(&response);
                self.context.record_usage(
                    &response.usage,
                    crate::session::context::message_token_estimate(&assistant),
                    self.request_overhead_tokens(),
                );

                let assistant_text = response.assistant_message.content.clone();
                let tool_calls = response.tool_calls().to_vec();
                let length_truncated = response.is_length_truncated();
                self.append_message(Some(&assistant_result_entry_id), assistant.clone())?;
                Self::emit_assistant_finished(&assistant_result_entry_id, &assistant, events);
                if length_truncated && !tool_calls.is_empty() {
                    // 截断的响应可能含有仅部分解析的工具调用。持久化 assistant
                    // 消息并为每个调用生成模型可见失败，但绝不执行这些调用或将
                    // 它们显示为成功的工具事件。
                    for call in &tool_calls {
                        self.append_message(
                            None,
                            tool_result_message(
                                &call.tool_call_id,
                                &call.tool_name,
                                &error_result(
                                    "tool execution failed: model output was truncated before the tool call completed",
                                ),
                            ),
                        )?;
                    }
                    outcome.truncated = true;
                    outcome.final_text = assistant_text;
                    continue;
                }
                if !tool_calls.is_empty() {
                    // 查找与参数解析按 source order 串行完成；未知工具/非法参数
                    // 只生成模型可见失败，不进入 worker。
                    let prepared_calls = tool_calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| PreparedToolCall {
                            call: call.clone(),
                            prepared: self.registry.preflight(&call.tool_name, &call.arguments),
                            result_entry_id: crate::session::tool_item_id(
                                &assistant_result_entry_id,
                                index,
                            ),
                        })
                        .collect::<Vec<_>>();

                    // 会话写者锁只用于读取 cwd，随即释放——绝不在工具执行期间
                    // 持有（工具 worker 与控制面共享同一写者，跨工具执行持锁
                    // 会阻塞控制接受与终态落盘）。
                    let cwd = lock_writer(&self.session).cwd().to_path_buf();
                    execute_tool_batch(
                        &prepared_calls,
                        &cwd,
                        cancellation,
                        events,
                        &mut |prepared, execution| {
                            Self::append_to_context(&self.session, &mut self.context, |writer| {
                                writer.append_message_with_id(
                                    &prepared.result_entry_id,
                                    tool_result_message(
                                        &prepared.call.tool_call_id,
                                        &prepared.call.tool_name,
                                        execution,
                                    ),
                                )
                            })
                            .map(|_| ())
                        },
                    )?;
                    if cancellation.is_cancelled() {
                        return Ok(self.abort_outcome(outcome));
                    }
                    continue;
                }
                // 无工具调用：记录最终文本并退出内层循环。
                outcome.final_text = assistant_text;
                outcome.truncated = length_truncated;
                break;
            }
            // 代理将要停止：消费停止窗口内到达的转向输入后回到内层循环。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            self.inject_controls(pending_inputs, events)?;
        }
    }

    fn inject_controls(
        &mut self,
        requests: Vec<crate::session::ControlRequest>,
        events: &mut AgentEvents,
    ) -> Result<()> {
        let mut pending = requests.into_iter();
        while let Some(request) = pending.next() {
            let text = request.text.as_deref().unwrap_or_default();
            let delivered = self.append_message(None, user_message(text));
            let entry_id = match delivered {
                Ok(entry_id) => entry_id,
                Err(error) => {
                    lock_inbox(&self.inbox).restore(std::iter::once(request).chain(pending));
                    return Err(error);
                }
            };
            crate::events::emit(
                events,
                AgentEvent::UserMessage {
                    entry_id,
                    text: text.to_string(),
                },
            );
            crate::events::emit(
                events,
                AgentEvent::ControlChanged(request.snapshot(ControlDisposition::Injected)),
            );
            if let Err(error) = self.load_manual_skill(text) {
                lock_inbox(&self.inbox).restore(pending);
                return Err(error);
            }
        }
        Ok(())
    }

    /// 无条件执行一次 compaction（provider 明确返回 context overflow 时使用）。
    fn force_compact(
        &mut self,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let pruned = self.prune_tool_results(0, cancellation)?;
        let tokens_before = self.context_pressure_tokens();
        match self.compact_with_record(tokens_before, 0, events, cancellation) {
            Ok(result) => {
                self.context.rebuild(&lock_writer(&self.session))?;
                self.refresh_instructions(events)?;
                Ok(
                    if pruned && matches!(result, CompactionOutcome::NotNeeded) {
                        CompactionOutcome::Reduced
                    } else {
                        result
                    },
                )
            }
            Err(crate::compaction::CompactionError::Session(error)) => {
                Err(AgentError::Session(error))
            }
            Err(error)
                if pruned && !matches!(error, crate::compaction::CompactionError::Aborted) =>
            {
                request::emit_compaction_skipped(events, &error);
                Ok(CompactionOutcome::Reduced)
            }
            Err(error) => Err(AgentError::Compaction(error)),
        }
    }

    /// 手动压缩：跳过压力门槛，保留最后一个完整消息或工具单元。
    pub fn compact_now(
        &mut self,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let tokens_before = self.context_pressure_tokens();
        let result = self.compact_with_record(tokens_before, 0, events, cancellation)?;
        self.context.rebuild(&lock_writer(&self.session))?;
        self.refresh_instructions(events)?;
        Ok(result)
    }

    /// 单个轮步：先经 prepare_request 装配请求（含发送前主动压缩），再交给
    /// 采样层发送。provider 明确返回 ContextOverflow 时强制压缩并基于压缩后的
    /// 会话重建请求；恢复预算是 turn 级单点（overflow_recovery_used）：一个
    /// turn 至多一次强制压缩重发，后续轮步再次溢出直接以原始根因失败。
    fn run_turn(
        &mut self,
        tools: &[ModelToolSchema],
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        let mut request = match self.prepare_request(tools, events, cancellation) {
            Ok(request) => request,
            Err(error) => return AttemptOutcome::Failed(error),
        };
        loop {
            match self.execute_request(
                &mut request,
                events,
                cancellation,
                model_turn_ordinal,
                singularity_protocol::RequestPurpose::Generation,
            ) {
                Ok((response, result_entry_id)) => {
                    return AttemptOutcome::Response(response, result_entry_id);
                }
                Err(RequestExecutionError::Aborted) => return AttemptOutcome::Aborted,
                Err(RequestExecutionError::Session(error)) => {
                    return AttemptOutcome::Failed(AgentError::Session(error));
                }
                Err(RequestExecutionError::Provider(provider)) => {
                    let error = AgentError::Provider(provider);
                    if matches!(
                        &error,
                        AgentError::Provider(provider)
                            if provider.is_context_overflow()
                    ) {
                        if self.overflow_recovery_used {
                            return AttemptOutcome::Failed(error);
                        }
                        self.overflow_recovery_used = true;
                        match self.force_compact(events, cancellation) {
                            Ok(CompactionOutcome::NotNeeded) => {
                                return AttemptOutcome::Failed(error);
                            }
                            Ok(_) => {}
                            Err(AgentError::Compaction(
                                crate::compaction::CompactionError::Aborted,
                            )) => {
                                return AttemptOutcome::Aborted;
                            }
                            Err(recovery_error) => {
                                // 无有效缩减时保留提供方原始溢出根因；存储失败仍 fail-stop。
                                emit_diagnostic(
                                    events,
                                    AgentDiagnostic::warning(
                                        diagnostic_code::CONTEXT_OVERFLOW_RECOVERY_FAILED,
                                        "forced compaction failed to recover from context overflow"
                                            .to_string(),
                                    ),
                                );
                                if matches!(recovery_error, AgentError::Session(_)) {
                                    return AttemptOutcome::Failed(recovery_error);
                                }
                                return AttemptOutcome::Failed(error);
                            }
                        }
                        // 强制压缩只修改了 self.session；重试必须基于压缩后的
                        // 会话重新装配请求，否则仍携带被拒绝的超限上下文。
                        if self.ensure_response_room().is_err() {
                            return AttemptOutcome::Failed(error);
                        }
                        request = self.build_request(tools);
                        continue;
                    }
                    return AttemptOutcome::Failed(error);
                }
            }
        }
    }

    /// 持久化消息后推进上下文，返回持久条目 id；写入失败保留原始 session
    /// 错误。id 为 Some 时沿用模型请求预分配的结果条目 id。
    fn append_message(&mut self, id: Option<&str>, message: AgentMessage) -> Result<String> {
        Self::append_to_context(&self.session, &mut self.context, |writer| match id {
            Some(id) => writer.append_message_with_id(id, message),
            None => writer.append_message(message),
        })
        .map_err(AgentError::Session)
    }

    /// Keep the writer locked until its appended entry reaches the context;
    /// control writes must not replace the last entry between these operations.
    fn append_to_context(
        session: &SessionWriter,
        context: &mut ContextView,
        append: impl FnOnce(
            &mut crate::session::SessionManager,
        ) -> std::result::Result<String, SessionError>,
    ) -> std::result::Result<String, SessionError> {
        let mut writer = lock_writer(session);
        let entry_id = append(&mut writer)?;
        if let Some(entry) = writer.entries().last()
            && crate::session::context::is_context_entry(entry)
        {
            context.append_entry(entry);
        }
        Ok(entry_id)
    }

    /// 持久化后的 assistant 消息内的思考块作为事实上报：每块一条事件，
    /// 供客户端实时展示，替代持久层回查。
    fn emit_assistant_finished(message_id: &str, message: &AgentMessage, events: &mut AgentEvents) {
        for block in message.thinking_blocks() {
            if let ContentBlock::Thinking { thinking, .. } = block
                && !thinking.trim().is_empty()
            {
                emit(
                    events,
                    AgentEvent::Thinking {
                        message_id: message_id.to_string(),
                        text: thinking.clone(),
                    },
                );
            }
        }
        emit(
            events,
            AgentEvent::MessageFinished {
                message_id: message_id.to_string(),
                failed: false,
            },
        );
    }

    /// 标记中止原因；实际用量始终由请求 accounting 维护。
    fn abort_outcome(&self, mut outcome: AgentOutcome) -> AgentOutcome {
        outcome.terminal_reason = AgentTerminalReason::Aborted;
        outcome
    }

    /// Measured request usage, including rejected summaries and failed attempts.
    pub fn request_usage(&self) -> (&ModelUsage, bool) {
        (&self.accounting.usage, self.accounting.complete)
    }
}

#[cfg(test)]
mod tests;
