//! 普通请求和摘要请求共用的发送、重试、计时与日志写入。普通生成请求在成功、或失败/取消前已产出可见内容时
//! 保存为助手消息（附带请求耗时和用量）；重试尝试、无输出的失败请求和摘要请求只保存 `ModelRequest` 记录。失败和取消的消息不进入后续模型上下文。

use crate::agent::AgentError;
use crate::message::public_items;
use crate::session::{LedgerRecord, SessionError, SessionWriter, with_writer_async};
use singularity_core::duration_millis;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, OpenAiProvider,
    ProviderStreamEvent,
};
use singularity_protocol::{
    ProviderAttemptStatus, RequestObservation, RequestPurpose, TurnEvent, TurnStatus,
};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// 输出上限取模型上限、请求用途与窗口剩余空间的最小值。
pub(crate) fn output_budget_tokens(
    model: &ModelConfigurationSnapshot,
    input_tokens: u64,
    requested_output_tokens: u32,
) -> u32 {
    let room = model.context_window().saturating_sub(input_tokens);
    model.max_output_tokens.min(requested_output_tokens).min(room as u32)
}

/// 发送一次请求，可重试的错误最多尝试三次。每次尝试使用新的消息 ID，等待期间可停止。
pub(crate) async fn execute_request(
    provider: &OpenAiProvider,
    session: &SessionWriter,
    request: &ModelTurnRequest,
    on_event: &mut (dyn FnMut(TurnEvent) + Send),
    cancellation: &CancellationToken,
    purpose: RequestPurpose,
) -> Result<(ModelTurnResponse, String), AgentError> {
    if request.max_output_tokens == 0 {
        return Err(AgentError::Provider(singularity_model::ProviderError::new(
            ModelErrorKind::InvalidRequest,
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
                        ProviderStreamEvent::OutputTextDelta { delta } => on_event(TurnEvent::TextDelta {
                            item_id: crate::session::text_item_id(&id, 0),
                            thinking: false,
                            delta,
                        }),
                        ProviderStreamEvent::ReasoningTextDelta { delta } => on_event(TurnEvent::TextDelta {
                            item_id: crate::session::thinking_item_id(&id, 0),
                            thinking: true,
                            delta,
                        }),
                        ProviderStreamEvent::ToolCallDelta => {}
                    }
                }
            })
            .await;
        let finished = Instant::now();
        let error = response.error().cloned();
        let retry = attempt < 3 && error.as_ref().is_some_and(singularity_model::ProviderError::is_retryable);
        let status = match &error {
            None => ProviderAttemptStatus::Ok,
            Some(error) if error.kind == ModelErrorKind::Cancelled => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        let (provider_name, model_name) = provider.model_identity();
        let observation = RequestObservation {
            purpose,
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            status,
            duration_ms: duration_millis(finished.duration_since(started)),
            ttft_ms: first_token.map(|first| duration_millis(first.duration_since(started))),
            decode_ms: first_token.map(|first| duration_millis(finished.duration_since(first))),
            usage: response.usage.clone(),
        };
        let item_status = match status {
            ProviderAttemptStatus::Ok => TurnStatus::Completed,
            ProviderAttemptStatus::Cancelled => TurnStatus::Interrupted,
            ProviderAttemptStatus::Error => TurnStatus::Failed,
        };
        let items = if purpose == RequestPurpose::Generation && error.is_some() && !retry {
            public_items(
                &response.assistant_message,
                &id,
                item_status,
                singularity_protocol::ToolStatus::Interrupted,
                &singularity_core::now_iso(),
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
            Some(message) => writer.append_message_with_id(&entry_id, message, Some(persisted)),
            None => {
                writer.append_record_with_id(entry_id, LedgerRecord::ModelRequest { observation: persisted })
            }
        })
        .await;
        if let Err(storage) = saved {
            return Err(AgentError::Session(match error {
                Some(error) => SessionError::InvalidSession(format!("{error}; 保存失败：{storage}")),
                None => storage,
            }));
        }
        on_event(TurnEvent::ProviderAttempt { observation: observation.clone() });
        let Some(error) = error else {
            return Ok((response, id));
        };
        if retry {
            on_event(TurnEvent::MessageDiscarded { message_id: id });
            let delay = error
                .retry_after
                .unwrap_or_else(|| std::time::Duration::from_millis(2_000 * 2u64.pow(attempt - 1)));
            tokio::select! {
                _ = tokio::time::sleep(delay) => {},
                _ = cancellation.cancelled() => return Err(AgentError::Aborted),
            }
            continue;
        }
        if purpose == RequestPurpose::Generation {
            on_event(TurnEvent::MessageFinished { message_id: id, items });
        }
        return Err(if error.kind == ModelErrorKind::Cancelled {
            AgentError::Aborted
        } else {
            AgentError::Provider(error)
        });
    }
    unreachable!("the last attempt returns its response or error")
}
