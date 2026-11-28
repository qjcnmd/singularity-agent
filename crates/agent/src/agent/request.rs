use super::compaction::{CompactionMode, PreRequestOutcome};
use super::{Agent, AgentError, AgentEvent, RequestContext, Result, modules};
use crate::request_execution::{execute_request, output_budget_tokens};
use crate::session::{context, context::PreparedContext, with_writer, with_writer_async};
use crate::tools::ToolRegistrySnapshot;
use model::Message;
use model::{ModelTurnRequest, ModelTurnResponse};
use protocol::TurnEvent;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct RequestDefinitions {
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<model::ToolDefinition>,
    pub(crate) estimated_tokens: u64,
}

impl RequestDefinitions {
    fn new(messages: Vec<Message>, tools: Vec<model::ToolDefinition>) -> Self {
        let instructions = messages.iter().map(context::message_token_estimate).sum::<u64>();
        let tool_tokens = model::estimate_tool_tokens(&tools);
        Self {
            messages,
            tools,
            estimated_tokens: instructions + tool_tokens,
        }
    }
}

#[derive(Clone)]
struct LockedPrefix {
    definitions: RequestDefinitions,
    registry: ToolRegistrySnapshot,
    context: Vec<protocol::ContextModule>,
}

#[derive(Default)]
pub struct PrefixCache {
    locked: Mutex<Option<LockedPrefix>>,
}

impl std::fmt::Debug for PrefixCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixCache").field("locked", &self.is_locked()).finish()
    }
}

impl PrefixCache {
    pub fn invalidate(&self) {
        *self.locked.lock().expect("prefix cache lock poisoned") = None;
    }

    pub fn is_locked(&self) -> bool {
        self.locked.lock().expect("prefix cache lock poisoned").is_some()
    }

    fn get(&self) -> Option<LockedPrefix> {
        self.locked.lock().expect("prefix cache lock poisoned").clone()
    }

    fn set(&self, prefix: LockedPrefix) {
        *self.locked.lock().expect("prefix cache lock poisoned") = Some(prefix);
    }
}

impl RequestContext {
    pub(super) async fn ensure_definitions(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.definitions.is_some() {
            return Ok(());
        }
        let idle_cold = with_writer_async(&self.session, |writer| {
            let cold = context::is_session_idle_cold(writer.data(), &app_core::now_iso());
            if cold {
                context::clear_idle_tool_results(writer)?;
            }
            Ok(cold)
        })
        .await?;
        if idle_cold {
            self.prefix_cache.invalidate();
        }
        if let Some(locked) = self.prefix_cache.get() {
            on_event(AgentEvent::Turn(TurnEvent::ContextLoaded { context: locked.context }));
            self.registry = locked.registry;
            self.definitions = Some(locked.definitions);
            return Ok(());
        }
        let (definitions, context) = self.load_definitions(cancellation).await?;
        self.prefix_cache.set(LockedPrefix {
            definitions: definitions.clone(),
            registry: self.registry.clone(),
            context: context.clone(),
        });
        on_event(AgentEvent::Turn(TurnEvent::ContextLoaded { context }));
        self.definitions = Some(definitions);
        Ok(())
    }

    pub(super) async fn load_definitions(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<(RequestDefinitions, Vec<protocol::ContextModule>)> {
        let cwd = with_writer(&self.session, |writer| writer.data().cwd().to_path_buf());
        if !std::mem::take(&mut self.preloaded_instructions) {
            let instruction_cwd = cwd.clone();
            let instruction_home = self.config.instruction_home.clone();
            self.config.initial_instructions = tokio::task::spawn_blocking(move || {
                app_core::load_agent_instructions(&instruction_cwd, &instruction_home)
            })
            .await
            .expect("instruction loader completes while the runtime is running")
            .map_err(std::io::Error::other)?;
        }
        let assembly =
            modules::load(&cwd, &self.config, std::sync::Arc::clone(&self.models), &self.mcp, cancellation)
                .await?;
        let tools = assembly.registry.provider_schemas();
        let definitions = RequestDefinitions::new(assembly.messages, tools);
        self.registry = assembly.registry;
        Ok((definitions, assembly.modules))
    }

    pub(super) fn request_definitions(&self) -> &RequestDefinitions {
        self.definitions.as_ref().expect("definitions are loaded before model requests")
    }

    async fn request_response(
        &mut self,
        prepared: PreparedContext,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> std::result::Result<(ModelTurnResponse, String, String), (AgentError, u64)> {
        let definitions = self.request_definitions();
        let tools = definitions.tools.clone();
        let mut messages = definitions.messages.clone();
        let model = self.provider.model_configuration();
        let (request, input_tokens) = tokio::task::spawn_blocking(move || {
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
        auto_attempted: &mut bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<PreparedContext> {
        loop {
            self.drain_inputs(on_event, cancellation).await?;
            let prepared =
                match self.requests.prepare_before_request(!*auto_attempted, on_event, cancellation).await? {
                    PreRequestOutcome::Ready(context) => context,
                    PreRequestOutcome::AutoAttempted => {
                        *auto_attempted = true;
                        continue;
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
        let mut auto_attempted = false;
        let prepared = self.drain_and_prepare(&mut auto_attempted, on_event, cancellation).await?;
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
        let prepared = self.drain_and_prepare(&mut auto_attempted, on_event, cancellation).await?;
        self.requests
            .request_response(prepared, on_event, cancellation)
            .await
            .map_err(|(error, _)| error)
    }
}
