//! Agent 请求装配：把指令、历史上下文与压缩后的材料组装成模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成请求与摘要请求共用那个入口。

use super::{Agent, AgentError, Result};
use crate::events::{AgentDiagnostic, AgentEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{LedgerRecord, RequestDefinitions, lock_writer};
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest, ModelTurnResponse,
};
use tokio_util::sync::CancellationToken;

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

impl Agent {
    /// 加载当前 MCP 目录，并将工具定义和服务器说明一起替换。
    pub(super) async fn refresh_tools(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) {
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let discovered = self.mcp.discover(&cwd, cancellation).await;
        for error in &discovered.errors {
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                "mcp_connection_failed",
                error.clone(),
            )));
        }
        self.registry.set_mcp_tools(discovered.tools);
        self.developer_instructions = crate::prompts::assemble_developer_instructions(
            &singularity_core::display_path(&cwd),
            &self.registry,
        );
        if !discovered.instructions.is_empty() {
            self.developer_instructions
                .push_str(&format!("\n\n{}", discovered.instructions.join("\n\n")));
        }
        if !discovered.errors.is_empty() {
            self.developer_instructions.push_str(&format!(
                "\n\nUnavailable MCP servers:\n{}",
                discovered.errors.join("\n")
            ));
        }
    }

    /// 读取手动选择的 skill，并把它的指令追加进持久账本：这一步同时是本轮指令的提交动作。
    pub(super) async fn load_and_record_manual_skill(&mut self, input: &str) -> Result<()> {
        let Some(skill) = self.skills.manual(input) else {
            return Ok(());
        };
        let skill = skill.clone();
        let text = tokio::task::spawn_blocking(move || skill.load())
            .await
            .expect("skill loader completes while the runtime is running")
            .map_err(AgentError::SkillLoad)?;
        self.append_record(LedgerRecord::SkillInstructions { text })
            .await?;
        Ok(())
    }

    /// 压缩后重新读取文件指令与 Skill 目录，直接替换本轮请求使用的内容。
    pub(super) async fn refresh_instructions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let home = self.config.instruction_home.clone();
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let (loaded, skills) = tokio::task::spawn_blocking(move || {
            let loaded = singularity_core::load_agent_instructions(&cwd, &home);
            let skills = singularity_core::skills::SkillCatalog::discover(&cwd, &home);
            (loaded, skills)
        })
        .await
        .expect("instruction loader completes while the runtime is running");
        let loaded = loaded.map_err(AgentError::Instructions)?;
        self.skills = skills;
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

    pub(super) fn request_definitions(&self) -> RequestDefinitions {
        RequestDefinitions::new(&self.instruction_prefix(), self.registry.provider_schemas())
    }

    /// 当前指令和工具定义的请求开销。
    pub(super) fn request_overhead_tokens(&self) -> u64 {
        self.request_definitions().estimated_tokens()
    }

    pub(super) fn context_pressure_tokens(&self) -> u64 {
        self.context.request_tokens(self.request_overhead_tokens())
    }

    /// 组装并发送一次生成请求；发送、预算和观测共用同一份指令与工具定义。
    async fn request_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String)> {
        let prefix = self.instruction_prefix();
        let tools = self.registry.provider_schemas();
        let definitions = RequestDefinitions::new(&prefix, tools.clone());
        let messages =
            Self::with_context(&self.session, &mut self.context, move |session, context| {
                let mut messages = prefix;
                let (materials, directory) = {
                    let writer = lock_writer(session);
                    (context.messages(&writer), writer.image_directory())
                };
                messages.extend(crate::session::context::load_messages(
                    materials, &directory,
                )?);
                Ok(messages)
            })
            .await?;
        let request = ModelTurnRequest {
            messages,
            tools,
            model_preferences: ModelPreferences {
                max_output_tokens: Some(output_budget_tokens(
                    &self.model,
                    self.context.request_tokens(definitions.estimated_tokens()),
                    self.model.max_output_tokens,
                )),
            },
        };
        execute_request(
            self.provider.as_ref(),
            &self.session,
            &mut self.accounting,
            &request,
            &definitions,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Generation,
        )
        .await
    }

    /// 开头是 Harness / Skill 目录的 Developer 消息与当前项目指令快照；其后是可压缩
    /// 对话历史。手动 Skill 指令在触发输入之前，直接用户输入仍保留 User 角色。
    pub(super) fn instruction_prefix(&self) -> Vec<ModelMessage> {
        let mut messages = Vec::new();
        if let Some(instruction) = developer_message(&self.developer_instructions) {
            messages.push(instruction);
        }
        if let Some(catalog) = developer_message(&self.skills.prompt()) {
            messages.push(catalog);
        }
        if let Some(files) = &self.file_instructions {
            messages.push(files.clone());
        }
        messages
    }
}

impl Agent {
    /// 生成一次模型响应：组装请求（含发送前压缩），再交给 provider 发送。
    /// provider 明确返回 ContextLengthExceeded 时强制压缩并重建请求，恢复机会至多一次。
    pub(super) async fn generate_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String)> {
        self.reduce_context_if_needed(on_event, cancellation)
            .await?;
        let overflow = match self.request_response(on_event, cancellation).await {
            Err(AgentError::Provider(error)) if error.is_context_overflow() => error,
            result => return result,
        };
        if !self.force_compact(on_event, cancellation).await? {
            return Err(AgentError::Provider(overflow));
        }
        // 恢复只发生一次；重发的结果直接返回，不重新进入压缩决策。
        self.request_response(on_event, cancellation).await
    }
}
