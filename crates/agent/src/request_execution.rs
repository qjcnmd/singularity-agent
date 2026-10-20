//! 生成与压缩共用的请求生命周期：attempt 循环与重试等待、attempt 的身份、
//! 落盘的记录、传输、用量和部分输出。请求装配在 `agent::request`，压缩编排在 `agent::compaction`。

use std::time::Instant;

use singularity_core::duration_millis;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, OpenAiProvider,
    ProviderError, ProviderStreamEvent,
};
use singularity_protocol::{ProviderAttemptStatus, RequestObservation, RequestPurpose};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentError;
use crate::events::AgentEvent;
use crate::message::{AgentMessage, ItemScope, public_thinking_text_blocks};
use crate::session::{LedgerRecord, SessionError, SessionWriter, with_writer_async};

/// 声明的输出预算同时受模型上限、请求用途和窗口余量约束。安全余量弥补启发式
/// 估价与端点实际 tokenization 的差异；生成与摘要各自提供本次请求的输入估价。
pub(crate) fn output_budget_tokens(
    model: &ModelConfigurationSnapshot,
    input_tokens: u64,
    requested_output_tokens: u32,
) -> u32 {
    const OUTPUT_SAFETY_TOKENS: u64 = 4_096;
    let window = model.context_window();
    let room = window.saturating_sub(input_tokens).saturating_sub(OUTPUT_SAFETY_TOKENS.min(window / 20));
    model.max_output_tokens.min(requested_output_tokens).min(room as u32)
}

/// 单次请求尝试：编排持久化与传输，持有稳定的输出身份及流式内容。
struct RequestAttempt<'a> {
    writer: &'a SessionWriter,
    on_event: &'a mut (dyn FnMut(AgentEvent) + Send),
    purpose: RequestPurpose,
    /// 流式输出需要在完成前就有稳定的条目 id。
    result_entry_id: String,
    visible_text: String,
    visible_reasoning: String,
}

fn provider_error(error: ProviderError) -> AgentError {
    if error.kind == ModelErrorKind::Cancelled {
        AgentError::Aborted
    } else {
        AgentError::Provider(error)
    }
}

/// 指数退避；Provider 明确返回 Retry-After 时以它的建议为准。
fn retry_delay_ms(base_delay_ms: u64, attempt: u32, retry_after: Option<std::time::Duration>) -> u64 {
    if let Some(retry_after) = retry_after {
        return duration_millis(retry_after);
    }
    base_delay_ms * 2u64.pow(attempt - 1)
}

/// 可立即被中断的异步退避等待；返回 false 表示等待期间被取消。
async fn sleep_abortable(millis: u64, cancellation: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(millis)) => !cancellation.is_cancelled(),
        _ = cancellation.cancelled() => false,
    }
}

/// 普通回复和摘要共用的完整请求边界：发送、重试、用量和结果身份都在这里；每个
/// attempt 各自持有 `RequestAttempt`，重试不复用上一次的结果身份，取消在退避等待中生效。
pub(crate) async fn execute_request(
    provider: &OpenAiProvider,
    session: &SessionWriter,
    request: &ModelTurnRequest,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    cancellation: &CancellationToken,
    purpose: RequestPurpose,
) -> Result<(ModelTurnResponse, String), AgentError> {
    const MAX_ATTEMPTS: u32 = 3;
    const BASE_DELAY_MS: u64 = 2_000;
    let mut retry_attempt = 0u32;
    loop {
        retry_attempt += 1;
        let mut attempt = RequestAttempt {
            writer: session,
            on_event,
            purpose,
            result_entry_id: crate::session::new_entry_id(),
            visible_text: String::new(),
            visible_reasoning: String::new(),
        };
        let error = match attempt.run(provider, request, cancellation).await {
            Ok(response) => return Ok((response, attempt.result_entry_id)),
            Err(error) => error,
        };
        if let AgentError::Provider(provider_error) = &error
            && retry_attempt < MAX_ATTEMPTS
            && provider_error.is_retryable()
        {
            (attempt.on_event)(AgentEvent::MessageDiscarded {
                message_id: attempt.result_entry_id.clone(),
            });
            let delay_ms = retry_delay_ms(BASE_DELAY_MS, retry_attempt, provider_error.retry_after);
            if !sleep_abortable(delay_ms, cancellation).await {
                return Err(AgentError::Aborted);
            }
            continue;
        }
        // 会话写入已失败时不再写中断显示记录。
        if purpose == RequestPurpose::Generation
            && !matches!(error, AgentError::Session(_))
            && let Err(storage) = attempt.finish_interrupted().await
        {
            return Err(AgentError::Session(SessionError::InvalidSession(format!(
                "{error}; 保存失败：{storage}"
            ))));
        }
        return Err(error);
    }
}

