use super::{AgentError, AgentEvent, RequestContext, Result, TurnEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{CompactionEntry, compaction, context, with_writer, with_writer_async};
use model::{Message, ModelTurnRequest, ModelTurnResponse};
use protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_PERCENT: u64 = 90;

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
    AutoAttempted,
}

#[derive(Clone, Copy)]
pub(super) enum CompactionMode {
    Auto,
    Overflow { overflow_input_tokens: u64 },
    Manual,
}

fn precheck_manual_compaction(
    writer: &mut crate::session::SessionManager,
    retain_tokens: u64,
    identity: (&str, &str),
) -> Result<bool> {
    context::clear_idle_tool_results(writer)?;
    Ok(compaction::has_compactable_prefix(writer.data(), retain_tokens, identity))
}

impl RequestContext {
    fn trigger_tokens(&self) -> u64 {
        self.context_window() * AUTO_COMPACTION_TRIGGER_PERCENT / 100
    }

    pub(super) async fn prepare_before_request(
        &self,
        allow_auto_compact: bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<PreRequestOutcome> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let target = self.model_identity();
        let overhead_tokens = self.request_definitions().estimated_tokens;
        let session = std::sync::Arc::clone(&self.session);
        let trigger = self.trigger_tokens();
        let prepared = tokio::task::spawn_blocking(move || -> Result<Option<context::PreparedContext>> {
            with_writer(&session, |writer| {
                context::clear_idle_tool_results(writer)?;
                let data = writer.data();
                let entries = context::resolve_context_entries(data);
                let cleared = context::cleared_tool_result_ids(data);
                let tokens =
                    context::input_tokens(data, &entries, (&target.0, &target.1), overhead_tokens, &cleared);
                if allow_auto_compact && tokens >= trigger {
                    return Ok(None);
                }
                Ok(Some(context::PreparedContext::new(data, &entries, tokens, &cleared)))
            })
        })
        .await
        .expect("pre-request context pressure inspection completes while the runtime is running")?;

        match prepared {
            Some(context) => Ok(PreRequestOutcome::Ready(context)),
            None => match self.compact_now(on_event, cancellation, CompactionMode::Auto).await {
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

    pub async fn compact_manual(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let retain_tokens = compaction::retention_budget_tokens(self.provider.model_configuration());
        let identity = self.model_identity();
        let session = std::sync::Arc::clone(&self.session);
        let has_prefix = tokio::task::spawn_blocking(move || -> Result<bool> {
            with_writer(&session, |writer| {
                precheck_manual_compaction(writer, retain_tokens, (&identity.0, &identity.1))
            })
        })
        .await
        .expect("compaction precheck completes while the runtime is running")?;
        if !has_prefix {
            compaction_unneeded(on_event, cancellation);
            return Ok(());
        }
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        self.ensure_definitions(on_event, cancellation).await?;
        let compacted = self.compact_now(on_event, cancellation, CompactionMode::Manual).await?;
        if !compacted {
            compaction_unneeded(on_event, cancellation);
        }
        Ok(())
    }

    pub(super) async fn compact_now(
        &self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
        mode: CompactionMode,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let (retain_tokens, overflow_input_tokens) = match mode {
            CompactionMode::Overflow { overflow_input_tokens } => (0, Some(overflow_input_tokens)),
            CompactionMode::Auto | CompactionMode::Manual => {
                (compaction::retention_budget_tokens(self.provider.model_configuration()), None)
            }
        };
        let instruction_tokens = context::message_token_estimate(&Message::user(COMPACTION_INSTRUCTION));
        let definitions = self.request_definitions().clone();
        let model = self.provider.model_configuration();
        let identity = self.model_identity();
        let session = std::sync::Arc::clone(&self.session);
        let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
            let step = with_writer(&session, |writer| {
                compaction::plan_compaction_step(
                    writer.data(),
                    retain_tokens,
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
            Ok(Some((request, step.first_kept_entry_id, step.raw_prefix_tokens)))
        })
        .await
        .expect("compaction preparation completes while the runtime is running");

        let (request, first_kept_entry_id, raw_prefix_tokens) = match prepared {
            Ok(Some(step)) => step,
            Ok(None) => {
                if cancellation.is_cancelled() {
                    return Err(AgentError::Aborted);
                }
                return Ok(false);
            }
            Err(error) => {
                if !error.is_session_failure() && cancellation.is_cancelled() {
                    return Err(AgentError::Aborted);
                }
                return Err(emit_failure(mode, None, error, on_event));
            }
        };

        let item_id = crate::session::new_entry_id();
        emit_status(on_event, item_id.clone(), TurnStatus::Running, None);
        let step_result = async {
            let (response, _, _) = execute_request(
                &self.provider,
                &self.session,
                &request,
                on_event,
                cancellation,
                protocol::RequestPurpose::Compaction,
            )
            .await?;
            let summary = validate_summary(&response, raw_prefix_tokens)?;
            if cancellation.is_cancelled() {
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
                on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
                    item: HistoryItem::Compaction { id: item_id, summary },
                }));
                Ok(true)
            }
            Err(error) if error.is_session_failure() => {
                tracing::warn!(error = %error, "context compaction session write failed");
                emit_status(on_event, item_id.clone(), TurnStatus::Failed, Some(error.to_string()));
                Err(AgentError::Compaction { item_id, source: Box::new(error) })
            }
            Err(error) if matches!(error, AgentError::Aborted) || cancellation.is_cancelled() => {
                emit_status(on_event, item_id, TurnStatus::Interrupted, None);
                Err(AgentError::Aborted)
            }
            Err(error) => {
                tracing::warn!(error = %error, "context compaction failed");
                Err(emit_failure(mode, Some(item_id), error, on_event))
            }
        }
    }
}

fn emit_failure(
    mode: CompactionMode,
    item_id: Option<String>,
    error: AgentError,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> AgentError {
    match mode {
        CompactionMode::Manual => {
            let message = error.to_string();
            let id = item_id.unwrap_or_else(crate::session::new_entry_id);
            emit_status(on_event, id.clone(), TurnStatus::Failed, Some(message));
            AgentError::Compaction { item_id: id, source: Box::new(error) }
        }
        CompactionMode::Auto | CompactionMode::Overflow { .. } => {
            if let Some(message_id) = item_id {
                on_event(AgentEvent::Turn(TurnEvent::MessageDiscarded { message_id }));
            }
            error
        }
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

fn compaction_unneeded(on_event: &mut (dyn FnMut(AgentEvent) + Send), cancellation: &CancellationToken) {
    let id = crate::session::new_entry_id();
    let done = if cancellation.is_cancelled() {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    for status in [TurnStatus::Running, done] {
        emit_status(on_event, id.clone(), status, None);
    }
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
