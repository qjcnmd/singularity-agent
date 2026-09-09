//! 采样请求管线：装配 → 压缩判定 → 重试包装 → 纯发送。
//!
//! 按「装配（消息、reasoning replay、工具 schema、预算估算）→ 发送前主动
//! 压缩判定（prepare_request）→ agent 层重试包装（sample_request）→
//! 纯发送（stream_completion）」的顺序组织；全部实现挂在 Agent 上，
//! 供 loop 的轮步编排调用。正常采样与 compaction 摘要请求经
//! send_with_retry 复用传输与重试策略。每次请求的终态统计写入同一
//! 会话 JSONL 的 model_request 记录，成功后发布实时观测；写入失败走
//! SendOutcome::Store。这些观测供轨迹查看，不参与恢复或模型上下文。

use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ModelUsage, Provider, ProviderAttemptEvent, ProviderError,
    ProviderStreamEvent, TurnRetryPolicy,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::compaction::{CompactionError, CompactionOutcome};
use crate::message::{AgentMessage, ContentBlock};
use crate::session::context::entry_to_llm_messages;
use crate::session::{LedgerRecord, SessionEntry, SessionError, SessionWriter, lock_writer};

use super::events::{
    AgentDiagnostic, AgentEvent, AgentEvents, diagnostic_code, emit, emit_diagnostic,
};
use super::{Agent, AgentError, Result};

/// 退避等待的取消轮询间隔。
const RETRY_POLL_INTERVAL_MS: u64 = 50;

/// 输出预算安全垫上限，用于覆盖启发式计量与提供方分词的差异。
/// 系统提示词、工具定义和历史已包含在上下文压力中。
const REQUEST_OUTPUT_SAFETY_TOKENS: u64 = 4_096;

/// 正常响应与摘要共享剩余窗口预算；零表示不能再发送该请求。
pub(crate) fn output_token_budget(window: u64, pressure: u64, declared: u32) -> u32 {
    let room = window
        .saturating_sub(pressure)
        .saturating_sub(REQUEST_OUTPUT_SAFETY_TOKENS.min(window / 20));
    declared.min(u32::try_from(room).unwrap_or(u32::MAX))
}

