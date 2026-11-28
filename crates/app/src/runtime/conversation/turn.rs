use std::sync::Arc;

use agent::agent::{Agent, AgentConfig, AgentError, AgentEvent, RequestContext};
use agent::session::{LedgerRecord, lock_writer, with_writer_async};
use model::ModelClient;

use protocol::{TurnErrorDetail, TurnEvent, TurnStatus};
use tokio_util::sync::CancellationToken;

use super::state::TurnControls;
use super::{Conversation, ConversationError};

impl Conversation {
    pub(super) async fn run_single_turn(
        self: &Arc<Self>,
        controls: &Arc<TurnControls>,
        sink: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let preparing = Arc::clone(controls);
        let prepared = tokio::task::spawn_blocking(move || conversation.prepare_turn(&preparing))
            .await
            .expect("turn preparation completes while the runtime is running");
        let ReadyTurn { mut agent, started_at, context_window } = match prepared {
            Ok(PreparedTurn::Ready(ready)) => *ready,
            Ok(PreparedTurn::Cancelled) => {
                controls.finish_cancel();
                return Ok(TurnStatus::Interrupted);
            }
            Err(error) => return finish_preparation_error(controls, error),
        };
        self.lock_inner().state.last_context_window = Some(context_window);
        let turn_id = controls.turn_id.clone();
        sink(AgentEvent::Turn(TurnEvent::TurnStarted { started_at }));

        let run_result = {
            let mut on_event = |event: AgentEvent| match &event {
                AgentEvent::Turn(TurnEvent::ItemUpdated { item })
                    if matches!(
                        item,
                        protocol::HistoryItem::Compaction { .. }
                            | protocol::HistoryItem::CompactionResult { .. }
                    ) =>
                {
                    let active = matches!(
                        item,
                        protocol::HistoryItem::CompactionResult { status: TurnStatus::Running, .. }
                    );
                    controls.set_compacting(active);
                    sink(event);
                    sink(AgentEvent::SessionStateChanged);
                }
                AgentEvent::Turn(TurnEvent::MessageDiscarded { .. }) if controls.is_compacting() => {
                    controls.set_compacting(false);
                    sink(event);
                    sink(AgentEvent::SessionStateChanged);
                }
                _ => sink(event),
            };
            agent.run(&mut on_event, controls.cancellation()).await
        };
        let cancel_accepted = controls.finish_cancel();
        let (turn_status, error) = match run_result {
            Ok(()) if cancel_accepted => (TurnStatus::Interrupted, None),
            Ok(()) => (TurnStatus::Completed, None),
            Err(AgentError::Aborted) => (TurnStatus::Interrupted, None),
            Err(error) => {
                if error.is_session_failure() {
                    return Err(error.detail());
                }
                (TurnStatus::Failed, Some(error.detail()))
            }
        };
        let record = LedgerRecord::OperationFinished {
            turn_id: turn_id.clone(),
            outcome: turn_status,
            error: error.clone(),
            user_stopped: cancel_accepted,
        };
        let finished_at =
            match with_writer_async(&self.writer, move |writer| writer.append_record(record)).await {
                Ok(timestamp) => timestamp,
                Err(storage_error) => {
                    let message = match error {
                        Some(error) => format!("{}; 保存执行结果失败：{storage_error}", error.message),
                        None => format!("保存执行结果失败：{storage_error}"),
                    };
                    return Err(TurnErrorDetail { message, item_id: None });
                }
            };
        let manually_stopped = crate::runtime::history::is_manually_stopped(turn_status, cancel_accepted);
        if let Some(error) = &error {
            sink(AgentEvent::Turn(TurnEvent::TurnFailed {
                error: error.clone(),
                finished_at,
                stats: None,
            }));
        } else {
            sink(AgentEvent::Turn(TurnEvent::TurnCompleted {
                status: turn_status,
                manually_stopped,
                finished_at,
                stats: None,
            }));
        }
        Ok(turn_status)
    }

    pub(super) async fn compact_thread(
        self: &Arc<Self>,
        controls: &TurnControls,
        sink: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let prepared = tokio::task::spawn_blocking(move || conversation.prepare_request_context())
            .await
            .expect("compaction preparation completes while the runtime is running");
        let mut requests = match prepared {
            Ok(requests) => requests,
            Err(error) => return finish_preparation_error(controls, error),
        };
        let result = requests.compact_manual(sink, controls.cancellation()).await;
        let cancelled = controls.finish_cancel();
        match result {
            Ok(()) if cancelled => Ok(TurnStatus::Interrupted),
            Ok(()) => Ok(TurnStatus::Completed),
            Err(AgentError::Aborted) => Ok(TurnStatus::Interrupted),
            Err(error) if error.is_session_failure() => Err(error.detail()),
            Err(_) if cancelled => Ok(TurnStatus::Interrupted),
            Err(error) => Err(error.detail()),
        }
    }

