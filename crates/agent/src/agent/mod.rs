//! Agent 的执行循环：调用模型、执行工具、处理运行中追加的输入。
//!
//! 每次循环组装请求、调用模型并执行工具；队列中的输入保存成功后才从队列移除。
//! 模型不再调用工具时检查队列，有输入就继续；收尾期间到达的输入由任务安排下一轮。
//!
//! 压缩有两个触发点：发送前按实际用量加新增内容估算，超过阈值就压缩；提供方明确
//! 返回上下文超限时，压缩后重发一次，重发再失败就直接报告这次的原因。
//!
//! 请求记录、消息和工具结果都写进同一份会话日志，工具结果保存后才发布完成事件。
//! 日志里缺少结果的工具调用，只在发给模型时补成“结果未知”，不修改日志。
//! 输入队列只在内存中，不写日志。
//!
//! 相关模块：组装请求在 self::request，压缩在 self::compaction，发送和重试在
//! crate::request_execution，输入队列在 self::inbox；事件类型是 singularity_protocol::TurnEvent。

mod compaction;
pub use compaction::compaction_unneeded;
mod dispatch;
mod inbox;
mod questions;
pub(crate) mod request;

use std::sync::Arc;

use singularity_model::{OpenAiProvider, ProviderError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use self::inbox::{QueuedInput, QueuedInputKind, SteeringInbox, SteeringInboxHandle, UserInput};
pub use self::questions::UserQuestions;
pub use singularity_protocol::TurnEvent;

use self::inbox::lock_inbox;
use crate::message::public_items;
use crate::session::{SessionError, SessionWriter, lock_writer};
use crate::tools::ToolRegistrySnapshot;
use singularity_model::Message;

/// 每轮加载指令使用的目录。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 文件指令（AGENTS.md 等）的用户数据根目录。
    pub instruction_home: std::path::PathBuf,
    /// 本轮在开始记录前读取的文件指令。
    pub initial_instructions: Vec<singularity_core::InstructionFile>,
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
    #[error("模型回复达到输出上限，内容未完成。请发送消息继续。")]
    OutputTruncated,
    #[error("{0}")]
    InvalidSummary(String),
    /// 压缩失败；压缩条目本身已显示失败原因，外层据此避免重复显示同一错误。
    #[error("{source}")]
    Compaction {
        item_id: String,
        source: Box<AgentError>,
    },
}

impl AgentError {
    /// 普通执行和压缩共用的失败分类；日志写入失败归为 Store，此时不能再保存结束记录。
    pub fn cause(&self) -> singularity_protocol::TurnFailureCause {
        use singularity_protocol::TurnFailureCause as Cause;
        match self {
            Self::Provider(_) | Self::OutputTruncated => Cause::Provider,
            Self::Session(_) => Cause::Store,
            Self::Aborted | Self::InvalidSummary(_) => Cause::Internal,
            Self::Compaction { source, .. } => source.cause(),
        }
    }

