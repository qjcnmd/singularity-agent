//! Agent 的核心执行循环：模型调用、工具执行与转向输入都汇聚在这里。
//!
//! 循环分两层：内层循环驱动每个轮步（组装请求 → 调用 provider → 执行工具 → 下一轮）；
//! 外层循环在模型准备停下来时，把停止窗口内新到的转向输入注入进去，再回到内层。
//!
//! 上下文压缩有两个触发点：发送前按 ContextView 的真实 usage 基线主动压缩（基线缺失时
//! 用装配阶段的估算兜底）；provider 明确返回 ContextLengthExceeded 时强制压缩后重发。
//! 重发机会每个轮步只有一次，第二次仍然溢出就保留最初的失败原因。
//!
//! 模型请求观测、消息与工具结果都经 SessionManager 追加到同一份会话日志，工具结果落盘后
//! 才发布完成事件；崩溃恢复只依据 assistant 的工具调用和对应的结果记录，绝不重放结果未知
//! 的副作用。转向控制只存在于 inbox 和 Conversation 的内存状态里，不落盘，因此本模块不
//! 实现控制的日志恢复。
//!
//! 相关模块：请求装配与压缩判定在 self::request，共用的请求执行在 crate::request_execution，
//! 事件类型在 crate::events，转向输入箱在 self::inbox。

mod inbox;
mod request;

use std::sync::Arc;

use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelToolSchema, ModelUsage, Provider, ProviderError,
};
use singularity_protocol::ControlDisposition;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

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

/// Agent 的运行配置：一次 turn 内冻结不变的提示词与模型/压缩事实。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub developer_instructions: String,
    /// 文件指令（AGENTS.md 等）的用户数据根目录；测试等没有文件上下文的消费者可以不设。
    pub instruction_home: Option<std::path::PathBuf>,
    /// 准备阶段已经读好的首轮文件指令；文件不存在时为 None，每次压缩后重新读取。
    pub initial_instructions: Option<singularity_core::ProjectInstructions>,
    /// 自动压缩的触发阈值，以及压缩后保留多少近期历史。
    pub compaction: CompactionConfig,
}

/// Agent 循环可能返回的错误。
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
    /// 文件指令读取失败。首轮加载与压缩后刷新共用这一个来源。
    #[error("file instructions unavailable: {0}")]
    Instructions(String),
    /// 手动选择的技能加载失败。技能正文同样是指令材料，因此与文件指令归为一类。
    #[error("skill unavailable: {0}")]
    SkillLoad(String),
    /// 程序故障（例如工具 worker panic）：不能交给模型继续处理，调用方应停止整条执行链。
    #[error("host failure: {0}")]
    HostFailure(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 的终止原因。错误细节仍由 AgentError 携带，不在 outcome 里再复制一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
}

/// 一次 run 的终态：只说明为什么停下来、有没有被截断。正文已随 assistant 消息落盘并经
/// 完成事件发布，轮数只在循环内部使用。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 最终 assistant 响应是否因为 provider 输出预算耗尽而被截断。
    pub truncated: bool,
    pub terminal_reason: AgentTerminalReason,
}

/// Agent：持有本次执行需要的会话写者、operation 范围、压缩配置、工具注册表快照与模型提供方。
pub struct Agent {
    /// 共享的会话写者：turn 执行、请求观测与工具结果追加都走同一个 SessionManager，
    /// 各自短暂加锁串行追加（lock_writer），绝不跨 provider 调用或工具执行持锁。
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    /// 本轮冻结的工具定义。请求装配与静态开销估算共用这一份快照。
    tools: Vec<ModelToolSchema>,
    /// 当前 Harness、Skill 目录与冻结工具定义的 token 开销；目录刷新后重算。
    request_static_tokens: u64,
    /// 本轮全局与项目文件指令；压缩后直接用重新读取的内容替换。
    file_instructions: Option<ModelMessage>,
    provider: Arc<dyn Provider + Send + Sync>,
    /// runtime 在 turn 边界解析并冻结的模型配置，是本次执行唯一的模型事实。
    model: ModelConfigurationSnapshot,
    config: AgentConfig,
    /// 当前 turn 的转向输入箱，只存在于内存，不持久化。
    inbox: TurnInboxHandle,
    /// 请求前上下文规模的唯一计量口径（usage 基线 + 尾部增量）。
    context: ContextView,
    /// 本 operation 内所有生成、重试与摘要请求的用量累计。
    accounting: RequestAccounting,
}

