//! Agent 请求装配：把指令、历史上下文与压缩后的材料组装成模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成请求与摘要请求共用那个入口。

use super::{Agent, AgentError, Result};
use crate::events::{AgentDiagnostic, AgentEvent};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{RequestDefinitions, context, lock_writer};
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
    /// 只在会话没有固定定义时加载；已有会话包括重启恢复都沿用保存的定义。
    pub(super) async fn ensure_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.definitions.is_none() {
            let (definitions, registry) = self.load_definitions(on_event, cancellation).await?;
            let saved = definitions.clone();
            crate::session::with_writer_async(&self.session, move |writer| {
                writer.append_record(crate::session::LedgerRecord::RequestDefinitions { definitions: saved })
            })
            .await?;
            self.definitions = Some(definitions);
            self.registry = registry;
        }
        Ok(())
    }

    /// 首次加载和压缩成功后共用；返回新定义，调用方保存成功后才替换当前内容。
    pub(super) async fn load_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(RequestDefinitions, crate::tools::ToolRegistrySnapshot)> {
        let home = self.config.instruction_home.clone();
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let initial = self.definitions.is_none();
        let initial_instructions = self.config.initial_instructions.take();
        let instruction_cwd = cwd.clone();
        let (loaded, skills) = tokio::task::spawn_blocking(move || {
            let loaded = if initial {
                Ok(initial_instructions)
            } else {
                singularity_core::load_agent_instructions(&instruction_cwd, &home)
            };
            let skills = singularity_core::skills::SkillCatalog::discover(&instruction_cwd, &home).prompt();
            (loaded, skills)
        })
        .await
        .expect("instruction loader completes while the runtime is running");
        let loaded = loaded.map_err(AgentError::Instructions)?;
        if loaded.as_ref().is_some_and(singularity_core::ProjectInstructions::truncated) {
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
                singularity_protocol::diagnostic_code::PROJECT_INSTRUCTIONS_TRUNCATED,
                "project instructions were truncated because they exceeded the size budget",
            )));
        }
        let discovered = self.mcp.discover(&cwd, cancellation).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let mut registry = crate::tools::ToolRegistrySnapshot::default();
        if self.questions.is_some() {
            registry.enable_questions();
        }
        registry.set_mcp_tools(discovered.tools);
        let mut developer =
            crate::prompts::assemble_developer_instructions(&singularity_core::display_path(&cwd), &registry);
        if !discovered.instructions.is_empty() {
            developer.push_str(&format!("\n\n{}", discovered.instructions.join("\n\n")));
        }
        if !discovered.errors.is_empty() {
            developer.push_str(&format!("\n\nUnavailable MCP servers:\n{}", discovered.errors.join("\n")));
        }
        for error in discovered.errors {
            on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning("mcp_connection_failed", error)));
        }
        let mut messages = vec![ModelMessage::text(ModelRole::Developer, developer)];
        if !skills.is_empty() {
            messages.push(ModelMessage::text(ModelRole::Developer, skills));
        }
        if let Some(files) = loaded
            .as_ref()
            .map(singularity_core::ProjectInstructions::content)
            .and_then(file_instruction_message)
        {
            messages.push(files);
        }
        Ok((RequestDefinitions::new(&messages, registry.provider_schemas()), registry))
    }

    pub(super) fn request_definitions(&self) -> &RequestDefinitions {
        self.definitions.as_ref().expect("definitions are loaded before model requests")
    }

    /// 普通请求读取固定前缀与完整有效对话；组装不会重新发现或刷新定义。
    pub(super) async fn model_messages(&self) -> Result<Vec<ModelMessage>> {
        let prefix = self.request_definitions().model_messages();
        let session = std::sync::Arc::clone(&self.session);
        Ok(tokio::task::spawn_blocking(move || {
            let mut messages = prefix;
            let (materials, directory) = {
                let writer = lock_writer(&session);
                (context::messages(&writer), writer.image_directory())
            };
            messages.extend(context::load_messages(materials, &directory)?);
            Ok::<_, crate::session::SessionError>(messages)
        })
        .await
        .expect("context worker completes while the runtime is running")?)
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
        context::estimated_tokens(&writer)
            .saturating_add(overhead)
            .saturating_add(self.usage_correction)
    }

    /// 组装并发送一次生成请求；发送、预算和观测共用同一份指令与工具定义。
    async fn request_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String)> {
        let definitions = self.request_definitions().clone();
        let tools = definitions.model_tools();
        let messages = self.model_messages().await?;
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
            &self.provider,
            &self.session,
            &mut self.accounting,
            &request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Generation,
        )
        .await
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
        if !self.compact_now(on_event).await? {
            return Err(AgentError::Provider(overflow));
        }
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        // 恢复只发生一次；重发的结果直接返回，不重新进入压缩决策。
        self.request_response(on_event, cancellation).await
    }
}
