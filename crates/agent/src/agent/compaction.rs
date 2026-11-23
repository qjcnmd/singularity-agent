//! 上下文压缩：按前缀缓存友好的单次前缀回放请求，将较早历史凝练为结构化检查点。

use super::{AgentError, AgentEvent, RequestContext, Result, TurnEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{CompactionEntry, CompactionInput, context, with_writer, with_writer_async};
use model::{Message, ModelClient, ModelTurnRequest, ModelTurnResponse};
use protocol::{HistoryItem, ModelSelection, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_PERCENT: u128 = 90;

const COMPACTION_INSTRUCTION: &str = r#"You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write "(none)" for an empty section — never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or "(none)"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.
- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure."#;

pub(super) enum PreRequestOutcome {
    Ready(context::PreparedContext),
    SwitchCompacted,
    AutoAttempted,
}

impl RequestContext {
    fn trigger_tokens(&self) -> u64 {
        u64::try_from(u128::from(self.context_window()) * AUTO_COMPACTION_TRIGGER_PERCENT / 100)
            .unwrap_or(u64::MAX)
    }

    async fn estimated_tokens(&self) -> u64 {
        let session = std::sync::Arc::clone(&self.session);
        let identity = (self.provider.provider_name().to_owned(), self.provider.model_name().to_owned());
        let overhead_tokens = self.request_definitions().estimated_tokens;
        tokio::task::spawn_blocking(move || {
            with_writer(&session, |writer| {
                context::estimated_input_tokens(writer.data(), (&identity.0, &identity.1), overhead_tokens)
            })
        })
        .await
        .expect("token estimation completes while the runtime is running")
    }

    /// 单次扫描会话估算下一次请求的上下文占用：
    /// - 切换了模型且已达目标模型 90%：用上一路由模型按目标模型 15% 保留预算预压并复测，失败或仍超阈值则直接中止；
    /// - 同模型（或首次生成）且已达 90%：按 15% 留尾自动压缩一次，可恢复故障仅记警告并清除占位后继续本轮。
    pub(super) async fn prepare_before_request(
        &mut self,
        already_switch_compacted: bool,
        allow_auto_compact: bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<PreRequestOutcome> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let target_provider = self.provider.provider_name().to_owned();
        let target_model = self.provider.model_name().to_owned();
        let overhead_tokens = self.request_definitions().estimated_tokens;
        let session = std::sync::Arc::clone(&self.session);
        let target_for_scan = (target_provider.clone(), target_model.clone());
        let trigger = self.trigger_tokens();
        let prepared = tokio::task::spawn_blocking(move || {
            with_writer(&session, |writer| {
                let data = writer.data();
                let target = (target_for_scan.0.as_str(), target_for_scan.1.as_str());
                let entries = context::resolve_context_entries(data);
                let tokens = context::input_tokens(data, &entries, target, overhead_tokens);
                if tokens >= trigger {
                    let previous = context::last_routed_model(data).filter(|selection| {
                        (selection.provider.as_str(), selection.model.as_str()) != target
                    });
                    if previous.is_some() || allow_auto_compact {
                        return Err((previous, tokens));
                    }
                }
                Ok(context::PreparedContext::new(data, &entries, tokens))
            })
        })
        .await
        .expect("pre-request context pressure inspection completes while the runtime is running");

        match prepared {
            Ok(context) => Ok(PreRequestOutcome::Ready(context)),
            Err((Some(previous_selection), current_tokens)) => {
                self.compact_for_model_switch(
                    previous_selection,
                    (&target_provider, &target_model),
                    current_tokens,
                    trigger,
                    already_switch_compacted,
                    on_event,
                    cancellation,
                )
                .await
            }
            Err((None, _)) => match self.compact_now(on_event, cancellation, true).await {
                Ok(_) => Ok(PreRequestOutcome::AutoAttempted),
                Err(AgentError::Aborted) => Err(AgentError::Aborted),
                Err(error) if error.is_session_failure() => Err(error),
                Err(error) => {
                    tracing::warn!(error = %error, "automatic context compaction failed; continuing turn");
                    Ok(PreRequestOutcome::AutoAttempted)
                }
            },
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal model-switch pre-compaction helper"
    )]
    async fn compact_for_model_switch(
        &self,
        previous_selection: ModelSelection,
        target: (&str, &str),
        current_tokens: u64,
        trigger: u64,
        already_switch_compacted: bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<PreRequestOutcome> {
        if already_switch_compacted {
            return Err(AgentError::InvalidCompaction(switch_error(
                target,
                format_args!(
                    "切换前压缩后上下文（{current_tokens} token）仍达到目标模型 90% 阈值（{trigger}）。"
                ),
            )));
        }
        let prev_provider = previous_selection.provider.clone();
        let prev_model = previous_selection.model.clone();
        let models = std::sync::Arc::clone(&self.models);
        let previous_client =
            tokio::task::spawn_blocking(move || ModelClient::new(&models, &previous_selection))
                .await
                .expect("previous model client resolution completes while the runtime is running")
                .map_err(|error| {
                    AgentError::InvalidCompaction(switch_error(
                        target,
                        format_args!(
                            "切换前原模型 {prev_provider}/{prev_model} 不可用（{error}），无法压缩上下文。"
                        ),
                    ))
                })?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let retain_tokens = context::retention_budget_tokens(self.provider.model_configuration());
        let input = CompactionInput::select(&self.session, retain_tokens).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let Some(input) = input else {
            return Err(AgentError::InvalidCompaction(switch_error(
                target,
                format_args!(
                    "当前上下文（{current_tokens} token）已达目标模型 90% 阈值（{trigger}），且无可压缩前缀。"
                ),
            )));
        };
        let format_failure = |error: &AgentError| {
            switch_error(
                target,
                format_args!("切换前使用 {prev_provider}/{prev_model} 压缩上下文失败（{error}）。"),
            )
        };
        self.compact_selected_with_client(
            &previous_client,
            input,
            Some(&format_failure),
            on_event,
            cancellation,
            || true,
        )
        .await?;
        let after_tokens = self.estimated_tokens().await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if after_tokens >= trigger {
            return Err(AgentError::InvalidCompaction(switch_error(
                target,
                format_args!(
                    "切换前压缩后上下文（{after_tokens} token）仍达到目标模型 90% 阈值（{trigger}）。"
                ),
            )));
        }
        Ok(PreRequestOutcome::SwitchCompacted)
    }

    pub(super) async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        retain_recent: bool,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let retain_tokens = if retain_recent {
            context::retention_budget_tokens(self.provider.model_configuration())
        } else {
            0
        };
        let input = CompactionInput::select(&self.session, retain_tokens).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let Some(input) = input else {
            return Ok(false);
        };
        self.compact_selected_with_client(&self.provider, input, None, on_event, cancellation, || true)
            .await?;
        Ok(true)
    }

    /// 手动压缩由调用方在保存前锁定停止结果；自动压缩不结束整轮停止窗口。
    pub async fn compact_selected(
        &mut self,
        input: CompactionInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        can_commit: impl FnMut() -> bool + Send,
    ) -> Result<()> {
        self.ensure_definitions(on_event, cancellation).await?;
        let format_failure = |error: &AgentError| error.to_string();
        self.compact_selected_with_client(
            &self.provider,
            input,
            Some(&format_failure),
            on_event,
            cancellation,
            can_commit,
        )
        .await
    }

    async fn compact_selected_with_client(
        &self,
        client: &ModelClient,
        input: CompactionInput,
        format_failure: Option<&(dyn Fn(&AgentError) -> String + Sync)>,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        mut can_commit: impl FnMut() -> bool + Send,
    ) -> Result<()> {
        let item_id = crate::session::new_entry_id();
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        }));
        let result = self
            .compact_context(client, input, &item_id, on_event, cancellation, &mut can_commit)
            .await;
        match result {
            Ok(summary) => {
                on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                    item: HistoryItem::Compaction { id: item_id, summary },
                }));
                Ok(())
            }
            // 会话写盘失败必须优先向上传播，不能被并发停止信号掩盖成普通中断。
            Err(error) if error.is_session_failure() => {
                tracing::warn!(error = %error, "context compaction session write failed");
                on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                    item: HistoryItem::CompactionResult {
                        id: item_id.clone(),
                        status: TurnStatus::Failed,
                        message: Some(error.to_string()),
                    },
                }));
                Err(AgentError::Compaction { item_id, source: Box::new(error) })
            }
            Err(error)
                if matches!(error, AgentError::Aborted) || !can_commit() || cancellation.is_cancelled() =>
            {
                on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                    item: HistoryItem::CompactionResult {
                        id: item_id,
                        status: TurnStatus::Interrupted,
                        message: None,
                    },
                }));
                Err(AgentError::Aborted)
            }
            Err(error) => {
                tracing::warn!(error = %error, "context compaction failed");
                if let Some(format_failure) = format_failure {
                    let message = format_failure(&error);
                    on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                        item: HistoryItem::CompactionResult {
                            id: item_id.clone(),
                            status: TurnStatus::Failed,
                            message: Some(message.clone()),
                        },
                    }));
                    Err(AgentError::Compaction {
                        item_id,
                        source: Box::new(AgentError::InvalidCompaction(message)),
                    })
                } else {
                    on_event(AgentEvent::Turn(TurnEvent::MessageDiscarded { message_id: item_id }));
                    Err(error)
                }
            }
        }
    }

    async fn compact_context(
        &self,
        client: &ModelClient,
        input: CompactionInput,
        item_id: &str,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        can_commit: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let definitions = self.request_definitions().clone();
        let model = client.model_configuration();
        let identity = (client.provider_name().to_owned(), client.model_name().to_owned());
        let instruction = Message::user(COMPACTION_INSTRUCTION);
        let instruction_tokens = context::message_token_estimate(&instruction);
        let defs_est = definitions.estimated_tokens;
        let full_est = defs_est.saturating_add(input.total_estimated_tokens);
        let prefix_est = defs_est.saturating_add(input.prefix_estimated_tokens);
        let base_tokens = match &input.latest_usage {
            Some((provider, model_id, total_tokens, tail_tokens))
                if provider == &identity.0 && model_id == &identity.1 && full_est > 0 =>
            {
                let real_full = total_tokens.saturating_add(*tail_tokens);
                u64::try_from(u128::from(real_full) * u128::from(prefix_est) / u128::from(full_est))
                    .unwrap_or(prefix_est)
            }
            _ => prefix_est,
        };
        let estimated_full_input = base_tokens.saturating_add(instruction_tokens);
        let max_output_tokens = output_budget_tokens(model, estimated_full_input);
        let request = tokio::task::spawn_blocking(move || {
            let mut messages = definitions.messages;
            messages.extend(input.messages);
            let images = context::load_images(&mut messages, &input.image_directory)?;
            messages.push(instruction);
            Ok::<_, std::io::Error>(ModelTurnRequest {
                messages,
                images,
                tools: definitions.tools,
                max_output_tokens,
            })
        })
        .await
        .expect("compaction preparation completes while the runtime is running")?;

        let (response, _, _) = execute_request(
            client,
            &self.session,
            &request,
            on_event,
            cancellation,
            protocol::RequestPurpose::Compaction,
        )
        .await?;
        let summary = validate_summary_text(&response)?;
        validate_shrink(&summary, input.prefix_estimated_tokens)?;

        if !can_commit() || cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let entry = CompactionEntry {
            summary: summary.clone(),
            first_kept_entry_id: Some(input.first_kept_entry_id),
        };
        let id = item_id.to_owned();
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        Ok(summary)
    }
}

