//! Singularity 核心 Agent 执行循环：单一 Agent execution seam。
//!
//! 轮步编排驻留本文件：内层循环逐轮驱动，发送前基于 ContextView 的真实
//! usage 基线（缺失时用装配估算兜底）做主动压缩，调用采样层，并在 provider
//! 明确返回 ContextOverflow 时强制压缩重发——恢复预算按 turn 计，至多一次；
//! 再次溢出保留原始根因失败。外层循环在代理将要停止
//! 时消费停止窗口内到达的引导输入。
//!
//! 模型请求观测、消息与工具结果都由 SessionManager 追加到同一会话日志。工具
//! 结果落盘后才发布完成事件；恢复依据 assistant 的工具调用及后续结果闭合记录，
//! 绝不重放结果未知的副作用。
//!
//! 转向控制的接受、归还与取消只发生在 inbox 与 Conversation 的内存状态里：
//! 未消费的控制不落盘，只有它被消费成一条输入消息之后才属于持久历史。因此本
//! 模块不承诺、也不实现控制的日志恢复。
//!
//! 请求装配与压缩判定在 self::request；共用请求执行（attempt 循环、重试等待
//! 与账本记录）在 crate::request_execution；
//! 事件出口类型在 crate::events，turn 转向输入箱在 self::inbox。会话状态
//! 持久化、上下文压缩、工具注册分发与模型调用分别由 session/ facade、
//! compaction.rs、tools/ 与 singularity_model 模块提供支持。

mod inbox;
mod request;

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelConfigurationSnapshot, ModelToolSchema, ModelUsage, Provider, ProviderError,
};
use singularity_protocol::ControlDisposition;
use thiserror::Error;

pub use self::inbox::{ControlRequest, TurnInbox, TurnInboxHandle, control_id};
use crate::events::diagnostic_code;
pub use crate::events::{AgentDiagnostic, AgentEvent};
use crate::request_execution::{RequestAccounting, execute_request};

use self::inbox::lock_inbox;
use crate::compaction::{CompactionConfig, CompactionOutcome};
use crate::message::{
    AgentMessage, ItemScope, assistant_response_message, tool_result_message, user_message,
};
use crate::session::context::ContextView;
use crate::session::{LedgerRecord, SessionError, SessionWriter, lock_writer};
use crate::tools::batch::{PreparedToolCall, ToolBatchError, execute_tool_batch};
use crate::tools::{ToolRegistrySnapshot, error_result};

/// Agent 运行配置：一次 turn 冻结的提示词与模型/压缩事实。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// 文件指令的用户数据根；测试或无文件上下文的消费者可省略。
    pub instruction_home: Option<std::path::PathBuf>,
    /// 准备阶段已读取的首轮文件指令；缺失文件为 None，压缩后重新读取。
    pub initial_instructions: Option<singularity_core::ProjectInstructions>,
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
    #[error("agent operation aborted")]
    Aborted,
    #[error("{0}")]
    InvalidSummary(String),
    /// 文件指令读取失败：首轮加载与压缩后刷新共用同一来源，不因阶段不同改类。
    #[error("file instructions unavailable: {0}")]
    Instructions(String),
    /// 手动技能加载失败：技能正文同样是指令材料，与文件指令同源。
    #[error("skill unavailable: {0}")]
    SkillLoad(String),
    /// 上下文容量不足：请求与响应预算放不进当前窗口，不是程序内部故障。
    #[error("{0}")]
    ContextCapacity(String),
    /// 程序故障（如工具 worker panic）：不是可交给模型继续处理的业务失败，
    /// 调用方应停止本执行链并保留真实故障原因。
    #[error("host failure: {0}")]
    HostFailure(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 的终止原因。错误细节继续由 AgentError 携带，避免在 outcome 中
/// 复制第二套错误事实源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
}

/// 一次 run 的终态结果：只表达终止原因与截断。最终正文与轮数不再是返回结果
/// 的一部分——正文已随 assistant 消息落盘并经完成事件发布，轮数留在循环内部。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最终 assistant 响应是否因 provider 输出预算耗尽而截断。
    pub truncated: bool,
    pub terminal_reason: AgentTerminalReason,
}

