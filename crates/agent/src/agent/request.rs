use super::compaction::{CompactionMode, PreRequestOutcome};
use super::{Agent, AgentError, AgentEvent, RequestContext, Result, modules};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{context, context::PreparedContext, with_writer};
use model::Message;
use model::{ModelTurnRequest, ModelTurnResponse};
use protocol::TurnEvent;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct RequestDefinitions {
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<app_core::ToolDefinition>,
    pub(crate) estimated_tokens: u64,
}

impl RequestDefinitions {
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

    pub(super) async fn load_definitions(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<(RequestDefinitions, protocol::InjectedContext)> {
        let home = self.config.instruction_home.clone();
        let session = std::sync::Arc::clone(&self.session);
        let models = std::sync::Arc::clone(&self.models);
        let instructions = self.config.initial_instructions.clone();
        let disabled = &self.config.disabled_modules;
        let (cwd, skills, web) = tokio::task::spawn_blocking(move || {
            let cwd = with_writer(&session, |writer| writer.data().cwd().to_path_buf());
            let skills = app_core::skills::SkillCatalog::discover(&cwd, &home);
            let web = models.web_search_access().map(std::sync::Arc::new);
            (cwd, skills, web)
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
            web,
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

    async fn request_response(
        &mut self,
        prepared: Option<PreparedContext>,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> std::result::Result<(ModelTurnResponse, String, String), (AgentError, u64)> {
        let definitions = self.request_definitions();
        let tools = definitions.tools.clone();
        let mut messages = definitions.messages.clone();
        let overhead_tokens = definitions.estimated_tokens;
        let session = std::sync::Arc::clone(&self.session);
        let identity = (self.provider.provider_name().to_owned(), self.provider.model_name().to_owned());
        let model = self.provider.model_configuration();
        let (request, input_tokens) = tokio::task::spawn_blocking(move || {
            let prepared = prepared.unwrap_or_else(|| {
                with_writer(&session, |writer| {
                    context::prepare_context(writer.data(), (&identity.0, &identity.1), overhead_tokens)
                })
            });
            let input_tokens = prepared.input_tokens;
            messages.extend(prepared.messages);
            let images = context::load_images(&mut messages, &prepared.image_directory)?;
            Ok::<_, std::io::Error>((
                ModelTurnRequest {
                    messages,
                    images,
                    tools,
                    max_output_tokens: output_budget_tokens(model, input_tokens),
                    prompt_cache_key: Some(prepared.session_id),
                },
                input_tokens,
            ))
        })
        .await
        .expect("context worker completes while the runtime is running")
        .map_err(|error| (AgentError::from(error), 0))?;
        execute_request(
            &self.provider,
            &self.session,
            &request,
            Some(overhead_tokens),
            on_event,
            cancellation,
            protocol::RequestPurpose::Generation,
        )
        .await
        .map_err(|error| (error, input_tokens))
    }
}

impl Agent {
    async fn drain_and_prepare(
        &mut self,
        switch_compacted: &mut bool,
        auto_attempted: &mut bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<Option<PreparedContext>> {
        loop {
            self.drain_inputs(on_event, cancellation).await?;
            let prepared = match self
                .requests
                .prepare_before_request(*switch_compacted, !*auto_attempted, on_event, cancellation)
                .await?
            {
                PreRequestOutcome::Ready(context) => Some(context),
                PreRequestOutcome::SwitchCompacted => {
                    *switch_compacted = true;
                    None
                }
                PreRequestOutcome::AutoAttempted => {
                    *auto_attempted = true;
                    None
                }
            };
            if !self.has_pending_inputs() {
                return Ok(prepared);
            }
        }
    }

    pub(super) async fn generate_response(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<(ModelTurnResponse, String, String)> {
        let mut switch_compacted = false;
        let mut auto_attempted = false;
        let prepared = self
            .drain_and_prepare(&mut switch_compacted, &mut auto_attempted, on_event, cancellation)
            .await?;
        let (overflow, overflow_input_tokens) =
            match self.requests.request_response(prepared, on_event, cancellation).await {
                Err((AgentError::Provider(error), input_tokens)) if error.is_context_overflow() => {
                    (error, input_tokens)
                }
                Ok(response) => return Ok(response),
                Err((error, _)) => return Err(error),
            };
        tracing::warn!(error = %overflow, "provider reported context overflow; triggering forced compaction");
        match self
            .requests
            .compact_now(on_event, cancellation, CompactionMode::Overflow { overflow_input_tokens })
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(AgentError::Provider(overflow)),
            Err(AgentError::Aborted) => return Err(AgentError::Aborted),
            Err(error) if error.is_session_failure() => return Err(error),
            Err(error) => {
                tracing::warn!(error = %error, "overflow compaction failed; preserving original overflow error");
                return Err(AgentError::Provider(overflow));
            }
        }
        auto_attempted = true;
        let prepared = self
            .drain_and_prepare(&mut switch_compacted, &mut auto_attempted, on_event, cancellation)
            .await?;
        self.requests
            .request_response(prepared, on_event, cancellation)
            .await
            .map_err(|(error, _)| error)
    }
}
