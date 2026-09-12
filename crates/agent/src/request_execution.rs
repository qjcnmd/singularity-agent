//! Request lifecycle shared by generation and compaction: attempt identity,
//! required recording, transport, usage, partial output and retry policy.

use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent,
    ProviderCallError, ProviderError, ProviderStreamEvent, TurnRetryPolicy,
};
use std::sync::Arc;

use crate::events::{
    AgentDiagnostic, AgentEvent, AgentEvents, diagnostic_code, emit, emit_diagnostic,
};
use crate::message::{AgentMessage, ContentBlock};
use crate::session::{SessionError, SessionWriter, lock_writer};

const RETRY_POLL_INTERVAL_MS: u64 = 50;
/// Allows for the difference between heuristic estimates and provider tokenization.
const REQUEST_OUTPUT_SAFETY_TOKENS: u64 = 4_096;

/// 正常响应与摘要共享剩余窗口预算；零表示不能再发送该请求。
pub(crate) fn output_token_budget(window: u64, pressure: u64, declared: u32) -> u32 {
    let room = window
        .saturating_sub(pressure)
        .saturating_sub(REQUEST_OUTPUT_SAFETY_TOKENS.min(window / 20));
    declared.min(u32::try_from(room).unwrap_or(u32::MAX))
}

/// 指数退避；Provider 明确返回 Retry-After 时优先服从其建议。
pub(super) fn retry_delay_ms(
    base_delay_ms: u64,
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> u64 {
    if let Some(retry_after) = retry_after {
        return singularity_model::duration_millis(retry_after);
    }
    base_delay_ms * 2u64.saturating_pow(attempt.saturating_sub(1))
}

/// 可中断的同步退避等待；返回 false 表示等待期间被取消。
fn sleep_abortable(millis: u64, cancellation: &CancellationToken) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
    while std::time::Instant::now() < deadline {
        if cancellation.is_cancelled() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(RETRY_POLL_INTERVAL_MS));
    }
    !cancellation.is_cancelled()
}

/// 一次 step 的 attempt 追踪器：管理重试 attempt 编号与结果条目 id 预分配。
pub(crate) struct RequestAccounting {
    pub attempts: u32,
    pub usage: ModelUsage,
    pub complete: bool,
}

impl Default for RequestAccounting {
    fn default() -> Self {
        Self {
            attempts: 0,
            usage: ModelUsage::default(),
            complete: true,
        }
    }
}

impl RequestAccounting {
    fn observe(&mut self, usage: Option<&ModelUsage>) {
        match usage.filter(|usage| usage.usage_present) {
            Some(usage) => self.usage.merge(usage),
            None => self.complete = false,
        }
    }
}

pub(crate) struct AttemptLedger<'a> {
    writer: &'a SessionWriter,
    accounting: &'a mut RequestAccounting,
    /// 当前 attempt 预分配的结果条目 id（begin 成功后有效）。
    result_entry_id: String,
    /// 预分配结果 id 已由可见的部分 assistant 文本闭合。
    result_committed: bool,
}

impl<'a> AttemptLedger<'a> {
    pub(crate) fn new(writer: &'a SessionWriter, accounting: &'a mut RequestAccounting) -> Self {
        Self {
            writer,
            accounting,
            result_entry_id: String::new(),
            result_committed: false,
        }
    }

    /// 当前 attempt 预分配的结果条目 id（begin 成功后有效）。
    pub(crate) fn result_entry_id(&self) -> &str {
        &self.result_entry_id
    }

    fn begin(&mut self) {
        self.accounting.attempts += 1;
        self.result_committed = false;
        self.result_entry_id = crate::session::new_entry_id();
    }

    /// 将已发布给客户端的可见流式文本落在本 attempt 预分配的 assistant
    /// 结果 id 上。终态由 operation outcome 独立表达，因此该消息保持普通
    /// assistant 形状，不引入第二套 partial 状态。
    fn persist_visible_assistant(
        &mut self,
        text: &str,
        reasoning: &str,
    ) -> Result<(), SessionError> {
        if (text.is_empty() && reasoning.is_empty()) || self.result_committed {
            return Ok(());
        }
        let mut content = Vec::new();
        if !reasoning.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: reasoning.to_string(),
                signature: None,
            });
        }
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
        lock_writer(self.writer).append_message_with_id(
            &self.result_entry_id,
            AgentMessage::Assistant {
                content,
                stop_reason: None,
                provider_reasoning_replay: None,
            },
        )?;
        self.result_committed = true;
        Ok(())
    }
}

