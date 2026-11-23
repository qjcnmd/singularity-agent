use crate::agent::{AgentError, AgentEvent};
use crate::message::public_items;
use crate::session::{LedgerRecord, SessionError, SessionWriter, with_writer_async};
use app_core::duration_millis;
use model::{
    ModelClient, ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse,
    ProviderStreamEvent,
};
use protocol::{ProviderAttemptStatus, RequestObservation, RequestPurpose, TurnEvent, TurnStatus};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_RETRY_AFTER: Duration = Duration::from_mins(1);
const MIN_COMPACTION_OUTPUT_FLOOR: u64 = 1024;
const MIN_COMPACTION_OUTPUT_CEIL: u64 = 2048;

pub(crate) fn output_budget_tokens(model: ModelConfigurationSnapshot, input_tokens: u64) -> u32 {
    let room = model.context_window().saturating_sub(input_tokens);
    model.max_output_tokens.min(u32::try_from(room).unwrap_or(u32::MAX))
}

pub(crate) fn min_compaction_output_tokens(model: ModelConfigurationSnapshot) -> u64 {
    let window = model.context_window();
    let proportional = (window / 20).clamp(MIN_COMPACTION_OUTPUT_FLOOR, MIN_COMPACTION_OUTPUT_CEIL);
    let cap = u64::from(model.max_output_tokens).min(window / 4).max(1);
    proportional.min(cap).min(u64::from(model.max_output_tokens))
}

pub(crate) async fn execute_request(
    provider: &ModelClient,
    session: &SessionWriter,
    request: &ModelTurnRequest,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    cancellation: &CancellationToken,
    purpose: RequestPurpose,
) -> Result<(ModelTurnResponse, String, String), AgentError> {
    if request.max_output_tokens == 0 {
        return Err(AgentError::Provider(model::ProviderError::new(
            ModelErrorKind::ContextLengthExceeded,
            "当前输入已占满模型上下文，无法生成回复。请缩短指令、工具定义或消息，或选择上下文更大的模型。",
        )));
    }
    for attempt in 1..=3 {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let id = crate::session::new_entry_id();
        let started = Instant::now();
        let mut first_token = None;
        let response = provider
            .complete_stream(request, cancellation, &mut |event| {
                first_token.get_or_insert_with(Instant::now);
                if purpose == RequestPurpose::Generation {
                    match event {
                        ProviderStreamEvent::OutputTextDelta { delta } => {
                            on_event(AgentEvent::Turn(TurnEvent::TextDelta {
                                item_id: crate::session::text_item_id(&id, 0),
                                thinking: false,
                                delta,
                            }));
                        }
                        ProviderStreamEvent::ReasoningTextDelta { delta } => {
                            on_event(AgentEvent::Turn(TurnEvent::TextDelta {
                                item_id: crate::session::thinking_item_id(&id, 0),
                                thinking: true,
                                delta,
                            }));
                        }
                        ProviderStreamEvent::ToolCallDelta => {}
                    }
                }
            })
            .await;
        let finished = Instant::now();
        let error = response.error().cloned();
        let retry = attempt < 3
            && error
                .as_ref()
                .is_some_and(|e| e.is_retryable() && e.retry_after.is_none_or(|d| d <= MAX_RETRY_AFTER));
        let status = match &error {
            None => ProviderAttemptStatus::Ok,
            Some(error) if error.kind == ModelErrorKind::Cancelled => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        let provider_name = provider.provider_name();
        let model_name = provider.model_name();
        let observation = RequestObservation {
            purpose,
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            status,
            ttft_ms: first_token.map(|first| duration_millis(first.duration_since(started))),
            decode_ms: first_token.map(|first| duration_millis(finished.duration_since(first))),
            usage: response.usage.clone(),
        };
        let item_status = match status {
            ProviderAttemptStatus::Ok => TurnStatus::Completed,
            ProviderAttemptStatus::Cancelled => TurnStatus::Interrupted,
            ProviderAttemptStatus::Error => TurnStatus::Failed,
        };
        let mut items = if purpose == RequestPurpose::Generation && error.is_some() && !retry {
            public_items(
                &response.assistant_message,
                None,
                &id,
                item_status,
                protocol::ToolStatus::Interrupted,
                "",
            )
        } else {
            Vec::new()
        };
        let message =
            (purpose == RequestPurpose::Generation && !retry && (error.is_none() || !items.is_empty()))
                .then(|| response.assistant_message.clone());
        let persisted = observation.clone();
        let entry_id = id.clone();
        let saved = with_writer_async(session, move |writer| match message {
            Some(message) => writer.append_message_with_id(&entry_id, message, Some(persisted), None),
            None => {
                writer.append_record_with_id(entry_id, LedgerRecord::ModelRequest { observation: persisted })
            }
        })
        .await;
        let saved_at = match saved {
            Ok(timestamp) => timestamp,
            Err(storage) => {
                return Err(AgentError::Session(match error {
                    Some(error) => SessionError::Context {
                        message: format!("{error}; 保存失败"),
                        source: Box::new(storage),
                    },
                    None => storage,
                }));
            }
        };
        on_event(AgentEvent::ProviderAttempt { observation });
        let Some(error) = error else {
            return Ok((response, id, saved_at));
        };
        if retry {
            let delay = error
                .retry_after
                .unwrap_or_else(|| std::time::Duration::from_millis(2_000 * 2u64.pow(attempt - 1)));
            tracing::warn!(
                provider = provider_name,
                model = model_name,
                attempt,
                delay_ms = duration_millis(delay),
                error = %error,
                "provider attempt failed; retrying"
            );
            if purpose == RequestPurpose::Generation {
                on_event(AgentEvent::Turn(TurnEvent::MessageDiscarded { message_id: id }));
            }
            tokio::select! {
                () = tokio::time::sleep(delay) => {},
                () = cancellation.cancelled() => return Err(AgentError::Aborted),
            }
            continue;
        }
        if error.kind != ModelErrorKind::Cancelled {
            tracing::warn!(
                provider = provider_name,
                model = model_name,
                attempt,
                error = %error,
                "provider request failed"
            );
        }
        if purpose == RequestPurpose::Generation {
            for item in &mut items {
                if let protocol::HistoryItem::Message { at, .. } = item {
                    *at = Some(saved_at.clone());
                }
            }
            on_event(AgentEvent::Turn(TurnEvent::MessageFinished { message_id: id, items }));
        }
        return Err(if error.kind == ModelErrorKind::Cancelled {
            AgentError::Aborted
        } else {
            AgentError::Provider(error)
        });
    }
    unreachable!("the last attempt returns its response or error")
}
