//! Agent 的核心执行循环：模型调用、工具执行与转向输入都汇聚在这里。
//!
//! 每轮组装请求、调用 provider 并执行工具；模型准备结束时，原子地取走停止窗口内
//! 新到的转向输入或关闭窗口，决定继续还是完成。
//!
//! 上下文压缩有两个触发点：发送前按 ContextView 的真实 usage 基线主动压缩（基线缺失时
//! 用装配阶段的估算兜底）；provider 明确返回 ContextLengthExceeded 时强制压缩后重发。
//! 重发机会每个轮步只有一次，重发失败直接报告该次请求的原因。
//!
//! 模型请求观测、消息与工具结果都经 SessionManager 追加到同一份会话日志，工具结果落盘后
//! 才发布完成事件。历史中缺失的工具结果只在模型输入投影为结果未知，不改写执行事实。
//! 转向控制只存在于 inbox 和 Conversation 的内存状态里，不落盘。
//!
//! 相关模块：请求装配在 self::request，压缩编排在 self::compaction，共用请求执行在 crate::request_execution，
//! 事件类型在 crate::events，转向输入箱在 self::inbox。

mod compaction;
mod dispatch;
mod inbox;
mod questions;
mod request;

use std::sync::Arc;

use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelUsage, Provider, ProviderError,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use self::inbox::{ControlRequest, TurnInbox, TurnInboxHandle, UserInput};
pub use self::questions::UserQuestions;
pub use crate::events::{AgentDiagnostic, AgentEvent};
use crate::request_execution::RequestAccounting;

use self::inbox::lock_inbox;
use crate::message::{AgentMessage, ItemScope, assistant_response_message};
use crate::session::context::ContextView;
use crate::session::{LedgerRecord, SessionError, SessionWriter, lock_writer};
use crate::tools::ToolRegistrySnapshot;

/// Agent 的首次文件指令及后续指令加载目录。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 文件指令（AGENTS.md 等）的用户数据根目录。
    pub instruction_home: std::path::PathBuf,
    /// 准备阶段已经读好的首轮文件指令；文件不存在时为 None，每次压缩后重新读取。
    pub initial_instructions: Option<singularity_core::ProjectInstructions>,
}

/// Agent 循环可能返回的错误。
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    /// 执行失败后的请求或展示记录写入失败；停止执行并同时保留两个原因。
    #[error("{execution}; execution failure could not be persisted: {storage}")]
    FailureRecording {
        execution: Box<AgentError>,
        #[source]
        storage: SessionError,
    },
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
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// Agent 的终止原因。错误细节仍由 AgentError 携带，不在 outcome 里再复制一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
}

