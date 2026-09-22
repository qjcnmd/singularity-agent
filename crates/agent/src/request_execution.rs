//! 生成与压缩共用的请求生命周期：attempt 循环与重试等待、attempt 的身份、
//! 必须落盘的记录、传输、用量和部分输出。请求装配与压缩编排在 `agent::request`。

use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent,
    ProviderCallError, ProviderStreamEvent,
};

use crate::agent::AgentError;
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::message::{AgentMessage, ItemScope};
use crate::session::{LedgerRecord, SessionError, SessionWriter, lock_writer};

/// 一次执行范围内的请求尝试与用量汇总：累计本 turn 的 attempt 次数、各次
/// provider usage，以及这些 usage 是否覆盖了全部尝试（complete）。
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

/// 单次 attempt 的账本：一个对象只对应一次有效 attempt，从构造到丢弃身份不变。
pub(crate) struct AttemptLedger<'a> {
    writer: &'a SessionWriter,
    accounting: &'a mut RequestAccounting,
    /// 本次 attempt 预分配的结果条目 id，构造时就已可用。
    result_entry_id: String,
    visible_text: String,
    visible_reasoning: String,
}

impl<'a> AttemptLedger<'a> {
    /// 构造即开始一次 attempt：登记 accounting.attempts 并拿到结果条目 id。
    pub(crate) fn new(writer: &'a SessionWriter, accounting: &'a mut RequestAccounting) -> Self {
        accounting.attempts += 1;
        Self {
            writer,
            accounting,
            result_entry_id: crate::session::new_entry_id(),
            visible_text: String::new(),
            visible_reasoning: String::new(),
        }
    }

    pub(crate) fn result_entry_id(&self) -> &str {
        &self.result_entry_id
    }

    /// 最终中断时留下的公开内容只用来显示，不会成为模型上下文里的正式消息。
    fn finish_interrupted(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), SessionError> {
        let message = AgentMessage::Assistant {
            content: crate::message::public_thinking_text_blocks(
                std::mem::take(&mut self.visible_reasoning),
                std::mem::take(&mut self.visible_text),
            ),
            stop_reason: None,
            provider_reasoning_replay: None,
        };
        let items = message.public_items(&self.result_entry_id, ItemScope::Completion);
        if !items.is_empty() {
            lock_writer(self.writer).append_record(LedgerRecord::AssistantInterrupted {
                items: items.clone(),
            })?;
        }
        on_event(AgentEvent::MessageFinished {
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
            ProviderCallError::Recording(error) => {
                Self::Session(match error.downcast::<SessionError>() {
                    Ok(error) => error,
                    Err(error) => SessionError::Io(error),
                })
            }
        }
    }
}

/// 重试退避等待的轮询间隔；等待期间以这个粒度检查是否被取消。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

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

/// 可以被中断的同步退避等待；返回 false 表示等待期间被取消。
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

/// 普通回复和摘要共用的完整请求边界：发送、重试、用量和结果身份都在这里；每个
/// attempt 各自持有 `AttemptLedger`，重试不复用上一次的结果身份，取消在退避等待中生效。
// 参数就是 Agent 已有的字段加上本次请求的事实；直接显式传入，不引入包装对象，也不让调用方自己编排 attempt。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_request(
    provider: &(dyn Provider + Send + Sync),
    session: &SessionWriter,
    accounting: &mut RequestAccounting,
    request: &mut ModelTurnRequest,
    on_event: &mut dyn FnMut(AgentEvent),
    cancellation: &CancellationToken,
    model_turn_ordinal: u32,
    purpose: singularity_protocol::RequestPurpose,
) -> Result<(ModelTurnResponse, String), AgentError> {
    const MAX_ATTEMPTS: u32 = 3;
    const BASE_DELAY_MS: u64 = 2_000;
    let mut retry_attempt = 0u32;
    // 每个 attempt 都在循环内构造，成功分支直接带出这次的身份，不需要跨 attempt 复位。
    let (response, result_entry_id) = loop {
        retry_attempt += 1;
        let mut ledger = AttemptLedger::new(session, accounting);
        let error = match stream_completion_once(
            provider,
            request,
            &mut ledger,
            on_event,
            cancellation,
            model_turn_ordinal,
            purpose,
        ) {
            Ok(response) => break (response, ledger.result_entry_id().to_string()),
            Err(error) => error,
        };
        if let AgentError::Provider(provider_error) = &error
            && retry_attempt < MAX_ATTEMPTS
            && provider_error.is_retryable()
        {
            on_event(AgentEvent::MessageDiscarded {
                message_id: ledger.result_entry_id().to_string(),
            });
            let delay_ms = retry_delay_ms(BASE_DELAY_MS, retry_attempt, provider_error.retry_after);
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::info(
                diagnostic_code::PROVIDER_RETRY_SCHEDULED,
                format!(
                    "provider request failed with a retryable error; retrying in {delay_ms} ms (attempt {retry_attempt} of {MAX_ATTEMPTS})",
                ),
            )));
            if !sleep_abortable(delay_ms, cancellation) {
                return Err(AgentError::Aborted);
            }
            continue;
        }
        // 会话写入已失败时不再写中断显示记录。
        if purpose == singularity_protocol::RequestPurpose::Generation
            && !matches!(error, AgentError::Session(_))
        {
            ledger.finish_interrupted(on_event)?;
        }
        return Err(error);
    };
    Ok((response, result_entry_id))
}

