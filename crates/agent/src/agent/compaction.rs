//! 手动和自动压缩共用：总结早期历史、保留近期原文，成功后保存摘要和保留位置。

use super::{AgentError, RequestContext, Result, TurnEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{CompactionEntry, CompactionPrefix, context, with_writer_async};
use singularity_model::{Message, ModelTurnRequest, ModelTurnResponse};
use singularity_protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;

/// 摘要请求的目标输出上限；实际值还受模型上限与本次请求的窗口余量约束。
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;

const INITIAL_SUMMARY_INSTRUCTION: &str =
    "Summarize the earlier conversation so another coding assistant can continue the user's current task.";
const UPDATE_SUMMARY_INSTRUCTION: &str = "Update the previous summary with the new conversation above. Preserve still-current goals, constraints, completed work, and decisions. Move finished work to Done, remove resolved blockers, and revise Next Steps.";
const SUMMARY_FORMAT: &str = r#"Output only a concise summary with these Markdown sections, in order:
## Goal
[The user's current objective]
## Constraints & Preferences
- [Current user requirements and preferences]
## Progress
### Done
- [x] [Completed work]
### In Progress
- [ ] [Current work]
### Blocked
- [Current blockers]
## Key Decisions
- **[Decision]**: [Reason]
## Next Steps
1. [Next concrete action]
## Critical Context
- [Facts needed to continue]

Use "(none)" for an empty section. Preserve exact paths, identifiers, commands, errors, and user wording when needed to continue. Distinguish verified results from plans. Project files remain authoritative; Harness and project instructions are loaded separately. Do not call tools or continue the conversation."#;

impl RequestContext {
    pub(super) async fn reduce_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if self.context_pressure_tokens()
            >= (self.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
        {
            self.compact_now(on_event, cancellation).await?;
        }
        Ok(())
    }

    /// 自动压缩在本轮请求上下文中选择历史，之后仍可停止本轮的模型和工具循环。
    pub(super) async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let Some(prefix) = CompactionPrefix::select(&self.session).await else {
            return compaction_unneeded(on_event, cancellation);
        };
        self.compact_selected(prefix, on_event, cancellation, || true).await?;
        Ok(true)
    }

    /// 总结已选历史。独立压缩由调用方在保存前锁定停止结果；自动压缩不结束整轮停止窗口。
    pub async fn compact_selected(
        &mut self,
        prefix: CompactionPrefix,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<()> {
        let item_id = crate::session::new_entry_id();
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        });
        let result = self.compact_context(prefix, &item_id, on_event, cancellation, begin_commit).await;
        let (outcome, item) = match result {
            Ok(summary) => (Ok(()), HistoryItem::Compaction { id: item_id, summary }),
            Err(AgentError::Aborted) => (
                Err(AgentError::Aborted),
                HistoryItem::CompactionResult {
                    id: item_id,
                    status: TurnStatus::Interrupted,
                    message: None,
                },
            ),
            Err(error) => {
                let item = HistoryItem::CompactionResult {
                    id: item_id.clone(),
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                };
                (Err(AgentError::Compaction { item_id, source: Box::new(error) }), item)
            }
        };
        on_event(TurnEvent::ItemUpdated { item });
        outcome
    }

    async fn compact_context(
        &mut self,
        prefix: CompactionPrefix,
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
        let first_kept_entry_id = prefix.first_kept_entry_id;
        let request = tokio::task::spawn_blocking(move || {
            let mut messages = definitions.messages;
            messages.extend(prefix.messages);
            let images = context::load_images(&mut messages, &prefix.image_directory)?;
            let task = if prefix.updating {
                UPDATE_SUMMARY_INSTRUCTION
            } else {
                INITIAL_SUMMARY_INSTRUCTION
            };
            let instruction = Message::user(format!("{task}\n\n{SUMMARY_FORMAT}"));
            let input_tokens = definitions
                .estimated_tokens
                .saturating_add(prefix.estimated_tokens)
                .saturating_add(context::message_token_estimate(&instruction));
            messages.push(instruction);
            Ok::<_, std::io::Error>(ModelTurnRequest {
                messages,
                images,
                tools: definitions.tools,
                max_output_tokens: output_budget_tokens(&model, input_tokens, DEFAULT_SUMMARY_MAX_TOKENS),
            })
        })
        .await
        .expect("summary preparation completes while the runtime is running")?;
        let (response, _, _) = execute_request(
            &self.provider,
            &self.session,
            &request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Compaction,
        )
        .await?;
        let summary = validate_summary(response)?;
        if !begin_commit() || cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let entry = CompactionEntry {
            summary: summary.clone(),
            first_kept_entry_id,
        };
        // 摘要和保留位置写在同一行；写入失败时旧上下文仍然有效。
        let id = item_id.to_owned();
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        Ok(summary)
    }
}

/// 没有可总结的历史时沿用原有的压缩反馈，不创建请求上下文。
pub fn compaction_unneeded(
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
            item: HistoryItem::CompactionResult { id: id.clone(), status, message: None },
        });
    }
    if cancelled {
        Err(AgentError::Aborted)
    } else {
        Ok(false)
    }
}

/// 检查摘要：没有被截断且正文非空才能使用。
fn validate_summary(response: ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidSummary(
            "summary reached the output limit (incomplete checkpoint)".into(),
        ));
    }
    let text = response.assistant_message.content_text();
    if text.trim().is_empty() {
        return Err(AgentError::InvalidSummary("summary contains no text".into()));
    }
    Ok(text)
}
