//! 采样请求管线：装配 → 压缩判定 → 重试包装 → 纯发送。
//!
//! 按「装配（消息、reasoning replay、工具 schema、预算估算）→ 发送前主动
//! 压缩判定（`prepare_request`）→ agent 层重试包装（`sample_request`）→
//! 纯发送（`stream_completion`）」的顺序组织；全部实现挂在 [`Agent`] 上，
//! 供 loop 的轮步编排调用。正常采样与 compaction 摘要请求经
//! [`send_with_retry`] 复用同一传输策略与同一 durable attempt ledger：
//! 每次实际出站请求之前先 durable 落盘 `step_attempt`（失败阻止请求并走
//! typed [`SendOutcome::Store`] 路径，绝不静默吞错），出站返回后落盘
//! `provider_attempt` 终态观测——provider 上报的终态观测在 durable 落盘
//! 成功后才发布 provider/attempt 终态投影；重试产生新 attempt，一次实际
//! 请求恒对应一条连续可审计 attempt，绝不隐藏第二次执行。

use singularity_core::CancellationToken;
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, Provider, ProviderAttemptEvent, ProviderError,
    ProviderStreamEvent, TurnRetryPolicy,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::compaction::{CompactionError, CompactionOutcome};
use crate::message::{AgentMessage, AgentMessageRole};
use crate::session::context::entry_to_llm_messages;
use crate::session::{SessionEntry, SessionError, SessionWriter, lock_writer};

use super::events::{
    AgentDiagnostic, AgentEvent, AgentEvents, diagnostic_code, emit, emit_diagnostic,
};
use super::{Agent, AgentError, Result};

/// 退避等待的取消轮询间隔。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

/// 输出预算的安全垫（token）：上下文计量只覆盖会话条目，不含本次装配的指令
/// 消息，也不含端点自己分词与我们估算的差值；预留这段后才向端点声明输出上限，
/// 使「实际提示 + 声明的输出上限」不会越过上下文窗口。与压缩的 `reserve_tokens`
/// 分处两层——那个决定何时摘要历史，这个只约束一次请求声明多少输出预算。
const REQUEST_OUTPUT_SAFETY_TOKENS: u64 = 4_096;

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

/// `send_with_retry` 的结果：响应、退避等待期间被取消、最终 provider 失败，
/// 或 durable 记录失败（typed store 路径：请求被阻止或观测未落盘）。
pub(crate) enum SendOutcome {
    Response(Box<ModelTurnResponse>),
    Aborted,
    Failed(ProviderError),
    Store(SessionError),
}

/// 一次 step 的 attempt 追踪器：管理重试 attempt 编号与结果条目 id 预分配。
pub(crate) struct AttemptLedger<'a> {
    writer: &'a SessionWriter,
    attempts: &'a mut u32,
    /// 当前 attempt 预分配的结果条目 id（begin 成功后有效）。
    result_entry_id: String,
    /// 当前 attempt 内暂存的 store 失败（provider 失败后追加可见消息时的落盘失败）。
    store_failure: Option<SessionError>,
    /// 预分配结果 id 已由可见的部分 assistant 文本闭合。
    result_committed: bool,
}

impl<'a> AttemptLedger<'a> {
    pub(crate) fn new(writer: &'a SessionWriter, attempts: &'a mut u32) -> Self {
        Self {
            writer,
            attempts,
            result_entry_id: String::new(),
            store_failure: None,
            result_committed: false,
        }
    }

    /// 当前 attempt 预分配的结果条目 id（begin 成功后有效）。
    pub(crate) fn result_entry_id(&self) -> &str {
        &self.result_entry_id
    }

    /// 共享会话写者引用（供 compaction 引擎读取条目与追加压缩条目）。
    pub(crate) fn writer(&self) -> &SessionWriter {
        self.writer
    }

