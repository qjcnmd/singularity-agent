use super::{AgentError, AgentEvent, RequestContext, Result, TurnEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{CompactionEntry, CompactionInput, context, with_writer, with_writer_async};
use model::{Message, ModelClient, ModelTurnRequest, ModelTurnResponse};
use protocol::{HistoryItem, ModelSelection, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_PERCENT: u64 = 90;
const MAX_COMPACTION_PASSES: usize = 8;

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

pub(super) enum CompactionMode<'a> {
    Auto,
    Overflow {
        overflow_input_tokens: u64,
    },
    Manual,
    ModelSwitch {
        target: (&'a str, &'a str),
        previous: (&'a str, &'a str),
        trigger: u64,
    },
}

impl RequestContext {
    fn trigger_tokens(&self) -> u64 {
        self.context_window() * AUTO_COMPACTION_TRIGGER_PERCENT / 100
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
        let target = (self.provider.provider_name().to_owned(), self.provider.model_name().to_owned());
        let overhead_tokens = self.request_definitions().estimated_tokens;
        let session = std::sync::Arc::clone(&self.session);
        let target_for_scan = target.clone();
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
                    (&target.0, &target.1),
                    current_tokens,
                    already_switch_compacted,
                    on_event,
                    cancellation,
                )
                .await
            }
            Err((None, _)) => match self.compact_now(on_event, cancellation, CompactionMode::Auto).await {
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

    async fn compact_for_model_switch(
        &self,
        previous_selection: ModelSelection,
        target: (&str, &str),
        current_tokens: u64,
        already_switch_compacted: bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<PreRequestOutcome> {
        let trigger = self.trigger_tokens();
        let still_over = |tokens: u64| {
            switch_invalid(
                target,
                format_args!("切换前压缩后上下文（{tokens} token）仍达到目标模型 90% 阈值（{trigger}）。"),
            )
        };
        if already_switch_compacted {
            return Err(still_over(current_tokens));
        }
        let (prev_provider, prev_model) =
            (previous_selection.provider.clone(), previous_selection.model.clone());
        let models = std::sync::Arc::clone(&self.models);
        let previous_client =
            tokio::task::spawn_blocking(move || ModelClient::new(&models, &previous_selection))
                .await
                .expect("previous model client resolution completes while the runtime is running")
                .map_err(|error| {
                    switch_invalid(
                        target,
                        format_args!(
                            "切换前原模型 {prev_provider}/{prev_model} 不可用（{error}），无法压缩上下文。"
                        ),
                    )
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
            return Err(switch_invalid(
                target,
                format_args!(
                    "当前上下文（{current_tokens} token）已达目标模型 90% 阈值（{trigger}），且无可压缩前缀。"
                ),
            ));
        };
        self.compact_selected_with_client(
            &previous_client,
            input,
            CompactionMode::ModelSwitch {
                target,
                previous: (&prev_provider, &prev_model),
                trigger,
            },
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
            return Err(still_over(after_tokens));
        }
        Ok(PreRequestOutcome::SwitchCompacted)
    }

    pub(super) async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        mode: CompactionMode<'_>,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let retain_tokens = if matches!(mode, CompactionMode::Overflow { .. }) {
            0
        } else {
            context::retention_budget_tokens(self.provider.model_configuration())
        };
        let input = CompactionInput::select(&self.session, retain_tokens).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let Some(input) = input else {
            return Ok(false);
        };
        self.compact_selected_with_client(&self.provider, input, mode, on_event, cancellation, || true)
            .await?;
        Ok(true)
    }

    pub async fn compact_selected(
        &mut self,
        input: CompactionInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        can_commit: impl FnMut() -> bool + Send,
    ) -> Result<()> {
        self.ensure_definitions(on_event, cancellation).await?;
        let mode = CompactionMode::Manual;
        self.compact_selected_with_client(&self.provider, input, mode, on_event, cancellation, can_commit)
            .await
    }

    async fn compact_selected_with_client(
        &self,
        client: &ModelClient,
        input: CompactionInput,
        mode: CompactionMode<'_>,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        mut can_commit: impl FnMut() -> bool + Send,
    ) -> Result<()> {
        let instruction_tokens = context::message_token_estimate(&Message::user(COMPACTION_INSTRUCTION));
        let (overflow_input_tokens, stop_below) = match &mode {
            CompactionMode::Auto => (None, Some(self.trigger_tokens())),
            CompactionMode::ModelSwitch { trigger, .. } => (None, Some(*trigger)),
            CompactionMode::Manual => (None, None),
            CompactionMode::Overflow { overflow_input_tokens } => (
                Some(*overflow_input_tokens),
                context::compaction_input_limit(
                    client.model_configuration(),
                    self.request_definitions().estimated_tokens,
                    instruction_tokens,
                    Some(*overflow_input_tokens),
                )
                .map(|limit| limit.saturating_add(1)),
            ),
        };

        let mut completed_passes = 0usize;
        for _ in 0..MAX_COMPACTION_PASSES {
            if cancellation.is_cancelled() {
                return Err(AgentError::Aborted);
            }
            let definitions = self.request_definitions().clone();
            let model = client.model_configuration();
            let identity = (client.provider_name().to_owned(), client.model_name().to_owned());
            let target_first_kept = input.first_kept_entry_id.clone();
            let session = std::sync::Arc::clone(&self.session);
            let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
                let step = with_writer(&session, |writer| {
                    context::plan_compaction_step(
                        writer.data(),
                        &target_first_kept,
                        (&identity.0, &identity.1),
                        definitions.estimated_tokens,
                        instruction_tokens,
                        model,
                        overflow_input_tokens,
                    )
                })
                .map_err(|msg| AgentError::InvalidCompaction(msg.into()))?;
                let Some(step) = step else {
                    return Ok(None);
                };
                let mut messages = definitions.messages;
                messages.extend(step.messages);
                let images = context::load_images(&mut messages, &step.image_directory)?;
                messages.push(Message::user(COMPACTION_INSTRUCTION));
                let request = ModelTurnRequest {
                    messages,
                    images,
                    tools: definitions.tools,
                    max_output_tokens: output_budget_tokens(model, step.estimated_request_input),
                    prompt_cache_key: Some(step.session_id),
                };
                Ok(Some((request, step.first_kept_entry_id, step.raw_prefix_tokens, step.is_final_chunk)))
            })
            .await
            .expect("compaction preparation completes while the runtime is running");

            let (request, first_kept_entry_id, raw_prefix_tokens, is_final_chunk) = match prepared {
                Ok(Some(step)) => step,
                Ok(None) if completed_passes > 0 => return Ok(()),
                Ok(None) => {
                    let err = AgentError::InvalidCompaction("当前无可压缩的有效上下文内容。".into());
                    return Err(emit_failure(&mode, None, err, on_event));
                }
                Err(error) => return Err(emit_failure(&mode, None, error, on_event)),
            };

            let item_id = crate::session::new_entry_id();
            emit_status(on_event, item_id.clone(), TurnStatus::Running, None);
            let step_result = async {
                let (response, _, _) = execute_request(
                    client,
                    &self.session,
                    &request,
                    on_event,
                    cancellation,
                    protocol::RequestPurpose::Compaction,
                )
                .await?;
                let summary = validate_summary(&response, raw_prefix_tokens)?;
                if !can_commit() || cancellation.is_cancelled() {
                    return Err(AgentError::Aborted);
                }
                let entry = CompactionEntry {
                    summary: summary.clone(),
                    first_kept_entry_id: Some(first_kept_entry_id),
                };
                let id = item_id.clone();
                with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry))
                    .await?;
                Ok(summary)
            }
            .await;

            match step_result {
                Ok(summary) => {
                    completed_passes += 1;
                    on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                        item: HistoryItem::Compaction { id: item_id, summary },
                    }));
                    if is_final_chunk {
                        return Ok(());
                    }
                    if let Some(target_tokens) = stop_below
                        && self.estimated_tokens().await < target_tokens
                    {
                        return Ok(());
                    }
                }
                Err(error) if error.is_session_failure() => {
                    tracing::warn!(error = %error, "context compaction session write failed");
                    emit_status(on_event, item_id.clone(), TurnStatus::Failed, Some(error.to_string()));
                    return Err(AgentError::Compaction { item_id, source: Box::new(error) });
                }
                Err(error)
                    if matches!(error, AgentError::Aborted)
                        || cancellation.is_cancelled()
                        || !can_commit() =>
                {
                    emit_status(on_event, item_id, TurnStatus::Interrupted, None);
                    return Err(AgentError::Aborted);
                }
                Err(error) => {
                    tracing::warn!(error = %error, "context compaction failed");
                    return Err(emit_failure(&mode, Some(item_id), error, on_event));
                }
            }
        }
        let err = AgentError::InvalidCompaction(
            "上下文压缩多轮分段后仍未收敛，请减少单轮输入或选择上下文更大的模型。".into(),
        );
        Err(emit_failure(&mode, None, err, on_event))
    }
}