    fn prepare_turn(&self, controls: &TurnControls) -> Result<PreparedTurn, TurnErrorDetail> {
        if controls.cancellation().is_cancelled() {
            return Ok(PreparedTurn::Cancelled);
        }
        let inbox = Arc::clone(&self.lock_inner().state.steering_inbox);
        let context = self.prepare_request_context()?;
        let context_window = context.context_window();
        let agent = Agent::new(context, inbox, Arc::clone(&controls.questions), Arc::clone(&self.bash_tasks));
        if controls.cancellation().is_cancelled() {
            return Ok(PreparedTurn::Cancelled);
        }
        let mut writer = lock_writer(&self.writer);
        let started_at =
            writer.append_operation_started(controls.turn_id.clone()).map_err(preparation_error)?;
        Ok(PreparedTurn::Ready(Box::new(ReadyTurn { agent, started_at, context_window })))
    }

    pub(crate) async fn inspect_context(
        self: &Arc<Self>,
        shutdown: &CancellationToken,
    ) -> Result<(), ConversationError> {
        let (cancel, mut cancel_guard) = {
            let mut inner = self.lock_inner();
            if shutdown.is_cancelled() || inner.state.turn.is_busy() || inner.view.context.is_some() {
                return Ok(());
            }
            if inner.state.closing {
                return Err(ConversationError::Busy);
            }
            let cancel = shutdown.child_token();
            let id = inner.state.register_inspect_cancel(cancel.clone());
            (cancel, InspectCancelGuard { conversation: self, id: Some(id) })
        };
        let conversation = Arc::clone(self);
        let result = async {
            let config = tokio::task::spawn_blocking(move || conversation.load_agent_config())
                .await
                .expect("context preparation completes while the runtime is running")
                .map_err(ConversationError::Message)?;
            Ok::<_, ConversationError>(
                agent::agent::modules::inspect(
                    std::path::Path::new(&self.cwd),
                    &config,
                    Arc::clone(&self.models),
                    &self.mcp,
                    &cancel,
                )
                .await?,
            )
        }
        .await;
        let mut inner = self.lock_inner();
        cancel_guard.remove_locked(&mut inner.state);
        if shutdown.is_cancelled() || inner.state.turn.is_busy() || inner.view.context.is_some() {
            return Ok(());
        }
        if inner.state.closing || cancel.is_cancelled() {
            return Err(ConversationError::Busy);
        }
        result.map(|context| self.publish_event(&mut inner, TurnEvent::ContextLoaded { context }))
    }

    fn load_agent_config(&self) -> Result<AgentConfig, String> {
        let instruction_home = self.home.clone();
        let initial_instructions =
            app_core::load_agent_instructions(std::path::Path::new(&self.cwd), &instruction_home)?;
        Ok(AgentConfig {
            instruction_home,
            initial_instructions,
            disabled_modules: self.disabled_modules.clone(),
        })
    }

    fn prepare_request_context(&self) -> Result<RequestContext, TurnErrorDetail> {
        let selection = self.model();
        let provider = ModelClient::new(&self.models, &selection).map_err(preparation_error)?;
        let config = self.load_agent_config().map_err(preparation_error)?;
        Ok(RequestContext::new(
            provider,
            Arc::clone(&self.models),
            config,
            Arc::clone(&self.writer),
            Arc::clone(&self.mcp),
        ))
    }
}

struct InspectCancelGuard<'a> {
    conversation: &'a Conversation,
    id: Option<u64>,
}

impl InspectCancelGuard<'_> {
    fn remove_locked(&mut self, state: &mut super::state::ConversationState) {
        if let Some(id) = self.id.take() {
            state.remove_inspect_cancel(id);
        }
    }
}

impl Drop for InspectCancelGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.conversation.lock_inner().state.remove_inspect_cancel(id);
        }
    }
}

struct ReadyTurn {
    agent: Agent,
    started_at: String,
    context_window: u64,
}

enum PreparedTurn {
    Ready(Box<ReadyTurn>),
    Cancelled,
}

fn finish_preparation_error(
    controls: &TurnControls,
    error: TurnErrorDetail,
) -> Result<TurnStatus, TurnErrorDetail> {
    if controls.finish_cancel() {
        Ok(TurnStatus::Interrupted)
    } else {
        Err(error)
    }
}

fn preparation_error(error: impl std::fmt::Display) -> TurnErrorDetail {
    TurnErrorDetail {
        message: error.to_string(),
        item_id: None,
    }
}
