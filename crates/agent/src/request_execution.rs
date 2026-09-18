//! 生成与压缩共用的请求生命周期：attempt 循环与重试等待、attempt 身份、必需
//! 记录、传输、用量与部分输出。请求装配与压缩编排在 `agent::request`。

use singularity_core::CancellationToken;
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent,
    ProviderCallError, ProviderStreamEvent,
};
use std::sync::Arc;

use crate::agent::AgentError;
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::message::{AgentMessage, ItemScope};
use crate::session::{SessionError, SessionWriter, lock_writer};

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
    /// 预分配结果 id 已由可见的部分 assistant 文本闭合。
    result_committed: bool,
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
            result_committed: false,
        }
    }

    /// 本次 attempt 预分配的结果条目 id。
    pub(crate) fn result_entry_id(&self) -> &str {
        &self.result_entry_id
    }

    /// 本次 attempt 是否已把可见部分输出闭合到持久结果。
    pub(crate) fn result_committed(&self) -> bool {
        self.result_committed
    }

    /// 将已发布给客户端的可见流式文本落在本 attempt 预分配的 assistant
    /// 结果 id 上。终态由 operation outcome 独立表达，因此该消息保持普通
    /// assistant 形状，不引入第二套 partial 状态；公开块的规则与正常响应
    /// 共用，stop_reason 与私有续接不在这里伪造。
    fn persist_visible_assistant(
        &mut self,
        text: &str,
        reasoning: &str,
    ) -> Result<Vec<singularity_protocol::HistoryItem>, SessionError> {
        if (text.is_empty() && reasoning.is_empty()) || self.result_committed {
            return Ok(Vec::new());
        }
        // 走到这里必然至少有一块要持久化；空串转换不分配堆内存。
        let content =
            crate::message::public_thinking_text_blocks(reasoning.to_string(), text.to_string());
        let message = AgentMessage::Assistant {
            content,
            stop_reason: None,
            provider_reasoning_replay: None,
        };
        let items = message.public_items(&self.result_entry_id, ItemScope::Completion);
        lock_writer(self.writer).append_message_with_id(&self.result_entry_id, message)?;
        self.result_committed = true;
        Ok(items)
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
/// 已闭合可见部分输出或请求已落盘的失败不再重试，取消在退避等待中生效。
// 参数就是 Agent 的现有字段（provider/session/accounting）加本次请求事实；
// 显式传入而不引入包装对象，也不让调用方承担 attempt 编排。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_request(
    provider: &Arc<dyn Provider + Send + Sync>,
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
        match stream_completion_once(
            provider,
            request,
            &mut ledger,
            on_event,
            cancellation,
            model_turn_ordinal,
            purpose,
        ) {
            Ok(response) => break (response, ledger.result_entry_id().to_string()),
            Err(AgentError::Provider(error)) if error.is_context_overflow() => {
                return Err(AgentError::Provider(error));
            }
            Err(AgentError::Provider(error)) => {
                if ledger.result_committed() {
                    return Err(AgentError::Provider(error));
                }
                // 摘要请求没有对话可见输出：传输层针对「已发射可见增量」加的重试
                // 抑制对摘要不成立。可重试性仍由错误类别决定（结构/校验类失败本来
                // 就不可重试），是否重试仍由本次请求的 attempt 预算决定。
                let mut error = error;
                if purpose == singularity_protocol::RequestPurpose::Compaction {
                    error.automatic_retry_allowed = true;
                }
                if retry_attempt < MAX_ATTEMPTS && error.is_retryable() {
                    let delay_ms = retry_delay_ms(BASE_DELAY_MS, retry_attempt, error.retry_after);
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
                return Err(AgentError::Provider(error));
            }
            Err(error) => return Err(error),
        }
    };
    Ok((response, result_entry_id))
}

/// 在传输前后提交 attempt 记录，然后发布其公开事实。
/// 生成保留可见的部分输出；摘要绝不进入对话流。
pub(crate) fn stream_completion_once(
    provider: &Arc<dyn Provider + Send + Sync>,
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
    let mut visible_text = String::new();
    let mut visible_reasoning = String::new();
    let message_id = ledger.result_entry_id().to_string();
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
            let (provider, model, status, duration_ms, usage, error, diagnostic_code) = match &event
            {
                ProviderAttemptEvent::Started(started) => (
                    &started.provider_name,
                    &started.model_name,
                    singularity_protocol::ProviderAttemptStatus::Started,
                    0,
                    None,
                    None,
                    None,
                ),
                ProviderAttemptEvent::Finished(occurrence) => {
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
                        occurrence.diagnostic_code.clone(),
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
                diagnostic_code,
                request_error: None,
            };
            observation.request_head = lock_writer(ledger.writer)
                .append_model_request(observation.clone(), is_start.then_some(request))
                .map_err(std::io::Error::other)?;
            let (protocol, retry_after_ms, retry_after_source) = match event {
                ProviderAttemptEvent::Started(event) => (event.actual_api_protocol, None, None),
                ProviderAttemptEvent::Finished(event) => (
                    event.actual_api_protocol,
                    event.retry_after_ms,
                    event.retry_after_source,
                ),
            };
            (**events_ref.borrow_mut())(AgentEvent::ProviderAttempt {
                // 实时事件与历史读取派生自同一份已落盘观测：诊断码不再只存在于
                // 事件里，重试后最终成功的请求仍能回溯前几次为何失败。
                diagnostic_code: observation.diagnostic_code.clone(),
                observation,
                protocol: protocol.observation_name().to_string(),
                retry_after_ms,
                retry_after_source,
            });
            Ok(())
        };
        provider.complete_stream(request, cancellation, &mut on_stream, &mut record_attempt)
    };
    let mut result = result.map_err(AgentError::from);
    if result.is_err() && purpose == singularity_protocol::RequestPurpose::Generation {
        let items = match ledger.persist_visible_assistant(&visible_text, &visible_reasoning) {
            Ok(items) => items,
            Err(error) => {
                if !matches!(result, Err(AgentError::Session(_))) {
                    result = Err(AgentError::Session(error));
                }
                Vec::new()
            }
        };
        (**events_cell.borrow_mut())(AgentEvent::MessageFinished {
            message_id,
            items,
            failed: true,
        });
    }
    result
}

#[cfg(test)]
mod tests;
