//! 组装模型请求：本轮的指令和工具定义，加上从会话日志生成的对话上下文。
//! 发送、重试和保存请求记录在 crate::request_execution，普通请求和摘要请求共用。

use super::{Agent, AgentError, RequestContext, Result};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{context, lock_writer};
use singularity_model::Message;
use singularity_model::{ModelTurnRequest, ModelTurnResponse};
use singularity_protocol::TurnEvent;
use tokio_util::sync::CancellationToken;

/// 本轮每次请求都会带上的指令消息和工具定义。
#[derive(Debug, Clone)]
pub(crate) struct RequestDefinitions {
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<singularity_model::ModelToolSchema>,
}

impl RequestDefinitions {
    /// 估算指令消息和工具定义占用的输入 Token 数。
    pub(crate) fn estimated_tokens(&self) -> u64 {
        let instructions = self
            .messages
            .iter()
            .map(|message| context::estimate_tokens_of(&message.content_text()) + 4)
            .sum::<u64>();
        let tools = if self.tools.is_empty() {
            0
        } else {
            context::estimate_tokens_of(
                &serde_json::to_string(&self.tools).expect("tool schemas are serializable"),
            ) + 4
        };
        instructions + tools
    }
}

fn file_instruction_message(instructions: &str) -> Message {
    // 转义正文里的结束标签，否则模型会以为指令在那里结束。
    let instructions = instructions.replace("</file-instructions>", "<\\/file-instructions>");
    Message::user(format!(
        "<file-instructions>\nThese global and project file instructions apply to the current workspace. More specific files take precedence. Direct user instructions take precedence over these file instructions.\n{instructions}\n</file-instructions>"
    ))
}

impl RequestContext {
    /// 本轮首次请求前加载定义，后续请求复用。
    pub(super) async fn ensure_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.definitions.is_none() {
            let (definitions, context) = self.load_definitions(cancellation).await?;
            on_event(TurnEvent::ContextLoaded { context });
            self.definitions = Some(definitions);
        }
        Ok(())
    }

    /// 读取本轮技能与工具目录，组装模型收到的定义。
    pub(super) async fn load_definitions(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<(RequestDefinitions, singularity_protocol::InjectedContext)> {
        let home = self.config.instruction_home.clone();
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let loaded = std::mem::take(&mut self.config.initial_instructions);
        let skills = tokio::task::spawn_blocking(move || {
            singularity_core::skills::SkillCatalog::discover(&cwd, &home)
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
            crate::prompts::assemble_developer_instructions(&singularity_core::display_path(&cwd));
        if !discovered.instructions.is_empty() {
            developer.push_str(&format!("\n\n{}", discovered.instructions.join("\n\n")));
        }
        if !discovered.errors.is_empty() {
            developer.push_str(&format!("\n\nUnavailable MCP servers:\n{}", discovered.errors.join("\n")));
        }
        let mut messages = vec![Message::developer(developer)];
        let skill_prompt = skills.prompt();
        if !skill_prompt.is_empty() {
            messages.push(Message::developer(skill_prompt));
        }
        if !loaded.is_empty() {
            let text = loaded
                .iter()
                .map(|file| format!("# Instructions from {}\n\n{}", file.path.display(), file.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            messages.push(file_instruction_message(&text));
        }
        let tools = registry.provider_schemas();
        let context = singularity_protocol::InjectedContext {
            instructions: loaded
                .into_iter()
                .map(|file| singularity_protocol::InstructionFile {
                    path: singularity_core::display_path(&file.path),
                    content: file.content,
                    global: file.global,
                })
                .collect(),
            skills: skills.model_skills().map(|skill| skill.name.clone()).collect(),
            tools: tools.iter().map(|tool| tool.name.clone()).collect(),
        };
        Ok((RequestDefinitions { messages, tools }, context))
    }

    pub(super) fn request_definitions(&self) -> &RequestDefinitions {
        self.definitions.as_ref().expect("definitions are loaded before model requests")
    }

    /// 估算下一次请求的输入 Token 数（最近一次实际用量加上新增消息，或全量估算），供自动压缩判断使用。
    pub(super) fn context_pressure_tokens(&self) -> u64 {
        let writer = lock_writer(&self.session);
        context::estimated_tokens(
            &writer,
            self.provider.model_identity(),
            self.request_definitions().estimated_tokens(),
        )
    }

    /// 组装并发送一次普通请求；输入 Token 估算与压缩判断用同一规则。
    async fn request_response(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String)> {
        let definitions = self.request_definitions();
        let tools = definitions.tools.clone();
        let mut messages = definitions.messages.clone();
        let overhead_tokens = definitions.estimated_tokens();
        let (provider, model_name) = self.provider.model_identity();
        let identity = (provider.to_owned(), model_name.to_owned());
        let session = std::sync::Arc::clone(&self.session);
        let model = self.provider.model_configuration();
        let request = tokio::task::spawn_blocking(move || {
            let (materials, input_tokens, directory) = {
                let writer = lock_writer(&session);
                let (materials, tokens) =
                    context::prepare_context(&writer, (&identity.0, &identity.1), overhead_tokens);
                (materials, tokens, writer.image_directory())
            };
            messages.extend(materials);
            let images = context::load_images(&mut messages, &directory)?;
            Ok::<_, crate::session::SessionError>(ModelTurnRequest {
                messages,
                images,
                tools,
                max_output_tokens: output_budget_tokens(&model, input_tokens, model.max_output_tokens),
            })
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
    }
}

impl Agent {
    /// 生成一次模型回复：需要时先压缩，再组装并发送请求。
    /// 提供方明确返回上下文超限时，压缩后重新组装并重发，最多一次。
    pub(super) async fn generate_response(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String)> {
        self.requests.reduce_context_if_needed(on_event, cancellation).await?;
        self.drain_inputs(on_event, cancellation).await?;
        let overflow = match self.requests.request_response(on_event, cancellation).await {
            Err(AgentError::Provider(error)) if error.is_context_overflow() => error,
            result => return result,
        };
        if !self.requests.compact_now(on_event, cancellation).await? {
            return Err(AgentError::Provider(overflow));
        }
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        // 压缩后只重发一次，仍然超限时直接把错误返回给调用方。
        self.drain_inputs(on_event, cancellation).await?;
        self.requests.request_response(on_event, cancellation).await
    }
}