impl RequestAttempt<'_> {
    fn on_stream(&mut self, event: ProviderStreamEvent) {
        if self.purpose == RequestPurpose::Compaction {
            return;
        }
        let message_id = self.result_entry_id.clone();
        match event {
            ProviderStreamEvent::OutputTextDelta { delta } => {
                self.visible_text.push_str(&delta);
                (self.on_event)(AgentEvent::MessageUpdate { message_id, delta });
            }
            ProviderStreamEvent::ReasoningTextDelta { delta } => {
                self.visible_reasoning.push_str(&delta);
                (self.on_event)(AgentEvent::ThinkingUpdate { message_id, delta });
            }
            ProviderStreamEvent::ToolCallDelta => {}
        }
    }

    async fn run(
        &mut self,
        provider: &OpenAiProvider,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let (provider_name, model_name) = provider.model_identity();
        let mut observation = RequestObservation {
            request_id: crate::session::new_entry_id(),
            purpose: self.purpose,
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            status: ProviderAttemptStatus::Started,
            duration_ms: 0,
            ttft_ms: None,
            decode_ms: None,
            usage: None,
        };
        let started = Instant::now();
        (self.on_event)(AgentEvent::ProviderAttempt { observation: observation.clone() });
        let mut first_token = None;
        let completion = provider
            .complete_stream(request, cancellation, &mut |event| {
                first_token.get_or_insert_with(Instant::now);
                self.on_stream(event);
            })
            .await;
        let finished = Instant::now();
        let failure = completion.result.as_ref().err();
        observation.status = match failure {
            None => ProviderAttemptStatus::Ok,
            Some(error) if error.kind == ModelErrorKind::Cancelled => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        observation.duration_ms = duration_millis(finished.duration_since(started));
        observation.ttft_ms = first_token.map(|first| duration_millis(first.duration_since(started)));
        observation.decode_ms = first_token.map(|first| duration_millis(finished.duration_since(first)));
        observation.usage = completion.usage;
        if let Err(storage) = self.record(observation).await {
            return Err(match completion.result {
                Err(error) => {
                    AgentError::Session(SessionError::InvalidSession(format!("{error}; 保存失败：{storage}")))
                }
                Ok(_) => AgentError::Session(storage),
            });
        }
        completion.result.map_err(provider_error)
    }

    async fn record(&mut self, observation: RequestObservation) -> Result<(), SessionError> {
        let persisted = observation.clone();
        with_writer_async(self.writer, move |writer| {
            writer.append_record(LedgerRecord::ModelRequest { observation: persisted })
        })
        .await?;
        (self.on_event)(AgentEvent::ProviderAttempt { observation });
        Ok(())
    }

    /// 最终中断时留下的公开内容只用来显示，不会成为模型上下文里的正式消息。
    async fn finish_interrupted(&mut self) -> Result<(), SessionError> {
        let content = public_thinking_text_blocks(
            std::mem::take(&mut self.visible_reasoning),
            std::mem::take(&mut self.visible_text),
        );
        let message = AgentMessage::Assistant { content, provider_reasoning_replay: None };
        let items = message.public_items(&self.result_entry_id, ItemScope::Completion);
        if !items.is_empty() {
            let id = self.result_entry_id.clone();
            with_writer_async(self.writer, move |writer| {
                writer.append_record_with_id(id, LedgerRecord::AssistantInterrupted { message })
            })
            .await?;
        }
        (self.on_event)(AgentEvent::MessageFinished {
            message_id: self.result_entry_id.clone(),
            items,
            failed: true,
        });
        Ok(())
    }
}