    fn begin(&mut self) {
        *self.attempts += 1;
        self.store_failure = None;
        self.result_committed = false;
        self.result_entry_id = lock_writer(self.writer).reserve_entry_id();
    }

    /// 将已发布给客户端的可见流式文本落在本 attempt 预分配的 assistant
    /// 结果 id 上。终态由 operation outcome 独立表达，因此该消息保持普通
    /// assistant 形状，不引入第二套 partial 状态。
    fn persist_visible_assistant(&mut self, text: &str) {
        if text.is_empty() || self.result_committed {
            return;
        }
        match lock_writer(self.writer).append_message_with_id(
            &self.result_entry_id,
            AgentMessage::text(AgentMessageRole::Assistant, text),
        ) {
            Ok(_) => self.result_committed = true,
            Err(error) => self.store_failure = Some(error),
        }
    }

    /// 取走当前 attempt 内暂存的 store 失败（追加可见文本时的落盘失败）。
    fn take_store_failure(&mut self) -> Option<SessionError> {
        self.store_failure.take()
    }
}

/// 一次纯发送的 agent 层重试包装：可重试 provider 错误按指数退避重试
///（Retry-After 优先），重试预算按次独立；ContextOverflow 原样上抛交给
/// 调用方处理；退避等待被取消时返回 [`SendOutcome::Aborted`]。
pub(crate) fn send_with_retry<'a>(
    mut attempt: impl FnMut(
        &mut AttemptLedger<'a>,
        &mut AgentEvents,
    ) -> std::result::Result<ModelTurnResponse, ProviderError>,
    _model: &ModelConfigurationSnapshot,
    ledger: &mut AttemptLedger<'a>,
    retry: TurnRetryPolicy,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
) -> SendOutcome {
    let mut retry_attempt = 0u32;
    loop {
        retry_attempt += 1;
        ledger.begin();
        let outcome = attempt(ledger, events);
        if let Some(error) = ledger.take_store_failure() {
            return SendOutcome::Store(error);
        }
        match outcome {
            Ok(response) => return SendOutcome::Response(Box::new(response)),
            Err(error) if error.error.is_context_overflow() => {
                return SendOutcome::Failed(error);
            }
            Err(error) => {
                if ledger.result_committed {
                    return SendOutcome::Failed(error);
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
                        return SendOutcome::Aborted;
                    }
                    continue;
                }
                return SendOutcome::Failed(error);
            }
        }
    }
}

/// 单轮 provider 请求的静态规格：除轮次序号外，一次 `run` 内恒定不变。
/// 输出预算不在这里——它随上下文变化，在装配时按模型声明值与剩余窗口现算
/// （见 [`Agent::output_budget_tokens`]），冻结它只会造出第二个事实源。
pub(super) struct TurnRequestSpec {
    pub(super) tools: Vec<ModelToolSchema>,
    pub(super) turn: u32,
}

/// 单个轮步的采样结果。
pub(crate) enum AttemptOutcome {
    Response(Box<ModelTurnResponse>, String),
    Aborted,
    Failed(AgentError),
}

/// 把系统/开发者指令投影为请求首条消息：恒以 Developer 角色构造，
/// 对不支持 developer 角色的端点由 wire 层按 `supports_developer_role`
/// 降级为 system（用户配置，默认 true）。
pub(crate) fn instruction_message(instruction: &str) -> Option<ModelMessage> {
    if instruction.is_empty() {
        return None;
    }
    Some(ModelMessage::text(ModelRole::Developer, instruction))
}