/// 新 headless core 的 Agent：会话写者 + operation 范围 + compaction +
/// 工具注册表快照 + 模型提供方。
pub struct Agent {
    /// 共享会话写者：turn 执行、请求观测与工具结果追加共用同一 SessionManager
    /// 实例，各操作短暂加锁串行追加（lock_writer），绝不跨 provider/工具
    /// 调用持锁。控制的接受与归还发生在 inbox/Conversation 的内存状态，不写
    /// 会话日志；被消费成输入消息的内容才经同一实例落盘，不存在绕过
    /// SessionManager 的第二写者。
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    /// 本轮冻结的工具定义；请求装配与静态开销共用同一快照。
    tools: Vec<ModelToolSchema>,
    /// 系统提示词与冻结工具定义的静态 token 开销，只派生一次。
    request_overhead_tokens: u64,
    provider: Arc<dyn Provider + Send + Sync>,
    /// runtime 在 turn 边界解析并冻结的唯一模型配置事实。
    model: ModelConfigurationSnapshot,
    config: AgentConfig,
    /// 活动 turn 的实时转向输入箱；内存态不持久化。
    inbox: TurnInboxHandle,
    /// 请求前上下文规模的唯一计量（usage 基线 + 尾部增量）。
    context: ContextView,
    /// 本 operation 内全部生成、重试与摘要请求。
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
        let tools = registry.provider_schemas();
        let request_overhead_tokens =
            request::static_request_overhead_tokens(&config.system_prompt, &tools);
        Ok(Self {
            session,
            registry,
            tools,
            request_overhead_tokens,
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
    /// 运行中注入的转向输入在后续轮次生效；停止后返回终态结果。
    ///
    /// cancellation 取消时终止并返回 terminal_reason=Aborted（不视为错误）；
    /// 已完成内容仍以会话内容与完成事件为准，不由结果重复携带。
    pub fn run(
        &mut self,
        input: &str,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let result = self.run_loop(input, on_event, cancellation);
        lock_inbox(&self.inbox).close();
        result
    }

    fn run_loop(
        &mut self,
        input: &str,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let mut outcome = AgentOutcome {
            truncated: false,
            terminal_reason: AgentTerminalReason::Completed,
        };
        // 模型轮序号只用于请求记账：HTTP 重试与压缩请求不增加此计数。
        let mut turns = 0u32;
        let input_entry = self.append_message(None, user_message(input))?;
        on_event(AgentEvent::UserMessage {
            entry_id: input_entry,
            text: input.to_string(),
        });

        if self.config.instruction_home.is_some() {
            let loaded = self.config.initial_instructions.take();
            self.apply_instructions(loaded, on_event)?;
        }
        self.load_and_record_manual_skill(input)?;

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
                self.inject_controls(drained, on_event)?;
                let model_turn_ordinal = turns.saturating_add(1);
                let (response, assistant_result_entry_id) =
                    match self.run_turn(on_event, cancellation, model_turn_ordinal) {
                        Ok(response) => response,
                        Err(AgentError::Aborted) => return Ok(self.abort_outcome(outcome)),
                        Err(error) => return Err(error),
                    };
                turns += 1;
                let length_truncated = response.is_length_truncated();
                // usage 与终止原因不属于会话内容，随响应移出前取用；正文、思考与
                // 私有续接材料直接移动进消息。
                let usage = response.usage.clone();
                let assistant = assistant_response_message(response);
                self.context.record_usage(
                    &usage,
                    crate::session::context::message_token_estimate(&assistant),
                    self.request_overhead_tokens,
                );

                // 工具调用既随消息持久化、又交给执行器：落盘前取下执行侧的拥有
                // 副本，落盘后按原始调用顺序准备。公开投影先于移动形成。
                let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
                // 实时完成只需要正文与思考：工具生命周期由工具自己的事件表达。
                let public_items =
                    assistant.public_items(&assistant_result_entry_id, ItemScope::Completion);
                self.append_message(Some(&assistant_result_entry_id), assistant)?;
                on_event(AgentEvent::MessageFinished {
                    message_id: assistant_result_entry_id.clone(),
                    items: public_items,
                    failed: false,
                });
                if !tool_calls.is_empty() {
                    // 查找与参数解析按 source order 串行完成；未知工具/非法参数
                    // 只生成模型可见失败，不进入 worker。截断响应中的调用统一
                    // 准备为模型可见失败，绝不进入 preflight 或执行 worker。
                    let prepared_calls = tool_calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, call)| {
                            let prepared = if length_truncated {
                                Err(error_result(
                                    "tool execution failed: model output was truncated before the tool call completed",
                                ))
                            } else {
                                self.registry.preflight(&call.tool_name, &call.arguments)
                            };
                            PreparedToolCall {
                                call,
                                prepared,
                                result_entry_id: crate::session::tool_item_id(
                                    &assistant_result_entry_id,
                                    index,
                                ),
                            }
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
                        on_event,
                        &mut |prepared, execution| {
                            Self::append_to_context(&self.session, &mut self.context, |writer| {
                                writer.append_message_with_id(
                                    &prepared.result_entry_id,
                                    tool_result_message(&prepared.call.tool_call_id, execution),
                                )
                            })
                            .map(|_| ())
                        },
                    )
                    .map_err(|error| match error {
                        ToolBatchError::Commit(error) => AgentError::Session(error),
                        // 工具 worker 的宿主故障不是模型可纠正的业务失败：停止本
                        // 执行链，保留原因，不继续派发下一次模型请求。
                        ToolBatchError::HostFailure(message) => AgentError::HostFailure(message),
                    })?;
                    if length_truncated {
                        outcome.truncated = true;
                    }
                    if cancellation.is_cancelled() {
                        return Ok(self.abort_outcome(outcome));
                    }
                    continue;
                }
                // 无工具调用：本轮响应即最终轮，终止结果只保留截断标记。
                outcome.truncated = length_truncated;
                break;
            }
            // 代理将要停止：消费停止窗口内到达的转向输入后回到内层循环。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            self.inject_controls(pending_inputs, on_event)?;
        }
    }

    fn inject_controls(
        &mut self,
        requests: Vec<ControlRequest>,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let mut pending = requests.into_iter();
        while let Some(request) = pending.next() {
            let delivered = self.append_message(None, user_message(&request.text));
            let entry_id = match delivered {
                Ok(entry_id) => entry_id,
                Err(error) => {
                    lock_inbox(&self.inbox).restore(std::iter::once(request).chain(pending));
                    return Err(error);
                }
            };
            on_event(AgentEvent::UserMessage {
                entry_id,
                text: request.text.clone(),
            });
            on_event(AgentEvent::ControlChanged(
                request.snapshot(ControlDisposition::Injected),
            ));
            if let Err(error) = self.load_and_record_manual_skill(&request.text) {
                lock_inbox(&self.inbox).restore(pending);
                return Err(error);
            }
        }
        Ok(())
    }

    /// 无条件执行一次 compaction（provider 明确返回 context overflow 时使用）。
    fn force_compact(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let pruned = self.prune_tool_results(0, cancellation)?;
        match self.compact_with_record(0, on_event, cancellation) {
            Ok(CompactionOutcome::NotNeeded) => {
                // 账本没有变化：剪枝只在改动时重建视图，这里只需按压缩后的读法刷新指令。
                self.refresh_instructions(on_event)?;
                Ok(if pruned {
                    CompactionOutcome::Reduced
                } else {
                    CompactionOutcome::NotNeeded
                })
            }
            Ok(result) => Ok(result),
            // 只有允许跳过的摘要失败才降级为「已剪枝」；永久 provider 失败、
            // 取消与存储故障向上传播，不因剪枝成功就把已知错误改报 Reduced。
            Err(error) if pruned && request::compaction_may_be_skipped(&error) => {
                request::emit_compaction_skipped(on_event, &error);
                Ok(CompactionOutcome::Reduced)
            }
            Err(error) => Err(error),
        }
    }

    /// 手动压缩：跳过压力门槛，保留最后一个完整消息或工具单元。
    pub fn compact_now(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let result = self.compact_with_record(0, on_event, cancellation)?;
        if matches!(result, CompactionOutcome::NotNeeded) {
            // 没有摘要落盘，上下文不变；仍按压缩后的读法刷新指令。
            self.refresh_instructions(on_event)?;
        }
        Ok(result)
    }

    /// 单个轮步：先经 prepare_request 装配请求（含发送前主动压缩），再交给
    /// 采样层发送。provider 明确返回 ContextOverflow 时强制压缩并基于压缩后的
    /// 会话重建请求；恢复预算是 turn 级单点（overflow_recovery_used）：一个
    /// turn 至多一次强制压缩重发，后续轮步再次溢出直接以原始根因失败。
    fn run_turn(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> Result<(singularity_model::ModelTurnResponse, String)> {
        let mut request = self.prepare_request(on_event, cancellation)?;
        loop {
            let error = match execute_request(
                self.provider.as_ref(),
                &self.session,
                &mut self.accounting,
                &mut request,
                on_event,
                cancellation,
                model_turn_ordinal,
                singularity_protocol::RequestPurpose::Generation,
            ) {
                Ok(response) => return Ok(response),
                Err(AgentError::Provider(provider)) if provider.is_context_overflow() => provider,
                Err(error) => return Err(error),
            };
            if self.overflow_recovery_used {
                return Err(AgentError::Provider(error));
            }
            self.overflow_recovery_used = true;
            match self.force_compact(on_event, cancellation) {
                Ok(CompactionOutcome::NotNeeded) => return Err(AgentError::Provider(error)),
                Ok(CompactionOutcome::Reduced) => {}
                Err(AgentError::Aborted) => return Err(AgentError::Aborted),
                Err(recovery_error) => {
                    // 恢复终止的真实原因不再被最初的 overflow 覆盖：同一个失败
                    // 结果里同时保留「最初是溢出」与「恢复为何失败」，诊断也直接
                    // 透传已有有界原因。
                    on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                        diagnostic_code::CONTEXT_OVERFLOW_RECOVERY_FAILED,
                        format!("context overflow recovery failed: {recovery_error}"),
                    )));
                    if matches!(recovery_error, AgentError::Session(_)) {
                        return Err(recovery_error);
                    }
                    return Err(AgentError::Provider(overflow_recovery_failure(
                        error,
                        &recovery_error,
                    )));
                }
            }
            if let Err(room_error) = self.ensure_response_room() {
                return Err(AgentError::Provider(overflow_recovery_failure(
                    error,
                    &room_error,
                )));
            }
            request = self.build_request();
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

    /// 保持写者上锁，直到追加的条目进入上下文：锁内保证这里追加的条目就是随后
    /// 被上下文吸收的同一尾条目。控制输入先进入 inbox，随后也走这条追加路径。
    fn append_to_context(
        session: &SessionWriter,
        context: &mut ContextView,
        append: impl FnOnce(
            &mut crate::session::SessionManager,
        ) -> std::result::Result<String, SessionError>,
    ) -> std::result::Result<String, SessionError> {
        let mut writer = lock_writer(session);
        let entry_id = append(&mut writer)?;
        context.append_entry(&writer, writer.entries().len() - 1)?;
        Ok(entry_id)
    }

    /// 标记中止原因；实际用量始终由请求 accounting 维护。
    fn abort_outcome(&self, mut outcome: AgentOutcome) -> AgentOutcome {
        outcome.terminal_reason = AgentTerminalReason::Aborted;
        outcome
    }
    /// 实测请求用量，包含被拒绝的摘要与失败的尝试。
    pub fn request_usage(&self) -> (&ModelUsage, bool) {
        (&self.accounting.usage, self.accounting.complete)
    }
}

/// 恢复终止的失败报告：最初的 context overflow 与实际导致恢复终止的原因在
/// 同一个结果里各自保留，后者不再被前者覆盖。
fn overflow_recovery_failure(
    overflow: ProviderError,
    recovery_error: &AgentError,
) -> ProviderError {
    ProviderError {
        message: format!(
            "{}; context overflow recovery failed: {recovery_error}",
            overflow.message
        ),
        ..overflow
    }
}

#[cfg(test)]
mod tests;
