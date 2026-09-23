//! Agent 请求装配：把指令、历史上下文与压缩后的材料组装成模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成请求与摘要请求共用那个入口。

use super::{Agent, AgentError, Result};
use crate::compaction::{CompactionOutcome, PreparedCompaction};
use crate::events::{AgentDiagnostic, AgentEvent, diagnostic_code};
use crate::request_execution::execute_request;
use crate::session::{LedgerRecord, lock_writer};
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolSchema, ModelTurnRequest,
};
use tokio_util::sync::CancellationToken;

/// 一次请求准备里最多自动压缩几轮：每轮重新判断上下文压力，NotNeeded 或摘要失败就停下。
const MAX_AUTO_COMPACTIONS_PER_REQUEST: usize = 2;

pub(super) fn emit_compaction_skipped(on_event: &mut dyn FnMut(AgentEvent), error: &AgentError) {
    on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
        diagnostic_code::COMPACTION_SKIPPED,
        format!("automatic context compaction skipped: {error}"),
    )));
}

/// 这次摘要失败能不能跳过、继续发本次请求：只有「摘要内容不可用」和「可重试的暂时失败
/// 已用尽重试预算」可以跳过；永久 provider 失败、取消与存储故障必须向上传播。
pub(super) fn compaction_may_be_skipped(error: &AgentError) -> bool {
    match error {
        AgentError::InvalidSummary(_) => true,
        AgentError::Provider(provider) => provider.is_retryable(),
        _ => false,
    }
}

/// Harness 指令使用 Developer 角色；不支持 developer 的端点由 Provider 降级。
fn developer_message(instruction: &str) -> Option<ModelMessage> {
    if instruction.is_empty() {
        return None;
    }
    Some(ModelMessage::text(ModelRole::Developer, instruction))
}

fn file_instruction_message(instructions: &str) -> Option<ModelMessage> {
    if instructions.is_empty() {
        return None;
    }
    // 文件正文不能提前关闭来源边界。
    let instructions = instructions.replace("</file-instructions>", "<\\/file-instructions>");
    Some(ModelMessage::text(
        ModelRole::User,
        format!(
            "<file-instructions>\nThese global and project file instructions apply to the current workspace. More specific files take precedence. Direct user instructions take precedence over these file instructions.\n{instructions}\n</file-instructions>"
        ),
    ))
}

/// Harness、当前 Skill 目录提示与冻结工具定义的开销在 Agent 初始化及目录刷新时计算。
// 工具 schema 序列化失败说明内部类型出了问题，直接 fail-stop，不静默退化成空串。
#[allow(clippy::expect_used)]
pub(super) fn static_request_overhead_tokens(
    developer_instructions: &str,
    skill_catalog: &str,
    tools: &[ModelToolSchema],
) -> u64 {
    let instruction_tokens = [developer_instructions, skill_catalog]
        .into_iter()
        .filter(|text| !text.is_empty())
        .map(|text| crate::session::context::estimate_tokens_of(text) + 4)
        .sum::<u64>();
    let tools = if tools.is_empty() {
        0
    } else {
        let schema = serde_json::to_string(tools).expect("tool schemas are serializable");
        crate::session::context::estimate_tokens_of(&schema) + 4
    };
    instruction_tokens + tools
}

/// 用于弥补启发式估算与 provider 实际 tokenization 之间的差异。
const REQUEST_OUTPUT_SAFETY_TOKENS: u64 = 4_096;

/// 生成请求能声明的输出预算：窗口减去压力与安全余量后的剩余；结果为 0 表示这个请求发不
/// 出去。压缩路径不用它——摘要的输出上限只受模型自身输出上限约束。
pub(super) fn output_token_budget(window: u64, pressure: u64, declared: u32) -> u32 {
    let room = window
        .saturating_sub(pressure)
        .saturating_sub(REQUEST_OUTPUT_SAFETY_TOKENS.min(window / 20));
    declared.min(u32::try_from(room).unwrap_or(u32::MAX))
}

impl Agent {
    /// 读取手动选择的 skill，并把它的指令追加进持久账本：这一步同时是本轮指令的提交动作。
    pub(super) fn load_and_record_manual_skill(&mut self, input: &str) -> Result<()> {
        let Some(skill) = self.registry.skills.manual(input) else {
            return Ok(());
        };
        let text = skill.load().map_err(AgentError::SkillLoad)?;
        self.append_record(LedgerRecord::SkillInstructions { text })?;
        Ok(())
    }

    /// 压缩后重新读取文件指令与 Skill 目录，直接替换本轮请求使用的内容。
    pub(super) fn refresh_instructions(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let Some(home) = self.config.instruction_home.as_ref().cloned() else {
            return Ok(());
        };
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded = singularity_core::load_agent_instructions(&cwd, &home)
            .map_err(AgentError::Instructions)?;
        self.registry.skills = singularity_core::skills::SkillCatalog::discover(&cwd, &home);
        self.request_static_tokens = static_request_overhead_tokens(
            &self.config.developer_instructions,
            &self.registry.skills.prompt(),
            &self.tools,
        );
        self.apply_instructions(loaded, on_event);
        Ok(())
    }