fn switch_error((target_provider, target_model): (&str, &str), detail: impl std::fmt::Display) -> String {
    format!(
        "无法切换到 {target_provider}/{target_model}：{detail}请换回原模型手动压缩或选择上下文更大的模型。"
    )
}

pub fn compaction_unneeded(
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    cancellation: &CancellationToken,
) -> TurnStatus {
    let id = crate::session::new_entry_id();
    let final_status = if cancellation.is_cancelled() {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    for status in [TurnStatus::Running, final_status] {
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult { id: id.clone(), status, message: None },
        }));
    }
    final_status
}

fn validate_summary_text(response: &ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidCompaction("上下文压缩达到输出上限，摘要不完整。".into()));
    }
    if response.is_refusal() {
        return Err(AgentError::InvalidCompaction("上下文压缩请求被模型拒绝。".into()));
    }
    let text = response.assistant_message.content_text();
    if text.trim().is_empty() {
        return Err(AgentError::InvalidCompaction("上下文压缩没有返回摘要正文。".into()));
    }
    Ok(text)
}

fn validate_shrink(summary: &str, prefix_estimated_tokens: u64) -> Result<()> {
    let framed = context::message_token_estimate(&context::frame_compacted_summary(summary));
    if framed >= prefix_estimated_tokens {
        return Err(AgentError::InvalidCompaction(format!(
            "压缩摘要未小于原始上下文（{framed} >= {prefix_estimated_tokens}），已保留原上下文。"
        )));
    }
    Ok(())
}