impl Agent {
    /// 构造 Agent；inbox 由生命周期所有者建立控制面时创建并绑定，使注入窗口在 turn
    /// 开始之前就已经就绪。
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
        let request_static_tokens = request::static_request_overhead_tokens(
            &config.developer_instructions,
            &registry.skills.prompt(),
            &tools,
        );
        Ok(Self {
            session,
            registry,
            tools,
            request_static_tokens,
            file_instructions: None,
            provider,
            model,
            config,
            inbox,
            context,
            accounting: RequestAccounting::default(),
        })
    }

    fn append_record(&mut self, record: LedgerRecord) -> std::result::Result<String, SessionError> {
        Self::append_to_context(&self.session, &mut self.context, |writer| {
            writer.append_record(record)
        })
    }

    /// 跑完一个完整的 Agent 循环：把输入持久化为 user 消息，内层循环处理工具调用，
    /// 运行期间注入的转向输入在后续轮次生效，直到模型停下来。
    ///
    /// 取消时返回 terminal_reason=Aborted（取消不算错误）；已经生成的内容以会话内容
    /// 和完成事件为准，不由返回值重复携带。
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
        // 模型轮序号只用于请求记账；HTTP 重试与压缩请求都不增加这个计数。
        let mut turns = 0u32;
        let input_entry = self.append_message(None, user_message(input))?;
        on_event(AgentEvent::UserMessage {
            entry_id: input_entry,
            text: input.to_string(),
        });

        if self.config.instruction_home.is_some() {
            let loaded = self.config.initial_instructions.take();
            self.apply_instructions(loaded, on_event);
        }
        self.load_and_record_manual_skill(input)?;

        // 外层循环：模型准备停下来时，先消费停止窗口内到达的转向输入。
        loop {
            // 内层循环：一次轮步的模型调用与工具执行。
            loop {
                if cancellation.is_cancelled() {
                    return Ok(self.abort_outcome(outcome));
                }
                // 把转向队列里的消息全部注入：按接受顺序追加为 user 消息，成功后再通知已消费。
                let drained = lock_inbox(&self.inbox).drain();
                self.inject_controls(drained, on_event)?;
                let model_turn_ordinal = turns.saturating_add(1);
                let (response, assistant_result_entry_id) =
                    match self.run_turn(on_event, cancellation, model_turn_ordinal) {
                        Ok(response) => response,
                        // 取消不是失败：返回中止终态，不返回错误。
                        Err(AgentError::Aborted) => return Ok(self.abort_outcome(outcome)),
                        Err(error) => return Err(error),
                    };
                turns += 1;
                let length_truncated = response.is_length_truncated();
                // usage 与终止原因不属于会话内容，在响应被移出前先取用。
                let usage = response.usage.clone();
                let assistant = assistant_response_message(response);
                let overhead_tokens = self.request_overhead_tokens();
                self.context.record_usage(
                    &usage,
                    crate::session::context::message_token_estimate(&assistant),
                    overhead_tokens,
                );

                // 工具调用既要随消息持久化、又要交给执行器：落盘前先取出执行侧的副本。
                let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
                // 实时完成事件只需要正文与思考；工具的生命周期由工具自己的事件表达。
                let public_items =
                    assistant.public_items(&assistant_result_entry_id, ItemScope::Completion);
                self.append_message(Some(&assistant_result_entry_id), assistant)?;
                on_event(AgentEvent::MessageFinished {
                    message_id: assistant_result_entry_id.clone(),
                    items: public_items,
                    failed: false,
                });
                if !tool_calls.is_empty() {
                    // 查找与参数解析按 source order 串行完成。未知工具、非法参数只生成模型
                    // 可见的失败；被截断的响应中的调用一律准备为模型可见失败，不进入 worker。
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

                    // 只为读 cwd 短暂持有会话写者锁；绝不跨工具执行持锁，否则会阻塞控制
                    // 接受与终态落盘（工具 worker 与控制面共用同一写者）。
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
                        // 工具 worker 的宿主故障不是模型能纠正的业务失败：停止整条执行链。
                        ToolBatchError::HostFailure(message) => AgentError::HostFailure(message),
                    })?;
                    // 还要继续下一轮：截断标记先记下，否则会被后续轮覆盖。
                    if length_truncated {
                        outcome.truncated = true;
                    }
                    if cancellation.is_cancelled() {
                        return Ok(self.abort_outcome(outcome));
                    }
                    continue;
                }
                // 没有工具调用：本轮响应就是最终轮，结果只保留截断标记。
                outcome.truncated = length_truncated;
                break;
            }
            // 模型准备停下来：把停止窗口内到达的转向输入注入后回到内层循环。
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

    /// 强制压缩一次（provider 明确返回 context overflow 时使用）。
    fn force_compact(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let pruned = self.prune_tool_results(cancellation)?;
        match self.compact_with_record(0, on_event, cancellation) {
            Ok(CompactionOutcome::NotNeeded) => {
                // 账本没有变化：剪枝只在真的改动时才重建视图，这里只需刷新一次指令。
                self.refresh_instructions(on_event)?;
                Ok(if pruned {
                    CompactionOutcome::Reduced
                } else {
                    CompactionOutcome::NotNeeded
                })
            }
            Ok(result) => Ok(result),
            // 只有允许跳过的摘要失败才降级为「已剪枝」；永久 provider 失败、取消与存储
            // 故障照旧向上传播，不能因为剪枝成功就把已知错误改报成 Reduced。
            Err(error) if pruned && request::compaction_may_be_skipped(&error) => {
                request::emit_compaction_skipped(on_event, &error);
                Ok(CompactionOutcome::Reduced)
            }
            Err(error) => Err(error),
        }
    }

    /// 手动压缩：跳过压力阈值判断，保留最后一个完整消息或工具单元。
    pub fn compact_now(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        if self.config.instruction_home.is_some() {
            let loaded = self.config.initial_instructions.take();
            self.apply_instructions(loaded, on_event);
        }
        let result = self.compact_with_record(0, on_event, cancellation)?;
        if matches!(result, CompactionOutcome::NotNeeded) {
            // 没有摘要落盘，上下文没有变化；仍按压缩后的读法刷新一次指令。
            self.refresh_instructions(on_event)?;
        }
        Ok(result)
    }

    /// 单个轮步：先用 prepare_request 组装请求（含发送前的主动压缩），再交给 provider 发送。
    /// provider 明确返回 ContextLengthExceeded 时强制压缩并重建请求，恢复机会至多一次。
    fn run_turn(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> Result<(singularity_model::ModelTurnResponse, String)> {
        let mut request = self.prepare_request(on_event, cancellation)?;
        let mut recovered = false;
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
                // 只有上下文溢出才值得压缩后重发，其余 provider 错误直接失败。
                Err(AgentError::Provider(provider)) if provider.is_context_overflow() => provider,
                Err(error) => return Err(error),
            };
            // 每个轮步只恢复一次：第二次溢出保留最初的失败原因。
            if recovered {
                return Err(AgentError::Provider(error));
            }
            recovered = true;
            match self.force_compact(on_event, cancellation) {
                // 没有可压缩的内容，恢复不了：保留最初的溢出失败。
                Ok(CompactionOutcome::NotNeeded) => return Err(AgentError::Provider(error)),
                // 压缩生效：用压缩后的历史重建请求再发一次。
                Ok(CompactionOutcome::Reduced) => {}
                Err(AgentError::Aborted) => return Err(AgentError::Aborted),
                Err(recovery_error) => {
                    // 恢复失败的真实原因不被最初的 overflow 覆盖：诊断直接透传它。
                    on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                        diagnostic_code::CONTEXT_OVERFLOW_RECOVERY_FAILED,
                        format!("context overflow recovery failed: {recovery_error}"),
                    )));
                    return Err(overflow_recovery_failure(&error, recovery_error));
                }
            }
            request = self.build_request();
        }
    }

    /// 持久化一条消息并推进上下文，返回持久条目 id；id 为 Some 时沿用预分配的结果条目 id。
    fn append_message(&mut self, id: Option<&str>, message: AgentMessage) -> Result<String> {
        Self::append_to_context(&self.session, &mut self.context, |writer| match id {
            Some(id) => writer.append_message_with_id(id, message),
            None => writer.append_message(message),
        })
        .map_err(AgentError::Session)
    }

    /// 追加期间一直持写者锁，直到新条目被上下文吸收：锁内保证追加的条目就是随后被上下文
    /// 吸收的同一条尾条目。控制输入先进入 inbox，之后也走这条追加路径。
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

    /// 标记中止原因。
    fn abort_outcome(&self, mut outcome: AgentOutcome) -> AgentOutcome {
        outcome.terminal_reason = AgentTerminalReason::Aborted;
        outcome
    }
    /// 实测请求用量，包含被拒绝的摘要与失败的尝试。
    pub fn request_usage(&self) -> (&ModelUsage, bool) {
        (&self.accounting.usage, self.accounting.complete)
    }
}

/// 恢复失败时的错误报告：保留恢复失败的真实类型与字段，最初的 context overflow 只作为
/// 错误文字进入 message，不覆盖 kind/code/retry_after。取消已在上游单独返回；Session 与
/// HostFailure 是执行链的 fail-stop 出口，三者都原样透传。
fn overflow_recovery_failure(overflow: &ProviderError, recovery_error: AgentError) -> AgentError {
    let with_overflow_context = |detail: &str| {
        format!(
            "{}; context overflow recovery failed: {detail}",
            overflow.message
        )
    };
    match recovery_error {
        AgentError::Provider(mut provider) => {
            provider.message = with_overflow_context(&provider.message);
            AgentError::Provider(provider)
        }
        AgentError::Instructions(detail) => {
            AgentError::Instructions(with_overflow_context(&detail))
        }
        AgentError::SkillLoad(detail) => AgentError::SkillLoad(with_overflow_context(&detail)),
        AgentError::InvalidSummary(detail) => {
            AgentError::InvalidSummary(with_overflow_context(&detail))
        }
        passthrough @ (AgentError::Aborted
        | AgentError::Session(_)
        | AgentError::HostFailure(_)) => passthrough,
    }
}
