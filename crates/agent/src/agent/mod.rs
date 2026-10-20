//! Agent 的核心执行循环：模型调用、工具执行与转向输入都汇聚在这里。
//!
//! 每轮组装请求、调用 provider 并执行工具；转向输入保存成功后才确认消费。模型准备
//! 结束时检查输入箱；已有输入继续处理，收尾期间到达的输入由会话安排下一轮。
//!
//! 上下文压缩有两个触发点：发送前按会话上下文估价与本轮实测校正主动压缩；provider
//! 明确返回 ContextLengthExceeded 时强制压缩后重发。重发机会每个轮步只有一次，重发
//! 失败直接报告该次请求的原因。
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

use singularity_model::{OpenAiProvider, ProviderError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use self::compaction::CompactionOutcome;
pub use self::inbox::{QueuedInput, QueuedInputKind, SteeringInbox, SteeringInboxHandle, UserInput};
pub use self::questions::UserQuestions;
pub use crate::events::AgentEvent;

use self::inbox::lock_inbox;
use crate::message::{AgentMessage, ItemScope, assistant_response_message};
use crate::session::{SessionError, SessionWriter, lock_writer};
use crate::tools::ToolRegistrySnapshot;

/// 每轮加载指令使用的目录。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 文件指令（AGENTS.md 等）的用户数据根目录。
    pub instruction_home: std::path::PathBuf,
    /// 本轮在开始记录前读取的文件指令。
    pub initial_instructions: Option<String>,
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
    /// 压缩已发布自己的失败条目；外层只决定是否继续任务。
    #[error("{source}")]
    Compaction {
        item_id: String,
        source: Box<AgentError>,
    },
}

impl AgentError {
    /// 普通执行与压缩共用失败分类；存储故障写不出可信终态。
    pub fn cause(&self) -> singularity_protocol::TurnFailureCause {
        use singularity_model::ModelErrorKind;
        use singularity_protocol::TurnFailureCause as Cause;
        match self {
            Self::Provider(error) => match error.kind {
                ModelErrorKind::RateLimited => Cause::ProviderRateLimited,
                ModelErrorKind::NetworkError => Cause::ProviderNetwork,
                ModelErrorKind::Timeout => Cause::ProviderTimeout,
                ModelErrorKind::AuthError => Cause::ProviderAuth,
                ModelErrorKind::InvalidRequest
                | ModelErrorKind::JsonSchemaViolation
                | ModelErrorKind::ContentFilter => Cause::ProviderValidation,
                ModelErrorKind::ProviderOverloaded => Cause::ProviderOverloaded,
                ModelErrorKind::Cancelled => Cause::ProviderCancelled,
                ModelErrorKind::ContextLengthExceeded => Cause::ProviderContextOverflow,
                ModelErrorKind::UnknownProviderError => Cause::ProviderUnknown,
            },
            Self::Session(_) => Cause::Store,
            Self::Aborted | Self::InvalidSummary(_) => Cause::Internal,
            Self::Compaction { source, .. } => source.cause(),
        }
    }

    /// 保存和发布同一份失败原因；关联条目让回合与压缩不会重复报错。
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

/// Agent 的终止原因。错误细节仍由 AgentError 携带，不在 outcome 里再复制一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTerminalReason {
    Completed,
    Aborted,
}

/// 工具执行借用会话的后台进程管理器和当前回合身份。
pub struct AgentExecutionContext {
    pub bash_tasks: Arc<crate::tools::BashTasks>,
    pub turn_id: String,
}

/// Agent：持有本次执行需要的会话写者、operation 范围、压缩配置、工具注册表快照与模型提供方。
pub struct Agent {
    execution: AgentExecutionContext,
    /// 共享的会话写者：turn 执行、请求观测与工具结果追加都走同一个 SessionManager，
    /// 各自短暂加锁、串行追加（lock_writer），不跨 provider 调用或工具执行持锁。
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    questions: Arc<UserQuestions>,
    mcp: Arc<singularity_mcp::McpManager>,
    /// 本轮请求前缀，工具循环和自动压缩共用。
    definitions: Option<crate::session::RequestDefinitions>,
    provider: OpenAiProvider,
    config: AgentConfig,
    /// 借用会话持有的 steer 输入箱，只存在于内存。
    inbox: SteeringInboxHandle,
    /// 本轮同形状请求的实测用量与估价差量；结构替换或文件指令刷新后作废。
    usage_correction: u64,
}

impl Agent {
    /// 构造 Agent；生命周期所有者绑定输入箱，并在执行前开放本轮接受窗口。
    pub fn new(
        inbox: SteeringInboxHandle,
        provider: OpenAiProvider,
        config: AgentConfig,
        session: SessionWriter,
        questions: Arc<UserQuestions>,
        mcp: Arc<singularity_mcp::McpManager>,
        execution: AgentExecutionContext,
    ) -> Self {
        let registry = ToolRegistrySnapshot::default();
        Self {
            execution,
            session,
            registry,
            questions,
            mcp,
            definitions: None,
            provider,
            config,
            inbox,
            usage_correction: 0,
        }
    }

