//! Agent 请求装配：把指令、历史上下文与压缩后的材料组装成模型输入。
//! 请求执行（attempt 循环、重试等待与账本记录）在 crate::request_execution，
//! 生成请求与摘要请求共用那个入口。

use super::{Agent, AgentError, Result};
use crate::events::AgentEvent;
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{RequestDefinitions, context, lock_writer};
use singularity_model::{ModelRole, ModelTurnRequest, ModelTurnResponse};
use singularity_protocol::{RequestMessage, RequestPreferences};
use tokio_util::sync::CancellationToken;

fn file_instruction_message(instructions: &str) -> RequestMessage {
    // 文件正文自带的闭合标签会提前截断这条消息。
    let instructions = instructions.replace("</file-instructions>", "<\\/file-instructions>");
    RequestMessage {
        name: "文件指令".into(),
        role: ModelRole::User,
        content: format!(
            "<file-instructions>\nThese global and project file instructions apply to the current workspace. More specific files take precedence. Direct user instructions take precedence over these file instructions.\n{instructions}\n</file-instructions>"
        ),
    }
}

impl Agent {
    /// 本轮首次请求前加载并保存定义，后续请求复用。
    pub(super) async fn ensure_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.definitions.is_none() {
            let definitions = self.load_definitions(cancellation).await?;
            let sections = definitions.sections();
            let injected = crate::session::with_writer_async(&self.session, move |writer| {
                writer.record_context(sections)
            })
            .await?;
            for item in injected {
                on_event(AgentEvent::ContextInjected { item });
            }
            self.definitions = Some(definitions);
        }
        Ok(())
    }

    /// 读取本轮技能与工具目录，组装模型收到的定义。
    pub(super) async fn load_definitions(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<RequestDefinitions> {
        let home = self.config.instruction_home.clone();
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded = self.config.initial_instructions.take();
        let skills = tokio::task::spawn_blocking(move || {
            singularity_core::skills::SkillCatalog::discover(&cwd, &home).prompt()
        })
        .await
        .expect("skill discovery completes while the runtime is running");
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let discovered = self.mcp.discover(&cwd, cancellation).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        self.registry.set_mcp_tools(discovered.tools);
        let registry = &self.registry;
        let mut developer =
            crate::prompts::assemble_developer_instructions(&singularity_core::display_path(&cwd), registry);
        if !discovered.instructions.is_empty() {
            developer.push_str(&format!("\n\n{}", discovered.instructions.join("\n\n")));
        }
        if !discovered.errors.is_empty() {
            developer.push_str(&format!("\n\nUnavailable MCP servers:\n{}", discovered.errors.join("\n")));
        }
        let mut messages = vec![RequestMessage {
            name: "Harness 指令".into(),
            role: ModelRole::Developer,
            content: developer,
        }];
        if !skills.is_empty() {
            messages.push(RequestMessage {
                name: "Skills 目录".into(),
                role: ModelRole::Developer,
                content: skills,
            });
        }
        if let Some(files) = loaded.as_deref().map(file_instruction_message) {
            messages.push(files);
        }
        Ok(RequestDefinitions {
            messages,
            tools: registry.provider_schemas(),
        })
    }

    pub(super) fn request_definitions(&self) -> &RequestDefinitions {
        self.definitions.as_ref().expect("definitions are loaded before model requests")
    }

    /// 生成预算与自动压缩共用：有效对话加上固定定义和本轮实测校正。
    pub(super) fn context_pressure_tokens(&self) -> u64 {
        let writer = lock_writer(&self.session);
        context::estimated_tokens(&writer)
            .saturating_add(self.request_definitions().estimated_tokens())
            .saturating_add(self.usage_correction)
    }

    /// 发送、预算和观测共用同一份定义；返回未校正估价，响应后无需重新扫描会话。
    async fn request_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String, u64)> {
        // 压缩期间收到的新消息在摘要保存后交给下一次生成请求。
        self.drain_inputs(on_event, cancellation).await?;
        let definitions = self.request_definitions();
        let tools = definitions.tools.clone();
        let mut messages = definitions.model_messages();
        let overhead_tokens = definitions.estimated_tokens();
        let usage_correction = self.usage_correction;
        let session = std::sync::Arc::clone(&self.session);
        let model = self.provider.model_configuration();
        let (request, baseline_tokens) = tokio::task::spawn_blocking(move || {
            let (materials, context_tokens, directory) = {
                let writer = lock_writer(&session);
                let (materials, tokens) = context::prepare_context(&writer);
                (materials, tokens, writer.image_directory())
            };
            messages.extend(context::load_messages(materials, &directory)?);
            let baseline_tokens = context_tokens.saturating_add(overhead_tokens);
            let input_tokens = baseline_tokens.saturating_add(usage_correction);
            Ok::<_, crate::session::SessionError>((
                ModelTurnRequest {
                    messages,
                    tools,
                    model_preferences: RequestPreferences {
                        max_output_tokens: output_budget_tokens(
                            &model,
                            input_tokens,
                            model.max_output_tokens,
                        ),
                    },
                },
                baseline_tokens,
            ))
        })
        .await
        .expect("context worker completes while the runtime is running")?;
        execute_request(
            &self.provider,
            &self.session,
            &request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Generation,
        )
        .await
        .map(|(response, id)| (response, id, baseline_tokens))
    }
}

impl Agent {
    /// 生成一次模型响应：组装请求（含发送前压缩），再交给 provider 发送。
    /// provider 明确返回 ContextLengthExceeded 时强制压缩并重建请求，恢复机会至多一次。
    pub(super) async fn generate_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String, u64)> {
        self.reduce_context_if_needed(on_event, cancellation).await?;
        let overflow = match self.request_response(on_event, cancellation).await {
            Err(AgentError::Provider(error)) if error.is_context_overflow() => error,
            result => return result,
        };
        if !self.compact_now(on_event, cancellation).await.into_result()? {
            return Err(AgentError::Provider(overflow));
        }
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        // 恢复只发生一次；重发的结果直接返回，不重新进入压缩决策。
        self.request_response(on_event, cancellation).await
    }
}
