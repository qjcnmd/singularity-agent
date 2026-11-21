//! 组装模型请求：本轮的指令和工具定义，加上从会话日志生成的对话上下文。
//! 发送、重试和保存请求记录在 crate::request_execution，普通请求和整理请求共用。

use super::{Agent, AgentError, AgentEvent, RequestContext, Result, modules};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{LedgerRecord, context, with_writer, with_writer_async};
use model::Message;
use model::{ModelTurnRequest, ModelTurnResponse};
use protocol::TurnEvent;
use tokio_util::sync::CancellationToken;

/// 本轮每次请求都会带上的指令消息和工具定义。
#[derive(Debug, Clone)]
pub(crate) struct RequestDefinitions {
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<app_core::ToolDefinition>,
    pub(crate) estimated_tokens: u64,
}

impl RequestDefinitions {
    /// 定义在本轮内不变，加载时一次计算预算，普通请求和整理请求直接复用。
    fn new(messages: Vec<Message>, tools: Vec<app_core::ToolDefinition>) -> Self {
        let instructions = messages.iter().map(context::message_token_estimate).sum::<u64>();
        let tool_tokens = app_core::estimate_tool_tokens(&tools);
        Self {
            messages,
            tools,
            estimated_tokens: instructions + tool_tokens,
        }
    }
}

impl RequestContext {
    /// 本轮首次请求前加载定义，后续请求复用。
    pub(super) async fn ensure_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.definitions.is_none() {
            let (definitions, context) = self.load_definitions(cancellation).await?;
            on_event(AgentEvent::Turn(TurnEvent::ContextLoaded { context }));
            self.definitions = Some(definitions);
        }
        Ok(())
    }

    /// 读取本轮技能与工具目录，按本任务开着的模块组装模型收到的定义。
    pub(super) async fn load_definitions(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<(RequestDefinitions, protocol::InjectedContext)> {
        let home = self.config.instruction_home.clone();
        let session = std::sync::Arc::clone(&self.session);
        let instructions = self.config.initial_instructions.clone();
        let disabled = &self.config.disabled_modules;
        let (cwd, skills) = tokio::task::spawn_blocking(move || {
            let cwd = with_writer(&session, |writer| writer.data().cwd().to_path_buf());
            let skills = app_core::skills::SkillCatalog::discover(&cwd, &home);
            (cwd, skills)
        })
        .await
        .expect("skill discovery completes while the runtime is running");
        let skipped_servers: Vec<String> = disabled
            .iter()
            .filter_map(|id| id.strip_prefix(modules::MCP_PREFIX))
            .map(str::to_owned)
            .collect();
        let discovered = self.mcp.discover(&cwd, cancellation, &skipped_servers).await;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let assembly = modules::assemble(modules::ModuleInputs {
            cwd: &app_core::display_path(&cwd),
            instructions,
            skills,
            mcp: discovered,
            disabled,
        });
        let tools = assembly.registry.provider_schemas();
        let definitions = RequestDefinitions::new(assembly.messages, tools);
        self.registry = assembly.registry;
        let context = protocol::InjectedContext { modules: assembly.modules };
        Ok((definitions, context))
    }

    pub(super) fn request_definitions(&self) -> &RequestDefinitions {
        self.definitions.as_ref().expect("definitions are loaded before model requests")
    }

    /// 达到软压力阈值时先执行零模型调用的旧工具输出微压缩，并返回最新的输入 Token 估算供自动轮换判断。
    pub(super) async fn prune_tool_outputs_if_needed(&self) -> Result<u64> {
        let session = std::sync::Arc::clone(&self.session);
        let identity = (self.provider.provider_name().to_owned(), self.provider.model_name().to_owned());
        let overhead_tokens = self.request_definitions().estimated_tokens;
        let context_window = self.context_window();
        let (current_tokens, candidate) = tokio::task::spawn_blocking(move || {
            with_writer(&session, |writer| {
                context::select_prune_target(
                    writer.data(),
                    (&identity.0, &identity.1),
                    overhead_tokens,
                    context_window,
                )
            })
        })
        .await
        .expect("prune target selection completes while the runtime is running");
        if let Some((before_entry_id, saved_tokens)) = candidate {
            tracing::info!(
                before_entry_id = %before_entry_id,
                saved_tokens,
                "pruning older tool outputs to defer full context rotation"
            );
            with_writer_async(&self.session, move |writer| {
                writer.append_record(LedgerRecord::ToolOutputsPruned { before_entry_id })
            })
            .await?;
        }
        Ok(current_tokens)
    }

    /// 组装并发送一次普通请求；输入 Token 估算与轮换判断用同一规则。
    async fn request_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String, String)> {
        let definitions = self.request_definitions();
        let tools = definitions.tools.clone();
        let mut messages = definitions.messages.clone();
        let overhead_tokens = definitions.estimated_tokens;
        let identity = (self.provider.provider_name().to_owned(), self.provider.model_name().to_owned());
        let session = std::sync::Arc::clone(&self.session);
        let model = self.provider.model_configuration();
        let request = tokio::task::spawn_blocking(move || {
            let (materials, input_tokens, directory) = with_writer(&session, |writer| {
                let (materials, tokens) =
                    context::prepare_context(writer.data(), (&identity.0, &identity.1), overhead_tokens);
                (materials, tokens, writer.data().image_directory())
            });
            messages.extend(materials);
            let images = context::load_images(&mut messages, &directory)?;
            Ok::<_, std::io::Error>(ModelTurnRequest {
                messages,
                images,
                tools,
                max_output_tokens: output_budget_tokens(model, input_tokens),
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
            protocol::RequestPurpose::Generation,
        )
        .await
    }
}

impl Agent {
    /// 生成一次模型回复：先按需微压缩旧工具输出，仍接近上限时再轮换，然后组装并发送请求。
    /// 提供方明确返回上下文超限时，强制降级轮换后重新组装并重发，最多一次。
    pub(super) async fn generate_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String, String)> {
        let pressure_tokens = self.requests.prune_tool_outputs_if_needed().await?;
        self.requests.rotate_context_if_needed(pressure_tokens, on_event, cancellation).await?;
        self.drain_inputs(on_event, cancellation).await?;
        let overflow = match self.requests.request_response(on_event, cancellation).await {
            Err(AgentError::Provider(error)) if error.is_context_overflow() => error,
            result => return result,
        };
        tracing::warn!(error = %overflow, "provider reported context overflow; triggering forced rotation");
        if !self.requests.rotate_now(on_event, cancellation, true).await? {
            return Err(AgentError::Provider(overflow));
        }
        // 轮换后只重发一次，仍然超限时直接把错误返回给调用方。
        self.drain_inputs(on_event, cancellation).await?;
        self.requests.request_response(on_event, cancellation).await
    }
}