/// 在传输前后提交 attempt 记录，再发布对应的公开事实；生成增量在完成、重试或最终中断前只是临时状态，摘要不进入对话流。
pub(crate) fn stream_completion_once(
    provider: &(dyn Provider + Send + Sync),
    request: &mut ModelTurnRequest,
    ledger: &mut AttemptLedger<'_>,
    on_event: &mut dyn FnMut(AgentEvent),
    cancellation: &CancellationToken,
    model_turn_ordinal: u32,
    purpose: singularity_protocol::RequestPurpose,
) -> Result<ModelTurnResponse, AgentError> {
    request.request_id = ledger.result_entry_id().to_string();
    let request = &*request;
    // provider 回调与 record_attempt 共用同一个事件出口；两个回调签名不同，用本地 RefCell
    // 承接可变借用（单线程 turn 内串行使用）。事件投递尽力而为，provider 结果不会因投递失败被丢掉。
    let events_cell = std::cell::RefCell::new(on_event);
    let events_ref = &events_cell;
    let message_id = ledger.result_entry_id().to_string();
    let visible_text = &mut ledger.visible_text;
    let visible_reasoning = &mut ledger.visible_reasoning;
    let result = {
        let mut on_stream = |event: ProviderStreamEvent| {
            if purpose == singularity_protocol::RequestPurpose::Compaction {
                return;
            }
            let mut sink = events_ref.borrow_mut();
            match event {
                ProviderStreamEvent::OutputTextDelta { delta } => {
                    visible_text.push_str(&delta);
                    (**sink)(AgentEvent::MessageUpdate {
                        message_id: message_id.clone(),
                        delta,
                    });
                }
                ProviderStreamEvent::ReasoningTextDelta { delta } => {
                    visible_reasoning.push_str(&delta);
                    (**sink)(AgentEvent::ThinkingUpdate {
                        message_id: message_id.clone(),
                        delta,
                    });
                }
                ProviderStreamEvent::ToolCallDelta => {}
            }
        };
        let mut record_attempt = |event: ProviderAttemptEvent| -> std::io::Result<()> {
            // 用量记账在 Finished 分支完成，每个 Finished 只记一次；request head 只有 Started
            // 分支才有。发布需要的协议与重试事实也在分支中一并取出。
            let request_head;
            let protocol;
            let retry_after_ms;
            let mut observation = match event {
                ProviderAttemptEvent::Started(started) => {
                    request_head = Some(request);
                    protocol = started.actual_api_protocol;
                    retry_after_ms = None;
                    singularity_protocol::RequestObservation {
                        request_id: request.request_id.clone(),
                        request_head: None,
                        purpose,
                        ordinal: model_turn_ordinal,
                        attempt: ledger.accounting.attempts,
                        provider: started.provider_name.clone(),
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
                        request_error: None,
                    }
                }
                ProviderAttemptEvent::Finished(occurrence) => {
                    ledger.accounting.observe(occurrence.usage.as_ref());
                    let usage = occurrence
                        .usage
                        .as_ref()
                        .filter(|usage| usage.usage_present);
                    request_head = None;
                    protocol = occurrence.started.actual_api_protocol;
                    retry_after_ms = occurrence.retry_after_ms;
                    singularity_protocol::RequestObservation {
                        request_id: request.request_id.clone(),
                        request_head: None,
                        purpose,
                        ordinal: model_turn_ordinal,
                        attempt: ledger.accounting.attempts,
                        provider: occurrence.started.provider_name.clone(),
                        model: occurrence.started.model_name.clone(),
                        status: occurrence.terminal_status,
                        duration_ms: occurrence.attempt_duration_ms,
                        decode_ms: occurrence.decode_ms,
                        total_tokens: usage.map(|usage| usage.total_tokens),
                        input_tokens: usage.map(|usage| usage.input_tokens),
                        output_tokens: usage.map(|usage| usage.output_tokens),
                        cached_input_tokens: usage
                            .filter(|usage| usage.cached_input_tokens_present)
                            .map(|usage| usage.cached_input_tokens),
                        error: occurrence.error_category.as_ref().map(ToString::to_string),
                        diagnostic_code: occurrence.diagnostic_code.clone(),
                        request_error: None,
                    }
                }
            };
            // 先持久化：记录失败就直接返回，公开事件只在落盘成功之后才发布。
            observation.request_head = lock_writer(ledger.writer)
                .append_model_request(observation.clone(), request_head)
                .map_err(std::io::Error::other)?;
            // 再发布：实时事件和历史读取都来自同一份已落盘的观测；诊断码只跟着
            // observation 走，所以重试后最终成功的请求仍能回溯前几次为什么失败。
            (**events_ref.borrow_mut())(AgentEvent::ProviderAttempt {
                observation,
                protocol: protocol.observation_name().to_string(),
                retry_after_ms,
            });
            Ok(())
        };
        provider.complete_stream(request, cancellation, &mut on_stream, &mut record_attempt)
    };
    result.map_err(AgentError::from)
}
