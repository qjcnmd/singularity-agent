//! 采样请求管线：装配 → 压缩判定 → 重试包装 → 纯发送。
//!
//! 按「装配（消息、reasoning replay、工具 schema、预算估算）→ 发送前主动
//! 压缩判定（`prepare_request`）→ agent 层重试包装（`sample_request`）→
//! 纯发送（`stream_completion`）」的顺序组织；全部实现挂在 [`Agent`] 上，
//! 供 loop 的轮步编排调用。正常采样与 compaction 摘要请求经
//! [`send_with_retry`] 复用同一传输策略。

use rand::Rng;
use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptEvent,
    ProviderError, ProviderProtocolContract, ProviderReasoningReplay, ProviderStreamEvent,
    ProviderToolReasoningMode, ToolChoicePolicy, split_model_selector,
};
use uuid::Uuid;

use crate::compaction::{
    CompactionBudget, CompactionError, CompactionOutcome, entry_token_estimate,
};
use crate::message::{AgentMessageRole, ContentBlock};
use crate::session::SessionEntry;
use crate::session::context::entry_to_llm_messages;
use crate::tools::ToolRegistry;

use super::events::{
    AgentDiagnostic, AgentEvent, AgentEvents, diagnostic_code, emit, emit_diagnostic,
};
use super::{Agent, AgentError, Result};

/// Agent 层重试上限：模型调用返回可重试错误时，在此层指数退避重试。
const MAX_TURN_RETRIES: u32 = 3;
/// 重试基础退避毫秒：delay = base × 2^(attempt-1)，再乘 ±10% 抖动。
const TURN_RETRY_BASE_DELAY_MS: u64 = 2_000;
/// 退避等待的取消轮询间隔。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

