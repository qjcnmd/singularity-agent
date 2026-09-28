//! 生成与压缩共用的请求生命周期：attempt 循环与重试等待、attempt 的身份、
//! 必须落盘的记录、传输、用量和部分输出。请求装配与压缩编排在 `agent::request`。

use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent,
    ProviderCallError, ProviderObserver, ProviderStreamEvent,
};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentError;
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::message::{AgentMessage, ItemScope};
use crate::session::{
    LedgerRecord, RequestDefinitions, SessionError, SessionWriter, append_record_async,
    with_writer_async,
};

/// 本次执行的请求次数与已上报用量。
#[derive(Default)]
pub(crate) struct RequestAccounting {
    pub attempts: u32,
    pub usage: ModelUsage,
}

/// 单次请求尝试：统一持有观测身份、输出身份、流式内容和持久化回调。
struct RequestAttempt<'a> {
    writer: &'a SessionWriter,
    accounting: &'a mut RequestAccounting,
    request: &'a ModelTurnRequest,
    on_event: &'a mut (dyn FnMut(AgentEvent) + Send),
    purpose: singularity_protocol::RequestPurpose,
    attempt_id: String,
    /// 流式输出需要在完成前就有稳定的条目 id。
    result_entry_id: String,
    visible_text: String,
    visible_reasoning: String,
}