pub(crate) enum RequestExecutionError {
    Aborted,
    Provider(ProviderError),
    Session(SessionError),
}

impl From<ProviderCallError> for RequestExecutionError {
    fn from(error: ProviderCallError) -> Self {
        match error {
            ProviderCallError::Provider(error) => Self::Provider(error),
            ProviderCallError::Recording(error) => {
                Self::Session(match error.downcast::<SessionError>() {
                    Ok(error) => error,
                    Err(error) => SessionError::Io(error),
                })
            }
        }
    }
}

/// 一次纯发送的 agent 层重试包装：可重试 provider 错误按指数退避重试
///（Retry-After 优先），重试预算按次独立；ContextOverflow 原样上抛交给
/// 调用方处理；退避等待被取消时返回 Aborted。
pub(crate) fn send_with_retry<'a>(
    mut attempt: impl FnMut(
        &mut AttemptLedger<'a>,
        &mut AgentEvents,
    ) -> Result<ModelTurnResponse, RequestExecutionError>,
    ledger: &mut AttemptLedger<'a>,
    retry: TurnRetryPolicy,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
) -> Result<Box<ModelTurnResponse>, RequestExecutionError> {
    let mut retry_attempt = 0u32;
    loop {
        retry_attempt += 1;
        ledger.begin();
        match attempt(ledger, events) {
            Ok(response) => return Ok(Box::new(response)),
            Err(error @ (RequestExecutionError::Session(_) | RequestExecutionError::Aborted)) => {
                return Err(error);
            }
            Err(RequestExecutionError::Provider(error)) if error.is_context_overflow() => {
                return Err(RequestExecutionError::Provider(error));
            }
            Err(RequestExecutionError::Provider(error)) => {
                if ledger.result_committed {
                    return Err(RequestExecutionError::Provider(error));
                }
                if retry_attempt < retry.max_retries && error.is_retryable() {
                    let delay_ms =
                        retry_delay_ms(retry.base_delay_ms, retry_attempt, error.retry_after);
                    emit_diagnostic(
                        events,
                        AgentDiagnostic::info(
                            diagnostic_code::PROVIDER_RETRY_SCHEDULED,
                            format!(
                                "provider request failed with a retryable error; retrying in {delay_ms} ms (attempt {retry_attempt} of {max})",
                                max = retry.max_retries,
                            ),
                        ),
                    );
                    if !sleep_abortable(delay_ms, cancellation) {
                        return Err(RequestExecutionError::Aborted);
                    }
                    continue;
                }
                return Err(RequestExecutionError::Provider(error));
            }
        }
    }
}