impl Agent {
    /// 装配单轮请求一次，并在发送前按上一轮真实 usage（缺失时用上下文条目
    /// 估算求和兜底）判定是否主动压缩；实际压缩后基于压缩后的会话重建请求。非
    /// session 压缩失败只发射诊断并跳过压缩，返回原始请求。
    pub(super) fn prepare_request(
        &mut self,
        spec: &TurnRequestSpec,
        outcome: &mut super::AgentOutcome,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let mut request = self.build_request(spec);
        // 唯一计量：usage 基线 + 尾部增量；首轮或 usage 缺失时由装配估算兜底。
        let compaction_tokens = self.context.request_tokens();
        let context_window = self.model.context_window();
        if self.compaction.should_compact(
            compaction_tokens,
            context_window,
            &self.config.compaction,
        ) {
            match self.compact_with_record(compaction_tokens, cancellation) {
                Ok(result) => {
                    if matches!(result, CompactionOutcome::Compacted { .. }) {
                        self.context.rebuild(&lock_writer(&self.session))?;
                        request = self.build_request(spec);
                    }
                }
                Err(CompactionError::Session(error)) => {
                    return Err(AgentError::Session(error));
                }
                // 压缩被取消：与采样取消同形收敛，跳过压缩且不发故障诊断，
                // 由上层取消路径统一收敛。
                Err(CompactionError::Aborted) => {
                    return Err(AgentError::Compaction(CompactionError::Aborted));
                }
                Err(_error) => {
                    outcome.usage_complete = false;
                    emit_diagnostic(
                        events,
                        AgentDiagnostic::warning(
                            diagnostic_code::COMPACTION_SKIPPED,
                            "automatic context compaction skipped".to_string(),
                        ),
                    );
                }
            }
        }
        Ok(request)
    }

    /// 采样层：对一次纯发送做 agent 层重试包装（[`send_with_retry`]）。
    /// 可重试 provider 错误指数退避重试，重试预算按次独立；ContextOverflow
    /// 原样上抛交给轮步层处理；退避等待被取消时返回 Aborted。
    pub(super) fn sample_request(
        &mut self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        let provider = &self.provider;
        let mut ledger = AttemptLedger::new(&self.session, &mut self.assistant_step_attempts);
        let model = &self.model;
        let retry = model.retry;
        let outcome = send_with_retry(
            |ledger, events| {
                stream_completion_once(
                    provider,
                    request,
                    ledger,
                    events,
                    cancellation,
                    model_turn_ordinal,
                )
            },
            model,
            &mut ledger,
            retry,
            events,
            cancellation,
        );
        match outcome {
            SendOutcome::Response(response) => {
                AttemptOutcome::Response(response, ledger.result_entry_id().to_string())
            }
            SendOutcome::Aborted => AttemptOutcome::Aborted,
            SendOutcome::Failed(error) => AttemptOutcome::Failed(AgentError::Provider(error)),
            SendOutcome::Store(error) => AttemptOutcome::Failed(AgentError::Session(error)),
        }
    }

    /// 本次请求可声明的输出上限：`spec.max_output_tokens` 与
    /// 「窗口 − 当前上下文 − 安全垫」的较小者。向端点声明一个窗口放不下的输出
    /// 预算会让兼容端点直接以 400 拒绝整次请求，因此收紧发生在装配处——它是
    /// 上下文变化后唯一真正决定 wire 形状的地方。
    fn output_budget_tokens(&self) -> u32 {
        let declared = self.model.capabilities.max_output_tokens;
        let room = self
            .model
            .context_window()
            .saturating_sub(self.context.request_tokens())
            .saturating_sub(REQUEST_OUTPUT_SAFETY_TOKENS)
            .max(1);
        declared.min(u32::try_from(room).unwrap_or(u32::MAX))
    }