impl RequestAttempt<'_> {
    /// 最终中断时留下的公开内容只用来显示，不会成为模型上下文里的正式消息。
    async fn finish_interrupted(&mut self) -> Result<(), SessionError> {
        let message = AgentMessage::Assistant {
            content: crate::message::public_thinking_text_blocks(
                std::mem::take(&mut self.visible_reasoning),
                std::mem::take(&mut self.visible_text),
            ),
            provider_reasoning_replay: None,
        };
        let items = message.public_items(&self.result_entry_id, ItemScope::Completion);
        if !items.is_empty() {
            append_record_async(
                self.writer,
                LedgerRecord::AssistantInterrupted {
                    items: items.clone(),
                },
            )
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

impl From<ProviderCallError> for AgentError {
    fn from(error: ProviderCallError) -> Self {
        match error {
            ProviderCallError::Provider(error)
                if error.kind == singularity_model::ModelErrorKind::Cancelled =>
            {
                Self::Aborted
            }
            ProviderCallError::Provider(error) => Self::Provider(error),
            ProviderCallError::Recording { execution, storage } => {
                let storage = match storage.downcast::<SessionError>() {
                    Ok(error) => error,
                    Err(error) => SessionError::Io(error),
                };
                match execution {
                    Some(error) => Self::FailureRecording {
                        execution: Box::new(Self::from(ProviderCallError::Provider(error))),
                        storage,
                    },
                    None => Self::Session(storage),
                }
            }
        }
    }
}

/// 指数退避；Provider 明确返回 Retry-After 时以它的建议为准。
fn retry_delay_ms(
    base_delay_ms: u64,
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> u64 {
    if let Some(retry_after) = retry_after {
        return singularity_core::duration_millis(retry_after);
    }
    base_delay_ms * 2u64.saturating_pow(attempt.saturating_sub(1))
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
// 参数就是 Agent 已有的字段加上本次请求的事实；直接显式传入，不引入包装对象，也不让调用方自己编排 attempt。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_request(
    provider: &(dyn Provider + Send + Sync),
    session: &SessionWriter,
    accounting: &mut RequestAccounting,
    request: &ModelTurnRequest,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    cancellation: &CancellationToken,
    purpose: singularity_protocol::RequestPurpose,
) -> Result<(ModelTurnResponse, String), AgentError> {
    const MAX_ATTEMPTS: u32 = 3;
    const BASE_DELAY_MS: u64 = 2_000;
    let mut retry_attempt = 0u32;
    // 每个 attempt 都在循环内构造，成功分支直接带出这次的身份，不需要跨 attempt 复位。
    loop {
        retry_attempt += 1;
        accounting.attempts += 1;
        let mut attempt = RequestAttempt {
            writer: session,
            accounting,
            request,
            on_event,
            purpose,
            attempt_id: crate::session::new_entry_id(),
            result_entry_id: crate::session::new_entry_id(),
            visible_text: String::new(),
            visible_reasoning: String::new(),
        };
        let error = match provider
            .complete_stream(request, cancellation, &mut attempt)
            .await
        {
            Ok(response) => return Ok((response, attempt.result_entry_id)),
            Err(error) => AgentError::from(error),
        };
        if let AgentError::Provider(provider_error) = &error
            && retry_attempt < MAX_ATTEMPTS
            && provider_error.is_retryable()
        {
            (attempt.on_event)(AgentEvent::MessageDiscarded {
                message_id: attempt.result_entry_id.clone(),
            });
            let delay_ms = retry_delay_ms(BASE_DELAY_MS, retry_attempt, provider_error.retry_after);
            (attempt.on_event)(AgentEvent::Diagnostic(AgentDiagnostic::info(
                diagnostic_code::PROVIDER_RETRY_SCHEDULED,
                format!(
                    "provider request failed with a retryable error; retrying in {delay_ms} ms (attempt {retry_attempt} of {MAX_ATTEMPTS})",
                ),
            )));
            if !sleep_abortable(delay_ms, cancellation).await {
                return Err(AgentError::Aborted);
            }
            continue;
        }
        // 会话写入已失败时不再写中断显示记录。
        if purpose == singularity_protocol::RequestPurpose::Generation
            && !matches!(
                error,
                AgentError::Session(_) | AgentError::FailureRecording { .. }
            )
            && let Err(storage) = attempt.finish_interrupted().await
        {
            return Err(AgentError::FailureRecording {
                execution: Box::new(error),
                storage,
            });
        }
        return Err(error);
    }
}

impl ProviderObserver for RequestAttempt<'_> {
    fn on_stream(&mut self, event: ProviderStreamEvent) {
        if self.purpose == singularity_protocol::RequestPurpose::Compaction {
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

    fn record_attempt<'a>(
        &'a mut self,
        event: ProviderAttemptEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let request_head;
            let mut observation = match event {
                ProviderAttemptEvent::Started(started) => {
                    // 持久化线程只接收轨迹定义，不复制对话和私有续接材料。
                    request_head = Some((
                        RequestDefinitions::from_request(self.request),
                        self.request.model_preferences.clone(),
                    ));
                    singularity_protocol::RequestObservation {
                        request_id: self.attempt_id.clone(),
                        request_head: None,
                        purpose: self.purpose,
                        attempt: self.accounting.attempts,
                        provider: started.provider_name,
                        model: started.model_name,
                        status: singularity_protocol::ProviderAttemptStatus::Started,
                        duration_ms: 0,
                        decode_ms: None,
                        total_tokens: None,
                        input_tokens: None,
                        output_tokens: None,
                        cached_input_tokens: None,
                        error: None,
                        diagnostic_code: None,
                    }
                }
                ProviderAttemptEvent::Finished(occurrence) => {
                    let usage = occurrence
                        .usage
                        .as_ref()
                        .filter(|usage| usage.usage_present);
                    if let Some(usage) = usage {
                        self.accounting.usage.merge(usage);
                    }
                    request_head = None;
                    singularity_protocol::RequestObservation {
                        request_id: self.attempt_id.clone(),
                        request_head: None,
                        purpose: self.purpose,
                        attempt: self.accounting.attempts,
                        provider: occurrence.started.provider_name.clone(),
                        model: occurrence.started.model_name.clone(),
                        status: occurrence.terminal_status,
                        duration_ms: occurrence.attempt_duration_ms,
                        decode_ms: occurrence.decode_ms,
                        total_tokens: usage.map(|usage| usage.total_tokens),
                        input_tokens: usage.map(|usage| usage.input_tokens),
                        output_tokens: usage.map(|usage| usage.output_tokens),
                        cached_input_tokens: usage.and_then(|usage| usage.cached_input_tokens),
                        error: occurrence.error_category.as_ref().map(ToString::to_string),
                        diagnostic_code: occurrence.diagnostic_code.clone(),
                    }
                }
            };
            let saved = with_writer_async(self.writer, move |writer| {
                writer
                    .append_model_request(observation.clone(), request_head)
                    .map(|head| (observation, head))
            })
            .await
            .map_err(std::io::Error::other)?;
            observation = saved.0;
            observation.request_head = saved.1;
            (self.on_event)(AgentEvent::ProviderAttempt { observation });
            Ok(())
        })
    }
}
