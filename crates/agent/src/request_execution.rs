//! 生成与压缩共用的请求生命周期：attempt 循环与重试等待、attempt 身份、必需
//! 记录、传输、用量与部分输出。请求装配与压缩编排在 `agent::request`。

use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent,
    ProviderCallError, ProviderStreamEvent,
};

use crate::agent::AgentError;
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::message::{AgentMessage, ItemScope};
use crate::session::{LedgerRecord, SessionError, SessionWriter, lock_writer};

/// 一次执行范围内的请求尝试与用量聚合：累计本 turn 的 attempt 次数、各次
/// provider usage，以及这些 usage 是否覆盖了全部尝试（complete）。
/// 结果条目 id 由 AttemptLedger 在其构造/提交窗口内预分配与占用。
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

/// 单次 attempt 的账本：一个对象只对应一次有效 attempt，从构造到丢弃不换身份。
pub(crate) struct AttemptLedger<'a> {
    writer: &'a SessionWriter,
    accounting: &'a mut RequestAccounting,
    /// 本 attempt 预分配的结果条目 id（构造时即有效）。
    result_entry_id: String,
    visible_text: String,
    visible_reasoning: String,
}

impl<'a> AttemptLedger<'a> {
    /// 构造即开始一次 attempt：登记本次 accounting.attempts 并取得结果条目 id，
    /// 不存在“构造成功但不可使用”的中间阶段。
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

    /// 本次 attempt 预分配的结果条目 id。
    pub(crate) fn result_entry_id(&self) -> &str {
        &self.result_entry_id
    }

    /// 最终中断的公开内容只供显示，不成为模型上下文中的正式消息。
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

/// 重试退避等待的轮询间隔；等待期间按此粒度检查取消。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

/// 指数退避；Provider 明确返回 Retry-After 时优先服从其建议。
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

/// 普通回复和摘要共用发送、重试、用量与结果身份的完整请求边界。
/// 每个 attempt 各自持有 `AttemptLedger`，因此重试不会复用上一次的结果身份；
/// 重试前丢弃临时输出，最终中断只保留显示记录；取消在退避等待中生效。
// 参数就是 Agent 的现有字段（provider/session/accounting）加本次请求事实；
// 显式传入而不引入包装对象，也不让调用方承担 attempt 编排。
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
    // 每个 attempt 都在循环内构造：构造即登记计数并取得本次结果 id，
    // 成功分支直接带出该次身份，不再跨 attempt 复位。
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
        if purpose == singularity_protocol::RequestPurpose::Generation
            && !matches!(error, AgentError::Session(_))
        {
            ledger.finish_interrupted(on_event)?;
        }
        return Err(error);
    };
    Ok((response, result_entry_id))
}

/// 在传输前后提交 attempt 记录，然后发布其公开事实。
/// 生成增量保持临时状态，直到完成、重试或最终中断；摘要不进入对话流。
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
    // provider 回调与 record_attempt 共享同一个事件出口；用本地 RefCell 承接
    // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
    // 而为，provider 结果不因投影失败丢弃。
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
            }
        };
        let mut record_attempt = |event: ProviderAttemptEvent| -> std::io::Result<()> {
            // 处理事件：按分支直接填写已有的 RequestObservation。Finished 在这里
            // 完成用量记账——每个 Finished 只记账一次；Started 是唯一携带
            // request head 的分支。发布所需的协议与重试事实也只在这一次分支里取出。
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
            // 持久化：记录失败即返回，公开事件只在落盘成功之后发布。
            observation.request_head = lock_writer(ledger.writer)
                .append_model_request(observation.clone(), request_head)
                .map_err(std::io::Error::other)?;
            // 发布：实时事件与历史读取派生自同一份已落盘观测：诊断码只随
            // observation 传递，重试后最终成功的请求仍能回溯前几次为何失败。
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
