use std::sync::Arc;

use agent::agent::{Agent, AgentConfig, AgentError, AgentEvent, RequestContext};
use agent::session::{LedgerRecord, lock_writer, with_writer_async};
use model::ModelClient;

use protocol::{Turn, TurnErrorDetail, TurnEvent, TurnStatus};
use tokio_util::sync::CancellationToken;

use super::{Conversation, ConversationError};

impl Conversation {
    pub(super) async fn run_single_turn(
        self: &Arc<Self>,
        controls: &Arc<crate::runtime::conversation::TurnControls>,
        prepared_context: Option<Box<RequestContext>>,
        sink: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let preparing = Arc::clone(controls);
        let prepared =
            tokio::task::spawn_blocking(move || conversation.prepare_turn(&preparing, prepared_context))
                .await
                .expect("turn preparation completes while the runtime is running");
        let ReadyTurn { mut agent, started_at, context_window } = match prepared {
            Ok(PreparedTurn::Ready(ready)) => *ready,
            Ok(PreparedTurn::Cancelled) => {
                controls.finish_cancel();
                return Ok(TurnStatus::Interrupted);
            }
            Err(error) => {
                return if controls.finish_cancel() {
                    Ok(TurnStatus::Interrupted)
                } else {
                    Err(error)
                };
            }
        };
        self.lock_state().last_context_window = Some(context_window);
        let turn_id = controls.turn_id.clone();
        let turn = Turn {
            turn_id: turn_id.clone(),
            status: TurnStatus::Running,
        };
        sink(AgentEvent::Turn(TurnEvent::TurnStarted { turn, started_at }));

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
                turn: Turn {
                    turn_id: turn_id.clone(),
                    status: turn_status,
                },
                manually_stopped,
                finished_at,
                stats: None,
            }));
        }
        Ok(turn_status)
    }

    fn prepare_turn(
        &self,
        controls: &crate::runtime::conversation::TurnControls,
        prepared_context: Option<Box<RequestContext>>,
    ) -> Result<PreparedTurn, TurnErrorDetail> {
        if controls.cancellation().is_cancelled() {
            return Ok(PreparedTurn::Cancelled);
        }
        let inbox = Arc::clone(&self.lock_state().steering_inbox);
        let context = match prepared_context {
            Some(context) => *context,
            None => self.prepare_request_context()?,
        };
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

    pub(crate) fn begin_context_preview(self: &Arc<Self>, signal: &CancellationToken) -> ContextPreview {
        let view = self.lock_view();
        let mut state = self.lock_state();
        if signal.is_cancelled() || view.context.is_some() || state.closing || state.turn.is_busy() {
            return ContextPreview::Skip;
        }
        if state.context_preview.is_some() {
            return ContextPreview::Busy;
        }
        state.context_preview = Some(signal.clone());
        ContextPreview::Run(PreviewGuard { conversation: Arc::clone(self) })
    }

    pub(crate) async fn wait_context_preview(&self, signal: &CancellationToken) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if signal.is_cancelled() || self.lock_state().context_preview.is_none() {
                return;
            }
            tokio::select! {
                biased;
                () = signal.cancelled() => return,
                () = changed => {}
            }
        }
    }

    pub(crate) async fn inspect_context(
        self: &Arc<Self>,
        signal: CancellationToken,
        guard: PreviewGuard,
    ) -> Result<(), ConversationError> {
        let conversation = Arc::clone(self);
        let result = async {
            let requests = tokio::task::spawn_blocking(move || conversation.prepare_request_context())
                .await
                .expect("context preparation completes while the runtime is running")
                .map_err(|error| ConversationError::Message(error.message))?;
            Ok::<_, ConversationError>(requests.inspect_context(&signal).await?)
        }
        .await;
        let result = {
            let mut view = self.lock_view();
            if signal.is_cancelled() {
                Ok(())
            } else {
                result.map(|context| self.publish_event(&mut view, TurnEvent::ContextLoaded { context }))
            }
        };
        drop(guard);
        result
    }

    pub(crate) fn prepare_resubmit_context(
        &self,
    ) -> Result<(protocol::ModelSelection, agent::agent::RequestContext), ConversationError> {
        let selection = self.model();
        let context = self
            .prepare_request_context_for(&selection)
            .map_err(|error| ConversationError::Message(error.message))?;
        Ok((selection, context))
    }

    pub(super) fn prepare_request_context(&self) -> Result<RequestContext, TurnErrorDetail> {
        self.prepare_request_context_for(&self.model())
    }

    fn prepare_request_context_for(
        &self,
        selection: &protocol::ModelSelection,
    ) -> Result<RequestContext, TurnErrorDetail> {
        let provider = ModelClient::new(&self.models, selection).map_err(preparation_error)?;
        let instruction_home = self.home.clone();
        let initial_instructions =
            app_core::load_agent_instructions(std::path::Path::new(&self.cwd), &instruction_home)
                .map_err(preparation_error)?;
        let config = AgentConfig {
            instruction_home,
            initial_instructions,
            disabled_modules: self.disabled_modules.clone(),
        };
        Ok(RequestContext::new(
            provider,
            Arc::clone(&self.models),
            config,
            Arc::clone(&self.writer),
            Arc::clone(&self.mcp),
        ))
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

pub(crate) enum ContextPreview {
    Skip,
    Busy,
    Run(PreviewGuard),
}

pub(crate) struct PreviewGuard {
    conversation: Arc<Conversation>,
}

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        self.conversation.lock_state().context_preview = None;
        self.conversation.changed.notify_waiters();
    }
}

fn preparation_error(error: impl std::fmt::Display) -> TurnErrorDetail {
    TurnErrorDetail {
        message: error.to_string(),
        item_id: None,
    }
}
