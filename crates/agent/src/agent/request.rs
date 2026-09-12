//! Agent request preparation: instructions, context reduction and model input.
//! Generation and compaction share execution through crate::request_execution.

use super::{Agent, AgentError, Result};
use crate::compaction::{CompactionError, CompactionOutcome, PreparedCompaction};
use crate::events::{AgentDiagnostic, AgentEvents, diagnostic_code, emit_diagnostic};
use crate::request_execution::{
    AttemptLedger, RequestExecutionError, output_token_budget, send_with_retry,
    stream_completion_once,
};
use crate::session::context::entry_to_llm_message;
use crate::session::{LedgerRecord, SessionEntry, lock_writer};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest, ModelTurnResponse,
};

pub(super) fn emit_compaction_skipped(events: &mut AgentEvents, error: &CompactionError) {
    emit_diagnostic(
        events,
        AgentDiagnostic::warning(
            diagnostic_code::COMPACTION_SKIPPED,
            format!("automatic context compaction skipped: {error}"),
        ),
    );
}

/// Reserve the normal threshold's remaining window for a response, capped by
/// the model's output capacity. Output and compaction use the same accounting.
fn response_reserve(window: u64, threshold_ratio: f64, declared: u32) -> u32 {
    let reserve = (window as f64 * (1.0 - threshold_ratio)).round() as u64;
    declared.min(u32::try_from(reserve.max(1)).unwrap_or(u32::MAX))
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
        self.append_record(LedgerRecord::SkillInstructions { text })?;
        Ok(())
    }

    /// 每轮开始及压缩后核对指令；来源内容相同且仍可见时不重复注入。
    pub(super) fn refresh_instructions(&mut self, events: &mut AgentEvents) -> Result<()> {
        let Some(home) = &self.config.instruction_home else {
            return Ok(());
        };
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded =
            singularity_core::load_agent_instructions(&cwd, home).map_err(AgentError::Loop)?;
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
        self.append_record(LedgerRecord::Instructions { text })?;
        if loaded
            .as_ref()
            .is_some_and(singularity_core::ProjectInstructions::truncated)
        {
            emit_diagnostic(
                events,
                AgentDiagnostic::warning(
                    singularity_protocol::diagnostic_code::PROJECT_INSTRUCTIONS_TRUNCATED,
                    "project instructions were truncated because they exceeded the size budget",
                ),
            );
        }
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
        let cut = crate::compaction::find_cut_point(self.context.entries(), keep_recent_tokens);
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
        if cancellation.is_cancelled() {
            return Err(CompactionError::Aborted);
        }
        let Some(mut summary) = PreparedCompaction::new(
            self.context.entries(),
            keep_recent_tokens,
            tokens_before,
            self.build_request(&self.registry.provider_schemas()),
            &self.model,
        )?
        else {
            return Ok(CompactionOutcome::NotNeeded);
        };
        let (response, id) = match self.execute_request(
            &mut summary.request,
            events,
            cancellation,
            0,
            singularity_protocol::RequestPurpose::Compaction,
        ) {
            Ok(result) => result,
            Err(RequestExecutionError::Aborted) => return Err(CompactionError::Aborted),
            Err(RequestExecutionError::Provider(_)) if cancellation.is_cancelled() => {
                return Err(CompactionError::Aborted);
            }
            Err(RequestExecutionError::Provider(error)) => {
                return Err(CompactionError::Provider(error));
            }
            Err(RequestExecutionError::Session(error)) => {
                return Err(CompactionError::Session(error));
            }
        };
        let entry = summary.into_entry(*response)?;
        if cancellation.is_cancelled() {
            return Err(CompactionError::Aborted);
        }
        lock_writer(&self.session).append_compaction_with_id(&id, entry)?;
        Ok(CompactionOutcome::Reduced)
    }

    /// 请求前刷新文件指令，再依次执行工具剪枝和至多两次摘要。
    /// 摘要失败时保留已提交的缩减，存储失败与取消直接结束当前请求准备。
    pub(super) fn prepare_request(
        &mut self,
        tools: &[ModelToolSchema],
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let window = self.model.context_window();
        if !self.needs_context_reduction() {
            return Ok(self.build_request(tools));
        }
        self.prune_tool_results(self.config.compaction.retain_tokens(window), cancellation)?;
        for _ in 0..2 {
            let tokens = self.context_pressure_tokens();
            if !self.needs_context_reduction() {
                break;
            }
            let retain = self.config.compaction.retain_tokens(window);
            match self.compact_with_record(tokens, retain, events, cancellation) {
                Ok(CompactionOutcome::Reduced) => {
                    self.context.rebuild(&lock_writer(&self.session))?;
                    self.refresh_instructions(events)?;
                }
                Ok(_) => break,
                Err(CompactionError::Session(error)) => return Err(AgentError::Session(error)),
                Err(CompactionError::Aborted) => {
                    return Err(AgentError::Compaction(CompactionError::Aborted));
                }
                Err(error) => {
                    emit_compaction_skipped(events, &error);
                    break;
                }
            }
        }
        self.ensure_response_room()?;
        Ok(self.build_request(tools))
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

    /// 普通回复和摘要共用发送、重试、用量与结果身份的完整请求边界。
    pub(super) fn execute_request(
        &mut self,
        request: &mut ModelTurnRequest,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
        model_turn_ordinal: u32,
        purpose: singularity_protocol::RequestPurpose,
    ) -> std::result::Result<(Box<ModelTurnResponse>, String), RequestExecutionError> {
        let provider = &self.provider;
        let mut ledger = AttemptLedger::new(&self.session, &mut self.accounting);
        let retry = self.model.retry;
        let response = send_with_retry(
            |ledger, events| {
                stream_completion_once(
                    provider,
                    request,
                    ledger,
                    events,
                    cancellation,
                    model_turn_ordinal,
                    purpose,
                )
            },
            &mut ledger,
            retry,
            events,
            cancellation,
        )?;
        Ok((response, ledger.result_entry_id().to_string()))
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

    /// 使用本轮冻结的工具定义组装 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    pub(super) fn build_request(&self, tools: &[ModelToolSchema]) -> ModelTurnRequest {
        // 真正的请求 ID 在发送 attempt 时取自预分配的 ledger 结果 ID。
        let mut request = ModelTurnRequest::new(String::new(), self.assemble_messages());
        request.tools = tools.to_vec();
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
                .filter_map(entry_to_llm_message),
        );
        messages
    }
}

#[cfg(test)]
mod tests;