/// 指数退避 + ±10% 真实随机抖动：每次重试产生独立的随机因子，
/// 避免确定性抖动在多进程或并发重试下共振。
pub(super) fn retry_delay_ms(
    base_delay_ms: u64,
    attempt: u32,
    retry_after: Option<std::time::Duration>,
) -> u64 {
    if let Some(retry_after) = retry_after {
        return retry_after.as_millis().min(u128::from(u64::MAX)) as u64;
    }
    let base = base_delay_ms * 2u64.saturating_pow(attempt.saturating_sub(1));
    // 抖动因子 ∈ [0.90, 1.10)。
    let jitter = rand::rng().random_range(0.9..1.1);
    (base as f64 * jitter) as u64
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

/// `send_with_retry` 的结果：响应、退避等待期间被取消，或最终失败。
pub(crate) enum SendOutcome {
    Response(Box<ModelTurnResponse>),
    Aborted,
    Failed(ProviderError),
}

/// 一次纯发送的 agent 层重试包装：可重试 provider 错误按指数退避重试
///（Retry-After 优先），重试预算按次独立；ContextOverflow 原样上抛交给
/// 调用方处理；退避等待被取消时返回 [`SendOutcome::Aborted`]。正常采样与
/// compaction 摘要请求经同一 helper 复用同一传输策略。
pub(crate) fn send_with_retry(
    mut attempt: impl FnMut(&mut AgentEvents) -> std::result::Result<ModelTurnResponse, ProviderError>,
    retry: super::TurnRetryConfig,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
) -> SendOutcome {
    let mut retry_attempt = 0u32;
    loop {
        match attempt(events) {
            Ok(response) => return SendOutcome::Response(Box::new(response)),
            Err(error) if error.error.is_context_overflow() => {
                return SendOutcome::Failed(error);
            }
            Err(error) => {
                if retry_attempt < retry.max_retries && error.is_retryable() {
                    retry_attempt += 1;
                    let delay_ms =
                        retry_delay_ms(retry.base_delay_ms, retry_attempt, error.retry_after);
                    emit_diagnostic(
                        events,
                        AgentDiagnostic::info(
                            diagnostic_code::PROVIDER_RETRY_SCHEDULED,
                            format!(
                                "provider retry {retry_attempt}/{max} in {delay_ms}ms: {}",
                                error.error.message,
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

/// Agent 层重试配置（pi 策略：可重试 provider 错误指数退避重试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRetryConfig {
    /// 重试上限；0 表示禁用 agent 层重试。
    pub max_retries: u32,
    /// 基础退避毫秒：delay = base × 2^(attempt-1) × 抖动。
    pub base_delay_ms: u64,
}

impl Default for TurnRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_TURN_RETRIES,
            base_delay_ms: TURN_RETRY_BASE_DELAY_MS,
        }
    }
}

/// 单轮 provider 请求的静态规格：除轮次序号外，一次 `run` 内恒定不变。
pub(super) struct TurnRequestSpec {
    pub(super) preferences: ModelPreferences,
    pub(super) tools: Vec<ModelToolSchema>,
    pub(super) tool_choice: ToolChoicePolicy,
    pub(super) max_output_tokens: u32,
    pub(super) turn: u32,
}

/// 单个轮步的采样结果。
pub(super) enum AttemptOutcome {
    Response(Box<ModelTurnResponse>),
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

/// 有效输出上限 = min(配置值, provider 静态能力声明)；正常请求与
/// compaction 摘要请求共用，避免摘要派生出超过模型上限的 max_tokens。
/// 两者都源自 u32 声明，min 结果不会溢出。
pub(super) fn effective_max_output_tokens(provider: &dyn Provider, configured: u64) -> u32 {
    configured.min(provider.protocol_contract().max_output_tokens as u64) as u32
}

/// 会话装配成品：消息、reasoning replay 与上下文内容计量同源于一次
/// `build_context_entries` 遍历。
pub(super) struct AssembledContext {
    pub(super) messages: Vec<ModelMessage>,
    pub(super) replays: Vec<ProviderReasoningReplay>,
    /// 上下文条目的 token 估算求和——请求前上下文规模的唯一内容计量
    /// （对齐 pi `estimateContextTokens`：只合计消息，不附加工具 schema、
    /// 输出预算或固定余量）。
    pub(super) token_estimate: u64,
}

impl Agent {
    /// 装配单轮请求一次，并在发送前按上一轮真实 usage（缺失时用装配估算）
    /// 判定是否主动压缩；实际压缩后基于压缩后的会话重建请求。非 session
    /// 压缩失败只发射诊断并跳过压缩，返回原始请求。
    pub(super) fn prepare_request(
        &mut self,
        spec: &TurnRequestSpec,
        outcome: &mut super::AgentOutcome,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let (mut request, assembled_estimate) = self.build_request(spec)?;
        let budget =
            CompactionBudget::from_config(self.config.context_window, &self.config.compaction);
        // 唯一计量：usage 基线 + 尾部增量；首轮或 usage 缺失时由装配估算兜底。
        let compaction_tokens = self.ledger.estimate().unwrap_or(assembled_estimate);
        if self.compaction.should_compact(compaction_tokens, &budget) {
            match self.compaction.compact(
                &mut self.session,
                &budget,
                compaction_tokens,
                cancellation,
            ) {
                Ok(result) => {
                    super::record_compaction(outcome, &result);
                    if matches!(result, CompactionOutcome::Compacted { .. }) {
                        self.ledger.invalidate();
                        request = self.rebuild_request(spec)?;
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
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> AttemptOutcome {
        match send_with_retry(
            |events| self.stream_completion(request, events, cancellation, model_turn_ordinal),
            self.config.retry,
            events,
            cancellation,
        ) {
            SendOutcome::Response(response) => AttemptOutcome::Response(response),
            SendOutcome::Aborted => AttemptOutcome::Aborted,
            SendOutcome::Failed(error) => AttemptOutcome::Failed(AgentError::Provider(error)),
        }
    }

    /// 流式调用；协议不支持流式（`provider_streaming_unsupported`）时回退 `complete`。
    /// 纯发送：不感知压缩、重试与 ContextOverflow。
    fn stream_completion(
        &self,
        request: &ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        // provider 回调与 on_attempt 共享同一个事件出口；用本地 RefCell 承接
        // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
        // 而为，provider 结果不因投影失败丢弃。
        let events_cell = std::cell::RefCell::new(events);
        let events_ref = &events_cell;
        let mut on_stream = |event: ProviderStreamEvent| {
            let ProviderStreamEvent::OutputTextDelta { delta } = event;
            let mut events = events_ref.borrow_mut();
            emit(&mut events, AgentEvent::MessageUpdate { delta });
        };
        let mut observed_attempt = |event: ProviderAttemptEvent| {
            let mut events = events_ref.borrow_mut();
            emit(
                &mut events,
                AgentEvent::ProviderAttempt {
                    model_turn_ordinal,
                    event,
                },
            );
        };
        match self.provider.complete_stream(
            request,
            cancellation,
            &mut on_stream,
            &mut observed_attempt,
        ) {
            Ok(response) => Ok(response),
            Err(error)
                if error.error.code.as_deref() == Some(PROVIDER_STREAMING_UNSUPPORTED_CODE) =>
            {
                self.provider
                    .complete(request, cancellation, &mut observed_attempt)
            }
            Err(error) => {
                // 保留传输层给出的类型、重放安全性与 Retry-After，交由调用处
                // 的唯一重试策略裁决。
                Err(error)
            }
        }
    }

    /// 按 `TurnRequestSpec` 组装单轮 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    ///
    /// 上下文条目只装配一次：消息、reasoning replay 与内容计量全部在
    /// 同一份装配成品上完成；返回 (请求, 装配成品估算)。
    pub(super) fn build_request(&self, spec: &TurnRequestSpec) -> Result<(ModelTurnRequest, u64)> {
        let assembled = self.assemble_messages()?;
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), spec.turn),
            assembled.messages,
        );
        request.tools = spec.tools.clone();
        request.tool_choice = spec.tool_choice.clone();
        request.provider_reasoning_history = assembled.replays;
        request.model_preferences = ModelPreferences {
            model_name: spec.preferences.model_name.clone(),
            max_output_tokens: Some(spec.max_output_tokens),
        };
        Ok((request, assembled.token_estimate))
    }

    /// 基于当前会话按同一装配 seam 重建请求；只返回请求本身（丢弃装配估算）。
    /// 主动压缩与溢出恢复在会话被修改后用它重建下一次发送的请求。
    pub(super) fn rebuild_request(&self, spec: &TurnRequestSpec) -> Result<ModelTurnRequest> {
        let (request, _estimate) = self.build_request(spec)?;
        Ok(request)
    }

    /// 上下文装配的单一 seam：指令消息 + compaction 感知会话历史 + reasoning
    /// replay 只在此一次完成，`build_request` 与压缩前估算共用同一份装配成品。
    pub(super) fn assemble_messages(&self) -> Result<AssembledContext> {
        let entries = self.session.build_context_entries();
        let token_estimate = entries.iter().map(entry_token_estimate).sum();
        let replays = self.reasoning_replays_from_entries(&entries);
        let mut messages = Vec::with_capacity(entries.len() + 1);
        if let Some(instruction) = instruction_message(&self.config.system_prompt) {
            messages.push(instruction);
        }
        messages.extend(entries.iter().flat_map(entry_to_llm_messages));
        Ok(AssembledContext {
            messages,
            replays,
            token_estimate,
        })
    }

    /// 从 durable assistant entries 恢复 provider-private continuation。
    ///
    /// Responses replay 必须直接使用 JSONL 中保存的 opaque output items；
    /// reasoning summary 只作为可见投影，绝不用于重建 Responses state。
    /// Chat 旧条目若没有 private replay，仍可从 thinking block 重建
    /// `reasoning_content`，以保持已存在会话的兼容性。
    fn reasoning_replays_from_entries(
        &self,
        entries: &[SessionEntry],
    ) -> Vec<ProviderReasoningReplay> {
        let tool_reasoning_mode = self.provider.protocol_contract().tool_reasoning_mode;
        // (provider, model) 必须齐全；变体侧允许双侧同为空（Option 语义由
        // replay 兼容检查判定），无 #variant 的选择器不再静默丢弃 replay。
        let selector = {
            let parts = split_model_selector(&self.config.model);
            match (parts.provider, parts.model) {
                (Some(provider_name), Some(model_name)) => {
                    Some((provider_name, model_name, parts.effort))
                }
                _ => None,
            }
        };
        let mut replays = Vec::new();
        for entry in entries {
            let SessionEntry::Message { message, .. } = entry else {
                continue;
            };
            if message.role() != AgentMessageRole::Assistant || !message.has_tool_calls() {
                continue;
            }
            if let Some(replay) = message.provider_reasoning_replay() {
                // provider/model 切换会使 opaque continuation 失效。保留会话中
                // 可见的 thinking/messages/tool results，但绝不跨不兼容的
                // provider 边界发送私有 replay。
                let Some((provider_name, model_name, variant)) = selector else {
                    continue;
                };
                if replay.is_compatible_with(
                    provider_name,
                    model_name,
                    variant,
                    tool_reasoning_mode,
                ) {
                    replays.push(replay.clone());
                }
                continue;
            }
            if tool_reasoning_mode != ProviderToolReasoningMode::ReplayReasoningContent {
                continue;
            }
            let Some((provider_name, model_name, variant)) = selector else {
                continue;
            };
            let thinking = message
                .thinking_blocks()
                .into_iter()
                .filter_map(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if thinking.is_empty() {
                continue;
            }
            let tool_call_ids: Vec<String> = message
                .tool_calls()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            if tool_call_ids.is_empty() {
                continue;
            }
            replays.push(ProviderReasoningReplay::Chat {
                provider_name: provider_name.to_string(),
                model_name: model_name.to_string(),
                reasoning_effort: variant.map(str::to_string),
                tool_call_ids,
                reasoning_content: thinking,
            });
        }
        replays
    }

    /// 注册表工具 → 模型可见 schema（不超过 provider 单请求上限）。
    pub(super) fn tool_schemas(
        &self,
        capabilities: &ProviderProtocolContract,
    ) -> Vec<ModelToolSchema> {
        Self::tool_schemas_from(&self.registry, capabilities)
    }

    /// [`Self::tool_schemas`] 的无 receiver 形式，供构造期缓存序列化复用。
    pub(super) fn tool_schemas_from(
        registry: &ToolRegistry,
        capabilities: &ProviderProtocolContract,
    ) -> Vec<ModelToolSchema> {
        registry
            .names()
            .into_iter()
            .filter_map(|name| {
                registry
                    .get(name)
                    .map(|spec| (name, spec.description, spec.parameters.clone()))
            })
            .take(capabilities.max_tools_per_request as usize)
            .map(|(name, description, parameters)| ModelToolSchema {
                name: name.to_string(),
                description: description.to_string(),
                parameters_schema: parameters,
            })
            .collect()
    }
}