    /// 按 `TurnRequestSpec` 组装单轮 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    pub(super) fn build_request(&self, spec: &TurnRequestSpec) -> ModelTurnRequest {
        let assembled = self.assemble_messages();
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), spec.turn),
            assembled.0,
        );
        request.tools = spec.tools.clone();
        request.provider_reasoning_history = assembled.1;
        request.model_preferences = ModelPreferences {
            model_name: Some(self.model.model.clone()),
            max_output_tokens: Some(self.output_budget_tokens()),
        };
        request
    }

    /// 上下文装配的单一 seam：指令消息 + compaction 感知会话历史 + reasoning
    /// replay 只在此一次完成，全部出自同一 [`ContextView`]。
    pub(super) fn assemble_messages(
        &self,
    ) -> (
        Vec<ModelMessage>,
        Vec<singularity_model::ProviderReasoningReplay>,
    ) {
        let replays = self.reasoning_replays_from_entries(self.context.entries());
        let mut messages = Vec::with_capacity(self.context.entries().len() + 1);
        if let Some(instruction) = instruction_message(&self.config.system_prompt) {
            messages.push(instruction);
        }
        messages.extend(
            self.context
                .entries()
                .iter()
                .flat_map(entry_to_llm_messages),
        );
        (messages, replays)
    }

    /// 从 durable assistant entries 恢复 provider-private continuation。
    ///
    /// replay 只认条目内保存的 opaque continuation（Responses 侧必须是
    /// JSONL 中的 output items 原样；reasoning summary 只作为可见投影）。
    /// 可见 thinking 不用于重建 replay：跨配置伪造绑定会把旧 reasoning 以
    /// 当前 provider 身份发出。绑定身份出自本 turn 冻结的模型快照，不再
    /// 解析 selector 字符串。
    fn reasoning_replays_from_entries(
        &self,
        entries: &[SessionEntry],
    ) -> Vec<singularity_model::ProviderReasoningReplay> {
        let model = &self.model;
        let tool_reasoning_mode = model.capabilities.tool_reasoning_mode;
        let mut replays = Vec::new();
        for entry in entries {
            let SessionEntry::Message { message, .. } = entry else {
                continue;
            };
            if message.role() != AgentMessageRole::Assistant || !message.has_tool_calls() {
                continue;
            }
            let Some(replay) = message.provider_reasoning_replay() else {
                continue;
            };
            // (provider, model[, variant]) 必须与本 turn 的快照一致；不一致的
            // 历史 replay 直接丢弃，绝不以当前身份重放旧 continuation。
            if !replay.is_compatible_with(
                &model.provider,
                &model.model,
                model.reasoning_variant.as_deref(),
                tool_reasoning_mode,
            ) {
                continue;
            }
            replays.push(replay.clone());
        }
        replays
    }
}

/// 流式调用（唯一模型调用形态）。纯发送：不感知压缩、重试与 ContextOverflow。
/// provider 的观测直接作为实时事件发射。
fn stream_completion_once(
    provider: &Arc<dyn Provider + Send + Sync>,
    request: &ModelTurnRequest,
    ledger: &mut AttemptLedger<'_>,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
    model_turn_ordinal: u32,
) -> std::result::Result<ModelTurnResponse, ProviderError> {
    // provider 回调与 on_attempt 共享同一个事件出口；用本地 RefCell 承接
    // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
    // 而为，provider 结果不因投影失败丢弃。
    let events_cell = std::cell::RefCell::new(events);
    let events_ref = &events_cell;
    let mut visible_text = String::new();
    let result = {
        let mut on_stream = |event: ProviderStreamEvent| {
            let ProviderStreamEvent::OutputTextDelta { delta } = event;
            visible_text.push_str(&delta);
            let mut events = events_ref.borrow_mut();
            emit(&mut events, AgentEvent::MessageUpdate { delta });
        };
        let mut observed_attempt = |event: ProviderAttemptEvent| {
            let event = event.with_attempt(*ledger.attempts);
            let mut events = events_ref.borrow_mut();
            emit(
                &mut events,
                AgentEvent::ProviderAttempt {
                    model_turn_ordinal,
                    event,
                },
            );
        };
        provider.complete_stream(request, cancellation, &mut on_stream, &mut observed_attempt)
    };
    if result.is_err() {
        ledger.persist_visible_assistant(&visible_text);
    }
    result
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod request_tests;
