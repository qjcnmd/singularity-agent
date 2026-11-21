//! 手动和自动轮换共用：一次或分块滚动请求更新整份用户消息整理内容，保存后切换上下文。
//!
//! 常规路径保留本轮指令、工具定义和完整上下文以尽量命中提供方前缀缓存；当估算超预算
//! 或由提供方 `context_overflow` 触发时，按“剥离图片与压缩工具结果 → 压缩助手思考与正文
//! → 按调用组切块滚动整理”阶梯降级，确保轮换本身不会陷入超窗死锁。

use std::collections::HashMap;

use super::{AgentError, AgentEvent, RequestContext, Result, TurnEvent};
use crate::compaction;
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{ContextRotationEntry, ContextRotationInput, context, with_writer_async};
use singularity_model::{Message, ModelConfigurationSnapshot, ModelTurnRequest, ModelTurnResponse};
use singularity_protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_CONTEXT_ROTATION_TRIGGER_RATIO: f64 = 0.9;

/// 正文的软目标；请求输出上限另按模型配置和窗口余量计算。
const USER_MESSAGES_TARGET_TOKENS: u32 = 8192;
/// 整理请求至少预留的输出空间，避免输入把窗口挤到 `max_output_tokens == 0`。
const MIN_ROTATION_OUTPUT_TOKENS: u32 = 2048;

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
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if self.context_pressure_tokens().await
            >= (self.context_window() as f64 * AUTO_CONTEXT_ROTATION_TRIGGER_RATIO).floor() as u64
        {
            self.rotate_now(on_event, cancellation, false).await?;
        }
        Ok(())
    }

    /// 自动轮换在本轮请求上下文中选择历史，之后仍可停止本轮的模型和工具循环。
    /// `force_compact` 为真时表示已命中提供方上下文超限，直接进入降级自救路径。
    pub(super) async fn rotate_now(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        force_compact: bool,
    ) -> Result<bool> {
        let Some(input) = ContextRotationInput::select(&self.session).await else {
            return context_rotation_unneeded(on_event, cancellation);
        };
        self.rotate_selected_with_mode(input, on_event, cancellation, force_compact, || true)
            .await?;
        Ok(true)
    }

    /// 整理当前上下文。独立轮换由调用方在保存前锁定停止结果；自动轮换不结束整轮停止窗口。
    pub async fn rotate_selected(
        &mut self,
        input: ContextRotationInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<()> {
        self.rotate_selected_with_mode(input, on_event, cancellation, false, begin_commit).await
    }

    async fn rotate_selected_with_mode(
        &mut self,
        input: ContextRotationInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        force_compact: bool,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<()> {
        let item_id = crate::session::new_entry_id();
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
            item: HistoryItem::ContextRotationResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        }));
        let result = self
            .rotate_context(input, &item_id, on_event, cancellation, force_compact, begin_commit)
            .await;
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
                tracing::warn!(error = %error, "context rotation failed");
                let item = HistoryItem::ContextRotationResult {
                    id: item_id.clone(),
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                };
                (Err(AgentError::ContextRotation { item_id, source: Box::new(error) }), item)
            }
        };
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated { item }));
        outcome
    }

    async fn rotate_context(
        &mut self,
        input: ContextRotationInput,
        item_id: &str,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        force_compact: bool,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        self.ensure_definitions(on_event, cancellation).await?;
        let definitions = self.request_definitions().clone();
        let model = self.provider.model_configuration();
        let (provider_name, model_name) = self.provider.model_identity();
        let identity = (provider_name.to_owned(), model_name.to_owned());
        let instruction = Message::developer(
            ORGANIZE_USER_MESSAGES.replace("{{B}}", &USER_MESSAGES_TARGET_TOKENS.to_string()),
        );
        let instruction_tokens = context::message_token_estimate(&instruction);
        let reserved_output = u64::from(model.max_output_tokens.min(MIN_ROTATION_OUTPUT_TOKENS));
        let input_budget = model.context_window().saturating_sub(reserved_output);
        let base_tokens = match &input.latest_usage {
            Some((provider, model_id, total_tokens, tail_tokens))
                if provider == &identity.0 && model_id == &identity.1 =>
            {
                total_tokens.saturating_add(*tail_tokens)
            }
            _ => definitions.estimated_tokens.saturating_add(input.estimated_tokens),
        };
        let estimated_full_input = base_tokens.saturating_add(instruction_tokens);

        let user_messages = if !force_compact && estimated_full_input <= input_budget {
            // 常规路径：保留完整历史、图片与工具定义，尽量命中提供方前缀缓存。
            let full_defs = definitions.clone();
            let full_messages = input.messages.clone();
            let full_dir = input.image_directory.clone();
            let full_instruction = instruction.clone();
            let max_output_tokens = output_budget_tokens(model, estimated_full_input);
            let request = tokio::task::spawn_blocking(move || {
                let mut messages = full_defs.messages;
                messages.extend(full_messages);
                let images = context::load_images(&mut messages, &full_dir)?;
                messages.push(full_instruction);
                Ok::<_, std::io::Error>(ModelTurnRequest {
                    messages,
                    images,
                    tools: full_defs.tools,
                    max_output_tokens,
                })
            })
            .await
            .expect("context_rotation preparation completes while the runtime is running")?;
            match execute_request(
                &self.provider,
                &self.session,
                &request,
                on_event,
                cancellation,
                singularity_protocol::RequestPurpose::ContextRotation,
            )
            .await
            {
                Ok((response, _, _)) => validate_user_messages(&response)?,
                Err(AgentError::Provider(error)) if error.is_context_overflow() => {
                    tracing::warn!(
                        estimated_tokens = estimated_full_input,
                        window = model.context_window(),
                        "standard context rotation overflowed; falling back to degraded compaction"
                    );
                    let plan = RotationPlan {
                        definitions: &definitions,
                        instruction: &instruction,
                        instruction_tokens,
                        model,
                        input_budget,
                    };
                    self.rotate_degraded(
                        &plan,
                        input.messages,
                        &input.image_directory,
                        on_event,
                        cancellation,
                    )
                    .await?
                }
                Err(error) => return Err(error),
            }
        } else {
            tracing::info!(
                force_compact,
                estimated_tokens = estimated_full_input,
                budget = input_budget,
                "context rotation entering degraded compaction path"
            );
            let plan = RotationPlan {
                definitions: &definitions,
                instruction: &instruction,
                instruction_tokens,
                model,
                input_budget,
            };
            self.rotate_degraded(&plan, input.messages, &input.image_directory, on_event, cancellation)
                .await?
        };

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

    async fn rotate_degraded(
        &self,
        plan: &RotationPlan<'_>,
        mut history: Vec<Message>,
        image_directory: &std::path::Path,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<String> {
        // 阶梯 1：剥离图片二进制（保留路径说明）并折叠所有历史工具结果。
        compaction::strip_images_to_captions(&mut history, image_directory);
        compaction::compact_tool_results(&mut history);
        let stage1_tokens = plan
            .definitions
            .estimated_tokens
            .saturating_add(compaction::messages_tokens(&history))
            .saturating_add(plan.instruction_tokens);
        if stage1_tokens <= plan.input_budget {
            match self.send_degraded_rotation(plan, &history, stage1_tokens, on_event, cancellation).await {
                Ok(text) => return Ok(text),
                Err(AgentError::Provider(error)) if error.is_context_overflow() => {
                    tracing::warn!("stage-1 degraded rotation overflowed; escalating to stage-2");
                }
                Err(error) => return Err(error),
            }
        }

        // 阶梯 2：进一步剥离助手思考并折叠超长助手正文与工具调用参数，完整保留用户发言。
        compaction::compact_assistant_messages(&mut history);
        let stage2_tokens = plan
            .definitions
            .estimated_tokens
            .saturating_add(compaction::messages_tokens(&history))
            .saturating_add(plan.instruction_tokens);
        if stage2_tokens <= plan.input_budget {
            match self.send_degraded_rotation(plan, &history, stage2_tokens, on_event, cancellation).await {
                Ok(text) => return Ok(text),
                Err(AgentError::Provider(error)) if error.is_context_overflow() => {
                    tracing::warn!("stage-2 degraded rotation overflowed; escalating to chunked rotation");
                }
                Err(error) => return Err(error),
            }
        }

        // 阶梯 3：按原子消息组切分，逐批滚动整理；单批可用预算留出上一版整理正文的空间。
        let fixed_overhead = plan.definitions.estimated_tokens.saturating_add(plan.instruction_tokens);
        let summary_reserve = u64::from(USER_MESSAGES_TARGET_TOKENS).min(plan.input_budget / 3);
        let chunk_budget =
            plan.input_budget.saturating_sub(fixed_overhead).saturating_sub(summary_reserve).max(1024);
        let chunks = compaction::partition_into_chunks(history, chunk_budget);
        tracing::info!(chunk_count = chunks.len(), chunk_budget, "running chunked rolling context rotation");
        let mut rolling: Option<String> = None;
        for chunk in chunks {
            rolling = Some(
                self.send_chunk_with_fallback(plan, rolling.as_deref(), chunk, 0, on_event, cancellation)
                    .await?,
            );
        }
        rolling.ok_or_else(|| AgentError::InvalidContextRotation("没有可供整理的历史消息。".into()))
    }

    async fn send_chunk_with_fallback(
        &self,
        plan: &RotationPlan<'_>,
        previous_summary: Option<&str>,
        mut chunk: Vec<Message>,
        depth: u8,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let mut batch = Vec::with_capacity(chunk.len() + 1);
        if let Some(summary) = previous_summary {
            batch.push(Message::user(format!("上一版用户消息整理正文：\n\n{summary}")));
        }
        batch.extend(chunk.iter().cloned());
        let estimated = plan
            .definitions
            .estimated_tokens
            .saturating_add(compaction::messages_tokens(&batch))
            .saturating_add(plan.instruction_tokens);
        match self.send_degraded_rotation(plan, &batch, estimated, on_event, cancellation).await {
            Ok(text) => Ok(text),
            Err(AgentError::Provider(error)) if error.is_context_overflow() && depth < 3 => {
                // 估算低估导致单批仍超窗：将当前批按原子组折半，或极端单条消息截短后重试。
                let groups = compaction::atomic_groups(std::mem::take(&mut chunk));
                if groups.len() >= 2 {
                    let mid = groups.len() / 2;
                    let left: Vec<Message> = groups[..mid].iter().flatten().cloned().collect();
                    let right: Vec<Message> = groups[mid..].iter().flatten().cloned().collect();
                    let first = Box::pin(self.send_chunk_with_fallback(
                        plan,
                        previous_summary,
                        left,
                        depth + 1,
                        on_event,
                        cancellation,
                    ))
                    .await?;
                    Box::pin(self.send_chunk_with_fallback(
                        plan,
                        Some(&first),
                        right,
                        depth + 1,
                        on_event,
                        cancellation,
                    ))
                    .await
                } else {
                    let mut single: Vec<Message> = groups.into_iter().flatten().collect();
                    compaction::shrink_messages_by_half(&mut single);
                    Box::pin(self.send_chunk_with_fallback(
                        plan,
                        previous_summary,
                        single,
                        depth + 1,
                        on_event,
                        cancellation,
                    ))
                    .await
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn send_degraded_rotation(
        &self,
        plan: &RotationPlan<'_>,
        history: &[Message],
        estimated_tokens: u64,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let mut messages = Vec::with_capacity(plan.definitions.messages.len() + history.len() + 1);
        messages.extend(plan.definitions.messages.iter().cloned());
        messages.extend(history.iter().cloned());
        messages.push(plan.instruction.clone());
        let max_output = output_budget_tokens(plan.model, estimated_tokens)
            .max(plan.model.max_output_tokens.min(MIN_ROTATION_OUTPUT_TOKENS));
        let request = ModelTurnRequest {
            messages,
            images: HashMap::new(),
            tools: plan.definitions.tools.clone(),
            max_output_tokens: max_output,
        };
        let (response, _, _) = execute_request(
            &self.provider,
            &self.session,
            &request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::ContextRotation,
        )
        .await?;
        validate_user_messages(&response)
    }
}

struct RotationPlan<'a> {
    definitions: &'a super::request::RequestDefinitions,
    instruction: &'a Message,
    instruction_tokens: u64,
    model: ModelConfigurationSnapshot,
    input_budget: u64,
}

/// 没有新增对话时沿用原有的轮换反馈，不创建请求上下文。
pub fn context_rotation_unneeded(
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
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
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
            item: HistoryItem::ContextRotationResult { id: id.clone(), status, message: None },
        }));
    }
    if cancelled {
        Err(AgentError::Aborted)
    } else {
        Ok(false)
    }
}

/// 只有未截断且正文非空的整理结果才能替换当前上下文。
fn validate_user_messages(response: &ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidContextRotation("用户消息整理达到输出上限，内容不完整。".into()));
    }
    let text = response.assistant_message.content_text();
    if text.trim().is_empty() {
        return Err(AgentError::InvalidContextRotation("用户消息整理没有返回正文。".into()));
    }
    Ok(text)
}