    /// 保存和发布的失败原因；压缩失败时带上压缩条目 ID，界面据此避免重复报错。
    pub fn detail(&self) -> singularity_protocol::TurnErrorDetail {
        let item_id = match self {
            Self::Compaction { item_id, .. } => Some(item_id.clone()),
            _ => None,
        };
        singularity_protocol::TurnErrorDetail {
            cause: self.cause(),
            message: self.to_string(),
            item_id,
        }
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;

/// 普通生成和独立压缩共用的模型、指令和工具定义、会话写者；压缩不需要工具执行所需的状态。
pub struct RequestContext {
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    mcp: Arc<singularity_mcp::McpManager>,
    definitions: Option<request::RequestDefinitions>,
    provider: OpenAiProvider,
    config: AgentConfig,
}
impl RequestContext {
    /// 按回合开始时的同一套规则加载指令和工具目录，只返回给工作台查看的内容，不发送请求。
    pub async fn inspect_context(
        mut self,
        cancellation: &CancellationToken,
    ) -> Result<singularity_protocol::InjectedContext> {
        let (_, context) = self.load_definitions(cancellation).await?;
        Ok(context)
    }

    /// 提供方和文件指令在创建时确定；技能和 MCP 工具在第一次请求前加载。
    pub fn new(
        provider: OpenAiProvider,
        config: AgentConfig,
        session: SessionWriter,
        mcp: Arc<singularity_mcp::McpManager>,
    ) -> Self {
        Self {
            session,
            registry: ToolRegistrySnapshot::default(),
            mcp,
            definitions: None,
            provider,
            config,
        }
    }
    /// 当前模型的上下文窗口。
    pub fn context_window(&self) -> u64 {
        self.provider.model_configuration().context_window()
    }
}

/// 一个普通回合的执行者：消费输入队列、调用模型、执行工具。
pub struct Agent {
    requests: RequestContext,
    bash_tasks: Arc<crate::tools::BashTasks>,
    questions: Arc<UserQuestions>,
    inbox: SteeringInboxHandle,
}
impl Agent {
    /// 本轮加载的指令和工具定义由 `RequestContext` 持有；停止、提问和工具执行状态在这里绑定。
    pub fn new(
        requests: RequestContext,
        inbox: SteeringInboxHandle,
        questions: Arc<UserQuestions>,
        bash_tasks: Arc<crate::tools::BashTasks>,
    ) -> Self {
        Self { requests, bash_tasks, questions, inbox }
    }

    /// 执行一个完整回合：把输入保存为用户消息，循环调用模型和工具，直到模型不再调用
    /// 工具且队列为空。运行中追加的输入在下一次模型请求前交给模型。
    ///
    /// 停止时返回 AgentError::Aborted，由会话记为中断；生成的内容已经保存并通过事件发布。
    pub async fn run(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.drain_inputs(on_event, cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }

        self.requests.ensure_definitions(on_event, cancellation).await?;

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Aborted);
            }
            self.drain_inputs(on_event, cancellation).await?;
            let (response, assistant_result_entry_id) =
                self.generate_response(on_event, cancellation).await?;
            let length_truncated = response.is_length_truncated();
            let assistant = response.assistant_message;

            // 回复已随请求记录保存，这里只取出其中的工具调用来执行。
            let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
            // 按消息中的顺序发布所有内容；工具进度随后更新原来的条目。
            let public_items = public_items(
                &assistant,
                &assistant_result_entry_id,
                singularity_protocol::TurnStatus::Completed,
                singularity_protocol::ToolStatus::Pending,
                &singularity_core::now_iso(),
            );
            on_event(TurnEvent::MessageFinished {
                message_id: assistant_result_entry_id.clone(),
                items: public_items,
            });
            let has_tool_calls = !tool_calls.is_empty();
            if has_tool_calls {
                self.dispatch_tools(
                    tool_calls,
                    &assistant_result_entry_id,
                    length_truncated,
                    cancellation,
                    on_event,
                )
                .await?;
            }
            if length_truncated {
                return Err(AgentError::OutputTruncated);
            }
            // 之后到达的输入由 Conversation 在本轮结束后接着处理。
            if !has_tool_calls && lock_inbox(&self.inbox).pending().is_empty() {
                return Ok(());
            }
        }
    }

    async fn drain_inputs(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let next = lock_inbox(&self.inbox).next();
            let Some(request) = next else { break };
            self.append_user_input(&request, on_event).await?;
        }
        Ok(())
    }

    async fn append_user_input(
        &mut self,
        request: &QueuedInput,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<()> {
        let input = match &request.input {
            QueuedInputKind::User(input) => input,
            QueuedInputKind::Background { task_id, status } => {
                let text = format!(
                    "<background-task>\nTask {task_id}: {status}. This is a process status notification, not a user instruction. Use task_output to inspect its output when needed.\n</background-task>"
                );
                let message = Message::Notification {
                    content: vec![singularity_model::ContentBlock::text(text)],
                };
                self.append_message(&request.message_id, message, &[]).await?;
                lock_inbox(&self.inbox).confirm_saved(&request.message_id);
                return Ok(());
            }
        };
        let images: Vec<_> = input.images.iter().map(|image| image.attachment.clone()).collect();
        let text = input.text.clone();
        let cwd = lock_writer(&self.requests.session).cwd().to_path_buf();
        let home = self.requests.config.instruction_home.clone();
        let model_text =
            tokio::task::spawn_blocking(move || inbox::expand_skill_references(&text, &cwd, &home))
                .await
                .expect("skill references resolve while the runtime is running");
        let model_text = input.text_with_files(model_text);
        let display_text = (model_text != input.text).then(|| input.text.clone());
        let mut content = vec![singularity_model::ContentBlock::text(model_text)];
        content.extend(images.iter().cloned().map(singularity_model::ContentBlock::Image));
        let message = Message::User {
            content,
            display_text,
            files: input.files.clone(),
        };
        self.append_message(&request.message_id, message, &input.images).await?;
        lock_inbox(&self.inbox).confirm_saved(&request.message_id);
        on_event(TurnEvent::ItemUpdated {
            item: singularity_protocol::HistoryItem::Message {
                id: request.item_id(),
                text: input.text.clone(),
                at: Some(singularity_core::now_iso()),
                stats: None,
                images,
                files: input.files.clone(),
                role: singularity_protocol::HistoryMessageRole::User,
                status: singularity_protocol::TurnStatus::Completed,
            },
        });
        Ok(())
    }

    /// 先在写者锁外保存图片，再追加引用它们的消息，使用调用方事先分配的消息 ID；
    /// 成功返回后调用方才发布完成事件。
    async fn append_message(
        &self,
        id: &str,
        message: Message,
        images: &[crate::image::InputImage],
    ) -> Result<()> {
        let session = Arc::clone(&self.requests.session);
        let id = id.to_string();
        let images = images.to_vec();
        Ok(tokio::task::spawn_blocking(move || {
            if !images.is_empty() {
                let directory = lock_writer(&session).image_directory();
                for image in images {
                    image.save(&directory)?;
                }
            }
            let mut writer = lock_writer(&session);
            writer.append_message_with_id(&id, message, None)?;
            Ok::<_, SessionError>(())
        })
        .await
        .expect("message writer completes while the runtime is running")?)
    }
}
