//! Agent 请求装配：指令、上下文缩减与模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成与压缩共用该入口。

use super::{Agent, AgentError, Result};
use crate::compaction::{CompactionOutcome, PreparedCompaction, output_token_budget};
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::request_execution::execute_request;
use crate::session::{LedgerRecord, SessionEntry, lock_writer};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest,
};

/// 一次请求准备里自动压缩的至多轮数：每轮重新判断上下文压力，
/// NotNeeded 或摘要失败即停，避免在同一请求上反复摘要。
const MAX_AUTO_COMPACTIONS_PER_REQUEST: usize = 2;

pub(super) fn emit_compaction_skipped(on_event: &mut dyn FnMut(AgentEvent), error: &AgentError) {
    on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
        diagnostic_code::COMPACTION_SKIPPED,
        format!("automatic context compaction skipped: {error}"),
    )));
}

/// 为响应预留正常阈值的剩余窗口，上限为模型的输出容量。
/// 输出与压缩使用同一套计量。
fn response_reserve(window: u64, threshold_ratio: f64, declared: u32) -> u32 {
    let reserve = (window as f64 * (1.0 - threshold_ratio)).round() as u64;
    declared.min(u32::try_from(reserve.max(1)).unwrap_or(u32::MAX))
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

/// 系统提示词与冻结工具定义是本轮请求的静态包络；只在这里计算一次。
// 工具 schema 是本 crate 构造的纯 serde 结构，序列化失败表示内部类型出了问题；
// 直接 fail-stop，不静默退化成空串。
#[allow(clippy::expect_used)]
pub(super) fn static_request_overhead_tokens(
    system_prompt: &str,
    tools: &[ModelToolSchema],
) -> u64 {
    let system = if system_prompt.is_empty() {
        0
    } else {
        crate::session::context::estimate_tokens_of(system_prompt) + 4
    };
    let tools = if tools.is_empty() {
        0
    } else {
        let schema = serde_json::to_string(tools).expect("tool schemas are serializable");
        crate::session::context::estimate_tokens_of(&schema) + 4
    };
    system + tools
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
    pub(super) fn refresh_instructions(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let Some(home) = &self.config.instruction_home else {
            return Ok(());
        };
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded =
            singularity_core::load_agent_instructions(&cwd, home).map_err(AgentError::Loop)?;
        self.apply_instructions(loaded, on_event)
    }

    pub(super) fn apply_instructions(
        &mut self,
        loaded: Option<singularity_core::ProjectInstructions>,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
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
        let writer = lock_writer(&self.session);
        let visible = self.context.visible_instructions(&writer);
        let previously_loaded = writer.entries().iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::Record {
                    record: LedgerRecord::Instructions { .. },
                    ..
                }
            )
        });
        // 比较在会话读锁内完成；只有确实需要追加时才释放锁并写盘。
        if visible == Some(text.as_str())
            || (current.is_empty() && visible.is_none() && !previously_loaded)
        {
            return Ok(());
        }
        drop(writer);
        self.append_record(LedgerRecord::Instructions { text })?;
        if loaded
            .as_ref()
            .is_some_and(singularity_core::ProjectInstructions::truncated)
        {
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                singularity_protocol::diagnostic_code::PROJECT_INSTRUCTIONS_TRUNCATED,
                "project instructions were truncated because they exceeded the size budget",
            )));
        }
        Ok(())
    }

    pub(super) fn context_pressure_tokens(&self) -> u64 {
        self.context.request_tokens(self.request_overhead_tokens)
    }

    /// 将剪枝作为引用原消息的追加记录落盘，随后从同一账本重建模型视图。
    pub(super) fn prune_tool_results(
        &mut self,
        keep_recent_tokens: u64,
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let writer = lock_writer(&self.session);
        let replacements = self
            .context
            .pruned_tool_results(&writer, keep_recent_tokens);
        drop(writer);
        let changed = !replacements.is_empty();
        for record in replacements {
            if cancellation.is_cancelled() {
                return Err(AgentError::Aborted);
            }
            lock_writer(&self.session).append_record(record)?;
        }
        if changed {
            self.context.rebuild(&lock_writer(&self.session))?;
        }
        Ok(changed)
    }

    /// 摘要先选历史前缀与输出上限，再和静态请求包络一起组装，不构造被丢弃的完整请求。
    pub(super) fn compact_with_record(
        &mut self,
        keep_recent_tokens: u64,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let instruction = instruction_message(&self.config.system_prompt);
        let Some(prefix) = self
            .context
            .compaction_prefix(&lock_writer(&self.session), keep_recent_tokens)
        else {
            return Ok(CompactionOutcome::NotNeeded);
        };
        let mut summary = PreparedCompaction::new(prefix, instruction.as_ref(), &self.model)?;
        let (response, id) = match execute_request(
            &self.provider,
            &self.session,
            &mut self.accounting,
            &mut summary.request,
            on_event,
            cancellation,
            0,
            singularity_protocol::RequestPurpose::Compaction,
        ) {
            Ok(result) => result,
            Err(AgentError::Provider(_)) if cancellation.is_cancelled() => {
                return Err(AgentError::Aborted);
            }
            Err(error) => return Err(error),
        };
        let entry = summary.into_entry(response)?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        lock_writer(&self.session).append_compaction_with_id(&id, entry)?;
        self.refresh_compacted_context(on_event)?;
        Ok(CompactionOutcome::Reduced)
    }

    pub(super) fn refresh_compacted_context(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        self.context.rebuild(&lock_writer(&self.session))?;
        self.refresh_instructions(on_event)
    }

    /// 准备一次请求：先按需执行工具剪枝和至多两次摘要，再装配请求。
    /// 文件指令不在这里读取（只在 turn 开始与压缩完成后刷新一次，见
    /// `apply_instructions`/`refresh_compacted_context`）。
    /// 摘要失败时保留已提交的缩减，存储失败与取消直接结束当前请求准备。
    pub(super) fn prepare_request(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let window = self.model.context_window();
        if !self.needs_context_reduction() {
            return Ok(self.build_request());
        }
        self.prune_tool_results(self.config.compaction.retain_tokens(window), cancellation)?;
        for _ in 0..MAX_AUTO_COMPACTIONS_PER_REQUEST {
            if !self.needs_context_reduction() {
                break;
            }
            let retain = self.config.compaction.retain_tokens(window);
            match self.compact_with_record(retain, on_event, cancellation) {
                Ok(CompactionOutcome::Reduced) => {}
                Ok(CompactionOutcome::NotNeeded) => break,
                Err(error @ (AgentError::Provider(_) | AgentError::InvalidSummary(_))) => {
                    emit_compaction_skipped(on_event, &error);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        self.ensure_response_room()?;
        Ok(self.build_request())
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
            self.model.max_output_tokens,
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

    /// 本次请求可声明的输出上限：模型输出上限与
    /// 「窗口 − 当前上下文 − 安全垫」的较小者。向端点声明一个窗口放不下的输出
    /// 预算会让兼容端点直接以 400 拒绝整次请求，因此收紧发生在装配处——它是
    /// 上下文变化后唯一真正决定 wire 形状的地方。
    fn output_budget_tokens(&self) -> u32 {
        output_token_budget(
            self.model.context_window(),
            self.context_pressure_tokens(),
            self.model.max_output_tokens,
        )
    }

    /// 使用本轮冻结的工具定义组装 provider 请求：首条指令消息恒以 Developer
    /// 角色构造（wire 层按 supports_developer_role 降级）+ 会话历史（compaction 感知）。
    pub(super) fn build_request(&self) -> ModelTurnRequest {
        // 真正的请求 ID 在发送 attempt 时取自预分配的 ledger 结果 ID。
        let mut request = ModelTurnRequest::new(String::new(), self.assemble_messages());
        request.tools = self.tools.clone();
        request.model_preferences = ModelPreferences {
            max_output_tokens: Some(self.output_budget_tokens()),
        };
        request
    }

    /// 正常请求与压缩均从同一历史投影取得消息及其私有续接。
    /// 协议兼容性由 Provider 处理，Agent 不筛选或重建续接数据。
    pub(super) fn assemble_messages(&self) -> Vec<ModelMessage> {
        let writer = lock_writer(&self.session);
        let mut messages = self.context.messages(&writer);
        if let Some(instruction) = instruction_message(&self.config.system_prompt) {
            messages.insert(0, instruction);
        }
        messages
    }
}

#[cfg(test)]
mod tests;