/// 一次 run 的终态：只说明为什么停下来、有没有被截断。正文已随 assistant 消息落盘并经
/// 完成事件发布。
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
    questions: Option<Arc<UserQuestions>>,
    mcp: Arc<singularity_mcp::McpManager>,
    skills: singularity_core::skills::SkillCatalog,
    developer_instructions: String,
    /// 本轮全局与项目文件指令；压缩后直接用重新读取的内容替换。
    file_instructions: Option<ModelMessage>,
    provider: Arc<dyn Provider + Send + Sync>,
    /// 从本轮 Provider 冻结的容量配置，供整个执行过程使用。
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
        config: AgentConfig,
        session: SessionWriter,
        mcp: Arc<singularity_mcp::McpManager>,
    ) -> Self {
        let model = provider.model_configuration();
        let context = ContextView::derive(&lock_writer(&session));
        let cwd = lock_writer(&session).cwd().to_path_buf();
        let registry = ToolRegistrySnapshot::default();
        let skills =
            singularity_core::skills::SkillCatalog::discover(&cwd, &config.instruction_home);
        Self {
            session,
            registry,
            questions: None,
            mcp,
            skills,
            // 生成请求前由 refresh_tools 组装当前指令；独立压缩可直接复用历史定义。
            developer_instructions: String::new(),
            file_instructions: None,
            provider,
            model,
            config,
            inbox,
            context,
            accounting: RequestAccounting::default(),
        }
    }

    /// 仅交互宿主装配提问能力；无交互入口不向模型提供会永久等待的工具。
    pub fn with_user_questions(mut self, questions: Arc<UserQuestions>) -> Self {
        self.registry.enable_questions();
        self.questions = Some(questions);
        self
    }

    async fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        Self::append_to_context(&self.session, &mut self.context, move |writer| {
            writer.append_record(record)
        })
        .await
    }

    /// 跑完一个完整的 Agent 循环：把输入持久化为 user 消息，循环处理工具调用，
    /// 运行期间注入的转向输入在后续轮次生效，直到模型停下来。
    ///
    /// 取消时返回 terminal_reason=Aborted（取消不算错误）；已经生成的内容以会话内容
    /// 和完成事件为准，不由返回值重复携带。
    /// 生命周期所有者在返回后关闭输入箱并收回未送达输入。
    pub async fn run(
        &mut self,
        input: &UserInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome> {
        let mut outcome = AgentOutcome {
            truncated: false,
            terminal_reason: AgentTerminalReason::Completed,
        };
        self.append_user_input(input, on_event).await?;

        let loaded = self.config.initial_instructions.take();
        self.apply_instructions(loaded, on_event);
        self.load_and_record_manual_skill(&input.text).await?;
        self.refresh_tools(on_event, cancellation).await;

        loop {
            if cancellation.is_cancelled() {
                return Ok(abort_outcome(outcome));
            }
            // 把转向队列里的消息全部注入：按接受顺序追加为 user 消息，成功后再通知已消费。
            let drained = lock_inbox(&self.inbox).drain();
            self.inject_controls(drained, on_event).await?;
            let (response, assistant_result_entry_id) =
                match self.generate_response(on_event, cancellation).await {
                    Ok(response) => response,
                    // 取消不是失败：返回中止终态，不返回错误。
                    Err(AgentError::Aborted) => return Ok(abort_outcome(outcome)),
                    Err(error) => return Err(error),
                };
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
            self.append_message(Some(&assistant_result_entry_id), assistant)
                .await?;
            on_event(AgentEvent::MessageFinished {
                message_id: assistant_result_entry_id.clone(),
                items: public_items,
                failed: false,
            });
            if !tool_calls.is_empty() {
                self.dispatch_tools(
                    tool_calls,
                    &assistant_result_entry_id,
                    length_truncated,
                    cancellation,
                    on_event,
                )
                .await?;
                // 还要继续下一轮：截断标记先记下，否则会被后续轮覆盖。
                if length_truncated {
                    outcome.truncated = true;
                }
                continue;
            }
            // 没有工具调用：本轮响应就是最终轮，结果只保留截断标记。
            outcome.truncated = length_truncated;
            // 模型准备停下来：把停止窗口内到达的转向输入注入后继续请求。
            let Some(pending_inputs) = lock_inbox(&self.inbox).take_at_stop() else {
                return Ok(outcome);
            };
            self.inject_controls(pending_inputs, on_event).await?;
        }
    }

    async fn inject_controls(
        &mut self,
        requests: Vec<ControlRequest>,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let mut pending = requests.into_iter();
        while let Some(request) = pending.next() {
            if let Err(error) = self.append_user_input(&request.input, on_event).await {
                lock_inbox(&self.inbox).restore(std::iter::once(request).chain(pending));
                return Err(error);
            }
            on_event(AgentEvent::ControlChanged);
            if let Err(error) = self.load_and_record_manual_skill(&request.input.text).await {
                lock_inbox(&self.inbox).restore(pending);
                return Err(error);
            }
        }
        Ok(())
    }

    async fn append_user_input(
        &mut self,
        input: &UserInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        self.save_images(&input.images).await?;
        let images: Vec<_> = input
            .images
            .iter()
            .map(|image| image.attachment.clone())
            .collect();
        let mut content = vec![crate::message::ContentBlock::Text {
            text: input.text.clone(),
        }];
        content.extend(
            images
                .iter()
                .cloned()
                .map(crate::message::ContentBlock::Image),
        );
        let message = AgentMessage::User { content };
        let entry_id = self.append_message(None, message).await?;
        on_event(AgentEvent::UserMessage {
            entry_id,
            text: input.text.clone(),
            images,
        });
        Ok(())
    }

    async fn save_images(&self, images: &[crate::image::InputImage]) -> Result<()> {
        if images.is_empty() {
            return Ok(());
        }
        let directory = lock_writer(&self.session).image_directory();
        let images = images.to_vec();
        tokio::task::spawn_blocking(move || {
            for image in images {
                image.save(&directory)?;
            }
            Ok::<_, SessionError>(())
        })
        .await
        .expect("image worker completes while the runtime is running")?;
        Ok(())
    }

    /// 持久化一条消息并推进上下文，返回持久条目 id；id 为 Some 时沿用预分配的结果条目 id。
    async fn append_message(&mut self, id: Option<&str>, message: AgentMessage) -> Result<String> {
        let id = id.map(str::to_string);
        Self::append_to_context(&self.session, &mut self.context, move |writer| match id {
            Some(id) => writer.append_message_with_id(&id, message),
            None => writer.append_message(message),
        })
        .await
    }

    /// 追加期间一直持写者锁，直到新条目被上下文吸收：锁内保证追加的条目就是随后被上下文
    /// 吸收的同一条尾条目。控制输入先进入 inbox，之后也走这条追加路径。
    async fn append_to_context(
        session: &SessionWriter,
        context: &mut ContextView,
        append: impl FnOnce(
            &mut crate::session::SessionManager,
        ) -> std::result::Result<String, SessionError>
        + Send
        + 'static,
    ) -> Result<String> {
        Self::with_context(session, context, move |session, context| {
            let mut writer = lock_writer(session);
            let entry_id = append(&mut writer)?;
            context.append_entry(&writer, writer.entries().len() - 1);
            Ok(entry_id)
        })
        .await
    }

    /// 在线程池中使用本轮上下文；业务失败也先归还上下文，再传播错误。
    /// 操作自行决定写者锁范围，避免把整个剪枝或请求准备过程扩大为一个临界区。
    async fn with_context<T: Send + 'static>(
        session: &SessionWriter,
        context: &mut ContextView,
        operation: impl FnOnce(&SessionWriter, &mut ContextView) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let session = Arc::clone(session);
        let mut current = std::mem::take(context);
        let (updated, result) = tokio::task::spawn_blocking(move || {
            let result = operation(&session, &mut current);
            (current, result)
        })
        .await
        .expect("context worker completes while the runtime is running");
        *context = updated;
        result
    }

    /// 本轮从 Provider 冻结的上下文容量。
    pub fn context_window(&self) -> u64 {
        self.model.context_window()
    }

    /// 实测请求用量，包含被拒绝的摘要与失败的尝试。
    pub fn request_usage(&self) -> &ModelUsage {
        &self.accounting.usage
    }
}

fn abort_outcome(mut outcome: AgentOutcome) -> AgentOutcome {
    outcome.terminal_reason = AgentTerminalReason::Aborted;
    outcome
}