    pub(super) fn apply_instructions(
        &mut self,
        loaded: Option<singularity_core::ProjectInstructions>,
        on_event: &mut dyn FnMut(AgentEvent),
    ) {
        self.file_instructions = loaded
            .as_ref()
            .map(singularity_core::ProjectInstructions::content)
            .and_then(file_instruction_message);
        self.context.reset_usage_correction();
        if loaded
            .as_ref()
            .is_some_and(singularity_core::ProjectInstructions::truncated)
        {
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                singularity_protocol::diagnostic_code::PROJECT_INSTRUCTIONS_TRUNCATED,
                "project instructions were truncated because they exceeded the size budget",
            )));
        }
    }

    pub(super) fn request_overhead_tokens(&self) -> u64 {
        self.request_static_tokens.saturating_add(
            self.file_instructions
                .as_ref()
                .map(|message| crate::session::context::estimate_tokens_of(&message.content) + 8)
                .unwrap_or(0),
        )
    }

    pub(super) fn context_pressure_tokens(&self) -> u64 {
        self.context.request_tokens(self.request_overhead_tokens())
    }

    /// 把剪枝作为「引用原消息」的追加记录落盘，随后从同一份账本重建模型视图。
    /// 剪枝覆盖整个活动历史：超长工具结果不分新旧。
    pub(super) fn prune_tool_results(&mut self, cancellation: &CancellationToken) -> Result<bool> {
        let writer = lock_writer(&self.session);
        let replacements = self.context.pruned_tool_results(&writer);
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

    /// 摘要先选出历史前缀，再和本轮冻结的系统提示词、工具定义一起组装，
    /// 不构造那份会被丢弃的完整请求。
    pub(super) fn compact_with_record(
        &mut self,
        keep_recent_tokens: u64,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let instructions = self.instruction_prefix();
        // 历史里没有可替换的前缀（内容太少）：本次无需摘要。
        let Some(prefix) = self
            .context
            .compaction_prefix(&lock_writer(&self.session), keep_recent_tokens)
        else {
            return Ok(CompactionOutcome::NotNeeded);
        };
        let mut summary = PreparedCompaction::new(prefix, &instructions, &self.tools, &self.model);
        // 请求层已经做过唯一一次 ProviderCallError→AgentError 分类；压缩只传播结果，
        // 不再按取消令牌改写真实失败原因（停止是否被接受由操作层的终态边界裁决）。
        let (response, id) = execute_request(
            self.provider.as_ref(),
            &self.session,
            &mut self.accounting,
            &mut summary.request,
            on_event,
            cancellation,
            0,
            singularity_protocol::RequestPurpose::Compaction,
        )?;
        let entry = summary.into_entry(response)?;
        // 摘要已生成但落盘前被取消：这次摘要不写入会话。
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

    /// 准备一次请求：先按需做工具剪枝和至多两次摘要，再组装请求。文件指令不在这里读取
    /// （只在 turn 开始和压缩完成后刷新一次）。摘要失败时保留已经提交的缩减；存储失败
    /// 与取消直接结束本次请求准备。
    pub(super) fn prepare_request(
        &mut self,
        on_event: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnRequest> {
        let window = self.model.context_window();
        if !self.needs_context_reduction() {
            return Ok(self.build_request());
        }
        self.prune_tool_results(cancellation)?;
        for _ in 0..MAX_AUTO_COMPACTIONS_PER_REQUEST {
            if !self.needs_context_reduction() {
                break;
            }
            let retain = self.config.compaction.retain_tokens(window);
            match self.compact_with_record(retain, on_event, cancellation) {
                // 压缩生效：回到循环开头重新判断是否还需要。
                Ok(CompactionOutcome::Reduced) => {}
                Ok(CompactionOutcome::NotNeeded) => break,
                // 可跳过的摘要失败：保留已缩减的历史，继续本次请求。
                Err(error) if compaction_may_be_skipped(&error) => {
                    emit_compaction_skipped(on_event, &error);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(self.build_request())
    }

    fn needs_context_reduction(&self) -> bool {
        self.config
            .compaction
            .should_compact(self.context_pressure_tokens(), self.model.context_window())
    }

    /// 本次请求能声明的输出上限：取「模型输出上限」与「窗口 − 当前上下文 − 安全垫」的较小者。
    /// 向端点声明一个窗口放不下的输出预算会让兼容端点以 400 拒绝整次请求。
    fn output_budget_tokens(&self) -> u32 {
        output_token_budget(
            self.model.context_window(),
            self.context_pressure_tokens(),
            self.model.max_output_tokens,
        )
    }

    /// 用本轮冻结的工具定义组装 Provider 无关请求。
    pub(super) fn build_request(&self) -> ModelTurnRequest {
        // 真正的请求 ID 在发送 attempt 时取自预分配的 ledger 结果 ID。
        let mut request = ModelTurnRequest::new(String::new(), self.assemble_messages());
        request.tools = self.tools.clone();
        request.model_preferences = ModelPreferences {
            max_output_tokens: Some(self.output_budget_tokens()),
        };
        request
    }

    /// 开头是 Harness / Skill 目录的 Developer 消息与当前项目指令快照；其后是可压缩
    /// 对话历史。手动 Skill 指令在触发输入之前，直接用户输入仍保留 User 角色。
    fn instruction_prefix(&self) -> Vec<ModelMessage> {
        let mut messages = Vec::new();
        if let Some(instruction) = developer_message(&self.config.developer_instructions) {
            messages.push(instruction);
        }
        if let Some(catalog) = developer_message(&self.registry.skills.prompt()) {
            messages.push(catalog);
        }
        if let Some(files) = &self.file_instructions {
            messages.push(files.clone());
        }
        messages
    }

    /// 普通请求与摘要请求共用历史投影及其私有续接材料；协议兼容性由 Provider 处理。
    pub(super) fn assemble_messages(&self) -> Vec<ModelMessage> {
        let mut messages = self.instruction_prefix();
        let writer = lock_writer(&self.session);
        messages.extend(self.context.messages(&writer));
        messages
    }
}