/// Commit attempt records around transport, then publish their public facts.
/// Generation retains visible partial output; summaries never enter the chat stream.
pub(crate) fn stream_completion_once(
    provider: &Arc<dyn Provider + Send + Sync>,
    request: &mut ModelTurnRequest,
    ledger: &mut AttemptLedger<'_>,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
    model_turn_ordinal: u32,
    purpose: singularity_protocol::RequestPurpose,
) -> Result<ModelTurnResponse, RequestExecutionError> {
    request.request_id = ledger.result_entry_id().to_string();
    let request = &*request;
    // provider 回调与 record_attempt 共享同一个事件出口；用本地 RefCell 承接
    // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
    // 而为，provider 结果不因投影失败丢弃。
    let events_cell = std::cell::RefCell::new(events);
    let events_ref = &events_cell;
    let mut visible_text = String::new();
    let mut visible_reasoning = String::new();
    let message_id = ledger.result_entry_id().to_string();
    let mut observed = false;
    let mut started = false;
    let result = {
        let mut on_stream = |event: ProviderStreamEvent| {
            if purpose == singularity_protocol::RequestPurpose::Compaction {
                return;
            }
            let mut events = events_ref.borrow_mut();
            match event {
                ProviderStreamEvent::OutputTextDelta { delta } => {
                    visible_text.push_str(&delta);
                    emit(
                        &mut events,
                        AgentEvent::MessageUpdate {
                            message_id: message_id.clone(),
                            delta,
                        },
                    );
                }
                ProviderStreamEvent::ReasoningTextDelta { delta } => {
                    visible_reasoning.push_str(&delta);
                    emit(
                        &mut events,
                        AgentEvent::ThinkingUpdate {
                            message_id: message_id.clone(),
                            delta,
                        },
                    );
                }
            }
        };
        let mut record_attempt = |event: ProviderAttemptEvent| -> std::io::Result<()> {
            let (provider, model, status, duration_ms, usage, error) = match &event {
                ProviderAttemptEvent::Started(started) => (
                    &started.provider_name,
                    &started.model_name,
                    singularity_protocol::ProviderAttemptStatus::Started,
                    0,
                    None,
                    None,
                ),
                ProviderAttemptEvent::Finished(occurrence) => {
                    observed = true;
                    ledger.accounting.observe(occurrence.usage.as_ref());
                    (
                        &occurrence.provider_name,
                        &occurrence.model_name,
                        occurrence.terminal_status,
                        occurrence.attempt_duration_ms,
                        occurrence
                            .usage
                            .as_ref()
                            .filter(|usage| usage.usage_present),
                        occurrence.error_category.as_ref().map(ToString::to_string),
                    )
                }
            };
            let is_start = matches!(&event, ProviderAttemptEvent::Started(_));
            let mut observation = singularity_protocol::RequestObservation {
                request_id: request.request_id.clone(),
                request_head: None,
                purpose,
                ordinal: model_turn_ordinal,
                attempt: ledger.accounting.attempts,
                provider: provider.clone(),
                model: model.clone(),
                status,
                duration_ms,
                input_tokens: usage.map(|usage| usage.input_tokens),
                output_tokens: usage.map(|usage| usage.output_tokens),
                cached_input_tokens: usage
                    .filter(|usage| usage.cached_input_tokens_present)
                    .map(|usage| usage.cached_input_tokens),
                error,
                request_error: None,
            };
            let saved_head = {
                let mut writer = lock_writer(ledger.writer);
                if let Err(error) =
                    writer.append_model_request(observation.clone(), is_start.then_some(request))
                {
                    return Err(std::io::Error::other(error));
                }
                is_start
                    .then(|| writer.request_head(&request.request_id))
                    .transpose()
            };
            observation.request_head = match saved_head {
                Ok(head) => head,
                Err(error) => {
                    emit_diagnostic(
                        &mut events_ref.borrow_mut(),
                        AgentDiagnostic::warning(
                            "request_observation_unavailable",
                            format!("request details could not be read: {error}"),
                        ),
                    );
                    None
                }
            };
            let (protocol, diagnostic_code, retry_after_ms, retry_after_source) = match event {
                ProviderAttemptEvent::Started(event) => {
                    started = true;
                    (event.actual_api_protocol, None, None, None)
                }
                ProviderAttemptEvent::Finished(event) => (
                    event.actual_api_protocol,
                    event.diagnostic_code,
                    event.retry_after_ms,
                    event.retry_after_source,
                ),
            };
            emit(
                &mut events_ref.borrow_mut(),
                AgentEvent::ProviderAttempt {
                    observation,
                    protocol: protocol.to_string(),
                    diagnostic_code,
                    retry_after_ms,
                    retry_after_source,
                },
            );
            Ok(())
        };
        provider.complete_stream(request, cancellation, &mut on_stream, &mut record_attempt)
    };
    if !observed && (started || result.is_ok()) {
        ledger
            .accounting
            .observe(result.as_ref().ok().map(|response| &response.usage));
    }
    let result = result.map_err(RequestExecutionError::from);
    if result.is_err() && purpose == singularity_protocol::RequestPurpose::Generation {
        let persisted = ledger.persist_visible_assistant(&visible_text, &visible_reasoning);
        emit(
            &mut events_cell.borrow_mut(),
            AgentEvent::MessageFinished {
                message_id,
                failed: true,
            },
        );
        if !matches!(result, Err(RequestExecutionError::Session(_))) {
            persisted.map_err(RequestExecutionError::Session)?;
        }
    }
    result
}

#[cfg(test)]
mod tests;
