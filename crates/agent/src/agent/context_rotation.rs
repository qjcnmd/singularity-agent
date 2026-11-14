//! 手动和自动轮换共用：一次请求更新整份用户消息整理内容，保存后切换上下文。

use super::{AgentError, RequestContext, Result, TurnEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{ContextRotationEntry, ContextRotationInput, context, with_writer_async};
use singularity_model::{Message, ModelTurnRequest, ModelTurnResponse};
use singularity_protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_CONTEXT_ROTATION_TRIGGER_RATIO: f64 = 0.9;

/// 正文的软目标；请求输出上限另按模型配置和窗口余量计算。
const USER_MESSAGES_TARGET_TOKENS: u32 = 8192;

const ORGANIZE_USER_MESSAGES: &str = r#"请结合当前对话，更新这份用户消息整理内容，输出完整的新版本。首次整理时，直接生成第一版。

整理对象是用户在对话中实际表达的内容，包括提问、反馈、猜测、纠正、指令，以及工具结果或后台通知中记录的用户回复。上一版用户消息整理正文用于接续此前的表达。

按内容来源区分，不把 user 角色中的所有文字都当成用户发言。文件指令（<file-instructions>）、技能正文（<skill>）、程序接手提示、后台通知中的程序状态和本次整理要求都不是整理对象。助手消息以及工具结果和后台通知中的其余内容只用于理解用户表达的背景，其中的工作进度、判断和执行结果留在原始日志中按需回读。上一版中混入的这些内容也应移除；没有新的用户表达时，不新增用户要求或偏好。

优先保留用户原话及对应时间。整体在 {{B}} Token 预算内时，尽量逐条保留完整表达，不为缩短篇幅重新改写、归类或概括。问答回复保留用户的选择或原话，并附理解回答所需的简短问题背景。新增用户表达接续已有内容；没有新增用户表达时沿用原文字，只清除此前混入的非用户内容。

只有整体接近预算时，才合并重复、调整详略；保留含义、语气、条件、不确定性、纠正及前后变化。

每条用户表达引用相应原始消息或回复记录的时间，形式为 [10-31 15:06:32]，已有引用随原文保留。工具结果或后台通知中的用户回复使用标注的记录时间；它是记录保存时间，不声称是外部操作的精确发生时间。来源没有时间时不编造。

本次回复直接给出更新后的用户消息整理正文。"#;

impl RequestContext {
    pub(super) async fn rotate_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if self.context_pressure_tokens()
            >= (self.context_window() as f64 * AUTO_CONTEXT_ROTATION_TRIGGER_RATIO).floor() as u64
        {
            self.rotate_now(on_event, cancellation).await?;
        }
        Ok(())
    }

    /// 自动轮换在本轮请求上下文中选择历史，之后仍可停止本轮的模型和工具循环。
    pub(super) async fn rotate_now(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let Some(input) = ContextRotationInput::select(&self.session).await else {
            return context_rotation_unneeded(on_event, cancellation);
        };
        self.rotate_selected(input, on_event, cancellation, || true).await?;
        Ok(true)
    }

    /// 整理当前上下文。独立轮换由调用方在保存前锁定停止结果；自动轮换不结束整轮停止窗口。
    pub async fn rotate_selected(
        &mut self,
        input: ContextRotationInput,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<()> {
        let item_id = crate::session::new_entry_id();
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::ContextRotationResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        });
        let result = self.rotate_context(input, &item_id, on_event, cancellation, begin_commit).await;
        let (outcome, item) = match result {
            Ok(user_messages) => (Ok(()), HistoryItem::ContextRotation { id: item_id, user_messages }),
            Err(AgentError::Aborted) => (
                Err(AgentError::Aborted),
                HistoryItem::ContextRotationResult {
                    id: item_id,
                    status: TurnStatus::Interrupted,
                    message: None,
                },
            ),
            Err(error) => {
                let item = HistoryItem::ContextRotationResult {
                    id: item_id.clone(),
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                };
                (Err(AgentError::ContextRotation { item_id, source: Box::new(error) }), item)
            }
        };
        on_event(TurnEvent::ItemUpdated { item });
        outcome
    }

    async fn rotate_context(
        &mut self,
        input: ContextRotationInput,
        item_id: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        self.ensure_definitions(on_event, cancellation).await?;
        let definitions = self.request_definitions().clone();
        let model = self.provider.model_configuration();
        let request = tokio::task::spawn_blocking(move || {
            let mut messages = definitions.messages;
            messages.extend(input.messages);
            let images = context::load_images(&mut messages, &input.image_directory)?;
            let instruction = Message::developer(
                ORGANIZE_USER_MESSAGES.replace("{{B}}", &USER_MESSAGES_TARGET_TOKENS.to_string()),
            );
            let input_tokens = definitions
                .estimated_tokens
                .saturating_add(input.estimated_tokens)
                .saturating_add(context::message_token_estimate(&instruction));
            messages.push(instruction);
            Ok::<_, std::io::Error>(ModelTurnRequest {
                messages,
                images,
                tools: definitions.tools,
                max_output_tokens: output_budget_tokens(&model, input_tokens),
            })
        })
        .await
        .expect("context_rotation preparation completes while the runtime is running")?;
        let (response, _, _) = execute_request(
            &self.provider,
            &self.session,
            &request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::ContextRotation,
        )
        .await?;
        let user_messages = validate_user_messages(response)?;
        if !begin_commit() || cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let entry = ContextRotationEntry { user_messages: user_messages.clone() };
        // 整理记录追加成功后才切换上下文；写入失败时原上下文仍然有效。
        let id = item_id.to_owned();
        with_writer_async(&self.session, move |writer| writer.append_context_rotation_with_id(&id, entry))
            .await?;
        Ok(user_messages)
    }
}

/// 没有新增对话时沿用原有的轮换反馈，不创建请求上下文。
pub fn context_rotation_unneeded(
    on_event: &mut (dyn FnMut(TurnEvent) + Send),
    cancellation: &CancellationToken,
) -> Result<bool> {
    let id = crate::session::new_entry_id();
    let cancelled = cancellation.is_cancelled();
    let final_status = if cancelled {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    for status in [TurnStatus::Running, final_status] {
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::ContextRotationResult { id: id.clone(), status, message: None },
        });
    }
    if cancelled {
        Err(AgentError::Aborted)
    } else {
        Ok(false)
    }
}

/// 只有未截断且正文非空的整理结果才能替换当前上下文。
fn validate_user_messages(response: ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidContextRotation("用户消息整理达到输出上限，内容不完整。".into()));
    }
    let text = response.assistant_message.content_text();
    if text.trim().is_empty() {
        return Err(AgentError::InvalidContextRotation("用户消息整理没有返回正文。".into()));
    }
    Ok(text)
}