/// Reserve the normal threshold's remaining window for a response, capped by
/// the model's output capacity. Output and compaction use the same accounting.
fn response_reserve(window: u64, threshold_ratio: f64, declared: u32) -> u32 {
    let reserve = (window as f64 * (1.0 - threshold_ratio)).round() as u64;
    declared.min(u32::try_from(reserve.max(1)).unwrap_or(u32::MAX))
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

/// send_with_retry 的结果：响应、退避等待期间被取消、最终 provider 失败，
/// 或 durable 记录失败（typed store 路径：请求被阻止或观测未落盘）。
pub(crate) enum SendOutcome {
    Response(Box<ModelTurnResponse>),
    Aborted,
    Failed(ProviderError),
    Store(SessionError),
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
    /// 当前 attempt 内暂存的 store 失败（provider 失败后追加可见消息时的落盘失败）。
    store_failure: Option<SessionError>,
    /// 预分配结果 id 已由可见的部分 assistant 文本闭合。
    result_committed: bool,
}

impl<'a> AttemptLedger<'a> {
    pub(crate) fn new(writer: &'a SessionWriter, accounting: &'a mut RequestAccounting) -> Self {
        Self {
            writer,
            accounting,
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
        self.accounting.attempts += 1;
        self.store_failure = None;
        self.result_committed = false;
        self.result_entry_id = lock_writer(self.writer).new_entry_id();
    }

    /// 将已发布给客户端的可见流式文本落在本 attempt 预分配的 assistant
    /// 结果 id 上。终态由 operation outcome 独立表达，因此该消息保持普通
    /// assistant 形状，不引入第二套 partial 状态。
    fn persist_visible_assistant(&mut self, text: &str, reasoning: &str) {
        if (text.is_empty() && reasoning.is_empty()) || self.result_committed {
            return;
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
        match lock_writer(self.writer).append_message_with_id(
            &self.result_entry_id,
            AgentMessage::Assistant {
                content,
                stop_reason: None,
                provider_reasoning_replay: None,
            },
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
/// 调用方处理；退避等待被取消时返回 SendOutcome::Aborted。
pub(crate) fn send_with_retry<'a>(
    mut attempt: impl FnMut(
        &mut AttemptLedger<'a>,
        &mut AgentEvents,
    ) -> std::result::Result<ModelTurnResponse, ProviderError>,
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

/// 单轮 provider 请求的静态规格：除轮次序号外，一次 run 内恒定不变。
/// 输出预算不在这里——它随上下文变化，在装配时按模型声明值与剩余窗口现算
/// （见 Agent::output_budget_tokens），冻结它只会造出第二个事实源。
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
/// 对不支持 developer 角色的端点由 wire 层按 supports_developer_role
/// 转为 system。
pub(crate) fn instruction_message(instruction: &str) -> Option<ModelMessage> {
    if instruction.is_empty() {
        return None;
    }
    Some(ModelMessage::text(ModelRole::Developer, instruction))
}

impl Agent {
    pub(super) fn load_manual_skill(&mut self, input: &str) -> Result<()> {
        let Some(skill) = self.registry.skills.manual(input) else {
            return Ok(());
        };
        let text = skill.load().map_err(AgentError::Loop)?;
        lock_writer(&self.session).append_record(LedgerRecord::SkillInstructions { text })?;
        self.track_last_entry();
        Ok(())
    }

    /// 每个模型步及压缩后从原文件核对指令。来源内容相同且仍可见时不重复注入。
    pub(super) fn refresh_instructions(&mut self) -> Result<()> {
        let Some(home) = &self.config.instruction_home else {
            return Ok(());
        };
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded = singularity_core::load_agent_instructions(&cwd, home)
            .map_err(|error| AgentError::Loop(error.to_string()))?;
        let instructions = loaded
            .as_ref()
            .map(singularity_core::ProjectInstructions::content)
            .unwrap_or("");
        let catalog = self.registry.skills.prompt();
        let current = [instructions, catalog.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let text = format!(
            "<system-reminder>\nThis is the current complete snapshot of global and project file instructions, replacing earlier file-instruction snapshots (including facts quoted in checkpoints). Missing files no longer apply. More specific project instructions take precedence. These do not override system, developer, or direct user instructions.\n{current}\n</system-reminder>"
        );
        let visible = self.context.entries().iter().rev().find_map(|entry| {
            if let SessionEntry::Record {
                record: LedgerRecord::Instructions { text },
                ..
            } = entry
            {
                Some(text)
            } else {
                None
            }
        });
        let previously_loaded = lock_writer(&self.session).entries().iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::Record {
                    record: LedgerRecord::Instructions { .. },
                    ..
                }
            )
        });
        if visible == Some(&text) || (current.is_empty() && visible.is_none() && !previously_loaded)
        {
            return Ok(());
        }
        lock_writer(&self.session).append_record(LedgerRecord::Instructions { text })?;
        self.track_last_entry();
        Ok(())
    }

    /// 当前请求包络与历史共享的压力口径。
    pub(super) fn request_overhead_tokens(&self) -> u64 {
        let tools = self.registry.provider_schemas();
        let system = if self.config.system_prompt.is_empty() {
            0
        } else {
            crate::session::context::estimate_tokens_of(&self.config.system_prompt) + 4
        };
        system
            + if tools.is_empty() {
                0
            } else {
                crate::session::context::estimate_tokens_of(
                    &serde_json::to_string(&tools).unwrap_or_default(),
                ) + 4
            }
    }

    pub(super) fn context_pressure_tokens(&self) -> u64 {
        self.context.request_tokens(self.request_overhead_tokens())
    }

    /// 将剪枝作为引用原消息的追加记录落盘，随后从同一账本重建模型视图。
    pub(super) fn prune_tool_results(
        &mut self,
        keep_recent_tokens: u64,
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let cut = crate::compaction::CompactionEngine::find_cut_point(
            self.context.entries(),
            keep_recent_tokens,
        );
        let replacements: Vec<_> = self.context.entries()[..cut]
            .iter()
            .filter_map(|entry| {
                if let SessionEntry::Message {
                    id,
                    message: crate::message::AgentMessage::ToolResult { content, .. },
                    ..
                } = entry
                {
                    crate::compaction::prune_tool_content(content).map(|content| {
                        LedgerRecord::ToolResultPruned {
                            entry_id: id.clone(),
                            content,
                        }
                    })
                } else {
                    None
                }
            })
            .collect();
        let changed = !replacements.is_empty();
        for record in replacements {
            if cancellation.is_cancelled() {
                return Err(AgentError::Compaction(
                    crate::compaction::CompactionError::Aborted,
                ));
            }
            lock_writer(&self.session).append_record(record)?;
        }
        if changed {
            self.context.rebuild(&lock_writer(&self.session))?;
        }
        Ok(changed)
    }

    /// 历史上下文压缩复用正常请求模板，只有历史前缀由摘要引擎选择。
    pub(super) fn compact_with_record(
        &mut self,
        tokens_before: u64,
        keep_recent_tokens: u64,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> std::result::Result<CompactionOutcome, crate::compaction::CompactionError> {
        let request = self.build_request(&TurnRequestSpec {
            tools: self.registry.provider_schemas(),
            turn: 0,
        });
        let mut ledger = AttemptLedger::new(&self.session, &mut self.accounting);
        self.compaction.compact(
            &mut ledger,
            crate::compaction::CompactionInput {
                entries: self.context.entries(),
                keep_recent_tokens,
                tokens_before,
                request,
            },
            events,
            cancellation,
        )
    }

    /// 请求前刷新文件指令，再依次执行工具剪枝和至多两次摘要。
    /// 摘要失败时保留已提交的缩减，存储失败与取消直接结束当前请求准备。
    pub(super) fn prepare_request(
        &mut self,
        spec: &TurnRequestSpec,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        self.refresh_instructions()?;
        let window = self.model.context_window();
        if !self.needs_context_reduction() {
            return Ok(self.build_request(spec));
        }
        self.prune_tool_results(self.config.compaction.retain_tokens(window), cancellation)?;
        for _ in 0..2 {
            let tokens = self.context_pressure_tokens();
            if !self.needs_context_reduction() {
                break;
            }
            let retain = self.config.compaction.retain_tokens(window);
            match self.compact_with_record(tokens, retain, events, cancellation) {
                Ok(CompactionOutcome::Compacted { .. }) => {
                    self.context.rebuild(&lock_writer(&self.session))?;
                    self.refresh_instructions()?;
                }
                Ok(_) => break,
                Err(CompactionError::Session(error)) => return Err(AgentError::Session(error)),
                Err(CompactionError::Aborted) => {
                    return Err(AgentError::Compaction(CompactionError::Aborted));
                }
                Err(error) => {
                    emit_diagnostic(
                        events,
                        AgentDiagnostic::warning(
                            diagnostic_code::COMPACTION_SKIPPED,
                            format!("automatic context compaction skipped: {error}"),
                        ),
                    );
                    break;
                }
            }
        }
        self.ensure_response_room()?;
        Ok(self.build_request(spec))
    }

    fn needs_context_reduction(&self) -> bool {
        self.config
            .compaction
            .should_compact(self.context_pressure_tokens(), self.model.context_window())
            || self.output_budget_tokens() < self.response_reserve()
    }

    fn response_reserve(&self) -> u32 {
        response_reserve(
            self.model.context_window(),
            self.config.compaction.threshold_ratio,
            self.model.capabilities.max_output_tokens,
        )
    }

    pub(super) fn ensure_response_room(&self) -> Result<()> {
        let available = self.output_budget_tokens();
        let reserve = self.response_reserve();
        if available < reserve {
            return Err(AgentError::Loop(format!(
                "insufficient context space after compaction: {available} output tokens available, \
                 {reserve} reserved; shorten the input or use a model with a larger context window"
            )));
        }
        Ok(())
    }

    /// 采样层：对一次纯发送做 agent 层重试包装（send_with_retry）。
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
        let mut ledger = AttemptLedger::new(&self.session, &mut self.accounting);
        let retry = self.model.retry;
        let outcome = send_with_retry(
            |ledger, events| {
                stream_completion_once(
                    provider,
                    request,
                    ledger,
                    events,
                    cancellation,
                    model_turn_ordinal,
                    singularity_protocol::RequestPurpose::Generation,
                )
            },
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

    /// 本次请求可声明的输出上限：模型输出上限与
    /// 「窗口 − 当前上下文 − 安全垫」的较小者。向端点声明一个窗口放不下的输出
    /// 预算会让兼容端点直接以 400 拒绝整次请求，因此收紧发生在装配处——它是
    /// 上下文变化后唯一真正决定 wire 形状的地方。
    fn output_budget_tokens(&self) -> u32 {
        output_token_budget(
            self.model.context_window(),
            self.context_pressure_tokens(),
            self.model.capabilities.max_output_tokens,
        )
    }

    /// 按 TurnRequestSpec 组装单轮 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    pub(super) fn build_request(&self, spec: &TurnRequestSpec) -> ModelTurnRequest {
        let mut request = ModelTurnRequest::new(
            format!("turn_{}_{}", Uuid::new_v4().simple(), spec.turn),
            self.assemble_messages(),
        );
        request.tools = spec.tools.clone();
        request.model_preferences = ModelPreferences {
            model_name: Some(self.model.model.clone()),
            max_output_tokens: Some(self.output_budget_tokens()),
        };
        request
    }

    /// 正常请求与压缩均从同一历史投影取得消息及其私有续接。
    /// 协议兼容性由 Provider 处理，Agent 不筛选或重建续接数据。
    pub(super) fn assemble_messages(&self) -> Vec<ModelMessage> {
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
        messages
    }
}

/// 流式调用（唯一模型调用形态）。纯发送：不感知压缩、重试与 ContextOverflow。
/// provider 的观测直接作为实时事件发射。
pub(crate) fn stream_completion_once(
    provider: &Arc<dyn Provider + Send + Sync>,
    request: &ModelTurnRequest,
    ledger: &mut AttemptLedger<'_>,
    events: &mut AgentEvents,
    cancellation: &CancellationToken,
    model_turn_ordinal: u32,
    purpose: singularity_protocol::RequestPurpose,
) -> std::result::Result<ModelTurnResponse, ProviderError> {
    let mut request = request.clone();
    request.request_id = ledger.result_entry_id().to_string();
    let request = &request;
    // provider 回调与 on_attempt 共享同一个事件出口；用本地 RefCell 承接
    // 两个异签名回调的可变借用（单线程 turn 内串行使用）。事件投影尽力
    // 而为，provider 结果不因投影失败丢弃。
    let events_cell = std::cell::RefCell::new(events);
    let events_ref = &events_cell;
    let mut visible_text = String::new();
    let mut visible_reasoning = String::new();
    let message_id = ledger.result_entry_id().to_string();
    let mut observed = false;
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
        let mut observed_attempt = |event: ProviderAttemptEvent| {
            let event = event.with_attempt(ledger.accounting.attempts);
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
            let started = matches!(&event, ProviderAttemptEvent::Started(_));
            let observation = singularity_protocol::RequestObservation {
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
                request: started.then(|| serde_json::json!(request)),
            };
            if let Err(error) =
                lock_writer(ledger.writer).append_record(LedgerRecord::ModelRequest {
                    observation,
                    context: None,
                })
            {
                if matches!(error, SessionError::Io(_)) {
                    ledger.store_failure = Some(error);
                    return;
                }
                emit_diagnostic(
                    &mut events_ref.borrow_mut(),
                    AgentDiagnostic::warning(
                        "request_observation_unavailable",
                        format!("request details could not be saved: {error}"),
                    ),
                );
            }
            emit(&mut events_ref.borrow_mut(), AgentEvent::ProviderAttempt {
                request_id: request.request_id.clone(),
                request_head: started.then(|| serde_json::json!({
                    "request_id": request.request_id,
                    "messages": request.messages.iter().filter(|m| matches!(m.role, ModelRole::System | ModelRole::Developer)).collect::<Vec<_>>(),
                    "tools": request.tools, "model_preferences": request.model_preferences,
                })),
                purpose, model_turn_ordinal, event,
            });
        };
        provider.complete_stream(request, cancellation, &mut on_stream, &mut observed_attempt)
    };
    if !observed {
        ledger
            .accounting
            .observe(result.as_ref().ok().map(|response| &response.usage));
    }
    if result.is_err() && purpose == singularity_protocol::RequestPurpose::Generation {
        ledger.persist_visible_assistant(&visible_text, &visible_reasoning);
        emit(
            &mut events_cell.borrow_mut(),
            AgentEvent::MessageFinished {
                message_id,
                failed: true,
            },
        );
    }
    result
}

#[cfg(test)]
mod tests;
