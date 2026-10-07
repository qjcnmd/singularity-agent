//! Agent 请求装配：把指令、历史上下文与压缩后的材料组装成模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成请求与摘要请求共用那个入口。

use super::{Agent, AgentError, Result};
use crate::events::{AgentDiagnostic, AgentEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{RequestDefinitions, lock_writer};
use singularity_model::{ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest, ModelTurnResponse};
use tokio_util::sync::CancellationToken;

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
            self.developer_instructions
                .push_str(&format!("\n\nUnavailable MCP servers:\n{}", discovered.errors.join("\n")));
        }
    }

    /// 压缩后重新读取文件指令与 Skill 目录，直接替换本轮请求使用的内容。
    pub(super) async fn refresh_instructions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let home = self.config.instruction_home.clone();
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let (loaded, skill_instructions) = tokio::task::spawn_blocking(move || {
            let loaded = singularity_core::load_agent_instructions(&cwd, &home);
            let skill_instructions = singularity_core::skills::SkillCatalog::discover(&cwd, &home).prompt();
            (loaded, skill_instructions)
        })
        .await
        .expect("instruction loader completes while the runtime is running");
        let loaded = loaded.map_err(AgentError::Instructions)?;
        self.skill_instructions = skill_instructions;
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
        self.usage_correction = 0;
        if loaded.as_ref().is_some_and(singularity_core::ProjectInstructions::truncated) {
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
        self.request_tokens(self.request_overhead_tokens())
    }

    /// 从账本派生历史估价，本轮 Agent 添加请求定义开销与实测校正。
    fn request_tokens(&self, overhead: u64) -> u64 {
        let writer = lock_writer(&self.session);
        writer
            .context()
            .estimated_tokens(&writer)
            .saturating_add(overhead)
            .saturating_add(self.usage_correction)
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
        let session = std::sync::Arc::clone(&self.session);
        let messages = tokio::task::spawn_blocking(move || {
            let mut messages = prefix;
            let (materials, directory) = {
                let writer = lock_writer(&session);
                (writer.context().messages(&writer), writer.image_directory())
            };
            messages.extend(crate::session::context::load_messages(materials, &directory)?);
            Ok::<_, crate::session::SessionError>(messages)
        })
        .await
        .expect("context worker completes while the runtime is running")?;
        let request = ModelTurnRequest {
            messages,
            tools,
            model_preferences: ModelPreferences {
                max_output_tokens: Some(output_budget_tokens(
                    &self.model,
                    self.request_tokens(definitions.estimated_tokens()),
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
    /// 对话历史。技能文件引用随用户输入进入历史，正文通过普通工具结果交付。
    pub(super) fn instruction_prefix(&self) -> Vec<ModelMessage> {
        let mut messages = Vec::new();
        // Harness 指令先于 Skill 目录；不支持 Developer 的端点由 Provider 降级。
        for instruction in [&self.developer_instructions, &self.skill_instructions] {
            if !instruction.is_empty() {
                messages.push(ModelMessage::text(ModelRole::Developer, instruction));
            }
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
        self.reduce_context_if_needed(on_event, cancellation).await?;
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