    /// 跑完一个完整的 Agent 循环：把输入持久化为 user 消息，循环处理工具调用，
    /// 运行期间注入的转向输入在后续轮次生效，直到模型停下来。
    ///
    /// 取消时返回 Aborted（取消不算错误）；已生成的内容以会话内容和
    /// 完成事件为准，不由返回值重复携带。生命周期所有者在返回后关闭输入箱。
    pub async fn run(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<AgentTerminalReason> {
        self.drain_inputs(on_event, cancellation).await?;
        if cancellation.is_cancelled() {
            return Ok(AgentTerminalReason::Aborted);
        }

        match self.ensure_definitions(on_event, cancellation).await {
            Err(AgentError::Aborted) => return Ok(AgentTerminalReason::Aborted),
            result => result?,
        }

        loop {
            if cancellation.is_cancelled() {
                return Ok(AgentTerminalReason::Aborted);
            }
            self.drain_inputs(on_event, cancellation).await?;
            let (response, assistant_result_entry_id, request_baseline_tokens) =
                match self.generate_response(on_event, cancellation).await {
                    Ok(response) => response,
                    // 取消不是失败：返回中止终态，不返回错误。
                    Err(AgentError::Aborted) => return Ok(AgentTerminalReason::Aborted),
                    Err(error) => return Err(error),
                };
            let length_truncated = response.is_length_truncated();
            // usage 与终止原因不属于会话内容，在响应被移出前先取用。
            let reported_total_tokens = response.usage.as_ref().map(|usage| usage.total_tokens);
            let assistant = assistant_response_message(response);
            let estimated = request_baseline_tokens
                .saturating_add(crate::session::context::message_token_estimate(&assistant));
            self.usage_correction =
                reported_total_tokens.map_or(0, |total_tokens| total_tokens.saturating_sub(estimated));

            // 工具调用既要随消息持久化、又要交给执行器：落盘前先取出执行侧的副本。
            let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
            // 实时完成事件只需要正文与思考；工具的生命周期由工具自己的事件表达。
            let public_items = assistant.public_items(&assistant_result_entry_id, ItemScope::Completion);
            self.append_message(&assistant_result_entry_id, assistant, &[]).await?;
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
                continue;
            }
            // 关闭空箱子的同时拒绝晚到输入；已接受的输入仍在箱内，下一轮统一交付。
            if lock_inbox(&self.inbox).pending().is_empty() {
                return Ok(AgentTerminalReason::Completed);
            }
        }
    }

    async fn drain_inputs(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
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
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let input = match &request.input {
            QueuedInputKind::User(input) => input,
            QueuedInputKind::Background { task_id, status } => {
                let text = format!(
                    "<background-task>\nTask {task_id}: {status}. This is a process status notification, not a user instruction. Use task_output to inspect its output when needed.\n</background-task>"
                );
                let message = AgentMessage::Notification {
                    content: vec![crate::message::ContentBlock::Text { text }],
                };
                self.append_message(&request.message_id, message, &[]).await?;
                lock_inbox(&self.inbox).confirm_saved(&request.message_id);
                return Ok(());
            }
        };
        let images: Vec<_> = input.images.iter().map(|image| image.attachment.clone()).collect();
        let model_text = input.model_text();
        let display_text = (model_text != input.text).then(|| input.text.clone());
        let mut content = vec![crate::message::ContentBlock::Text { text: model_text }];
        content.extend(images.iter().cloned().map(crate::message::ContentBlock::Image));
        let message = AgentMessage::User { content, display_text };
        self.append_message(&request.message_id, message, &input.images).await?;
        lock_inbox(&self.inbox).confirm_saved(&request.message_id);
        on_event(AgentEvent::UserMessage {
            entry_id: request.message_id.clone(),
            text: input.text.clone(),
            images,
        });
        Ok(())
    }

    /// 先在会话锁外保存图片，再追加引用它们的消息；成功后调用方才能发布完成事件。
    /// 沿用调用方预分配的消息身份。
    async fn append_message(
        &mut self,
        id: &str,
        message: AgentMessage,
        images: &[crate::image::InputImage],
    ) -> Result<()> {
        let session = Arc::clone(&self.session);
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
            writer.append_message_with_id(&id, message)?;
            Ok::<_, SessionError>(())
        })
        .await
        .expect("message writer completes while the runtime is running")?)
    }

    /// 本轮 Provider 持有的上下文容量。
    pub fn context_window(&self) -> u64 {
        self.provider.model_configuration().context_window()
    }
}