fn emit_failure(
    mode: &CompactionMode<'_>,
    item_id: Option<String>,
    error: AgentError,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> AgentError {
    let message = match mode {
        CompactionMode::Auto | CompactionMode::Overflow { .. } => None,
        CompactionMode::Manual => Some(error.to_string()),
        CompactionMode::ModelSwitch { target, previous, .. } => Some(switch_error(
            *target,
            format_args!("切换前使用 {}/{} 压缩上下文失败（{error}）。", previous.0, previous.1),
        )),
    };
    if let Some(message) = message {
        let id = item_id.unwrap_or_else(crate::session::new_entry_id);
        emit_status(on_event, id.clone(), TurnStatus::Failed, Some(message.clone()));
        AgentError::Compaction {
            item_id: id,
            source: Box::new(AgentError::InvalidCompaction(message)),
        }
    } else {
        if let Some(message_id) = item_id {
            on_event(AgentEvent::Turn(TurnEvent::MessageDiscarded { message_id }));
        }
        error
    }
}

fn emit_status(
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    id: String,
    status: TurnStatus,
    message: Option<String>,
) {
    on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
        item: HistoryItem::CompactionResult { id, status, message },
    }));
}

fn switch_invalid(target: (&str, &str), detail: impl std::fmt::Display) -> AgentError {
    AgentError::InvalidCompaction(switch_error(target, detail))
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
    let done = if cancellation.is_cancelled() {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    for status in [TurnStatus::Running, done] {
        emit_status(on_event, id.clone(), status, None);
    }
    done
}

fn validate_summary(response: &ModelTurnResponse, prefix_estimated_tokens: u64) -> Result<String> {
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
    let framed = context::message_token_estimate(&context::frame_compacted_summary(&text));
    if framed >= prefix_estimated_tokens {
        return Err(AgentError::InvalidCompaction(format!(
            "压缩摘要未小于原始上下文（{framed} >= {prefix_estimated_tokens}），已保留原上下文。"
        )));
    }
    Ok(text)
}
