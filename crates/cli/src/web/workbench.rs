//! 本地工作台的深模块：Workspace、Session、模型设置与运行态只有这一层组合。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use singularity_core::{CancellationToken, now_iso};
use singularity_model::ModelConfigOwner;
use singularity_protocol::{
    ActionReceipt, ActiveCompactionSnapshot, ActiveTurnSnapshot, CredentialConfigured, EmptyParams,
    EndpointSnapshot, ProviderConfigurationInput, RedactedModelCatalog, ResyncRequiredPayload,
    RpcError, RpcErrorCode, SessionPhase, SessionReadResult, SessionSettledPayload,
    SessionSnapshot, SessionTerminalSnapshot, StreamEnvelope, StreamEvent, ThreadSummary,
    TurnEvent, TurnStatus, WORKBENCH_PROTOCOL_VERSION, WorkbenchBootstrap, Workspace,
};
use singularity_runtime::{
    CatalogError, Conversation, ConversationControlError, ConversationError, FollowUpPromotion,
    ThreadCatalog, TurnReservation, TurnRunner, WorkspaceError, WorkspaceStore,
};
use tokio::sync::broadcast;
use uuid::Uuid;

const STREAM_CAPACITY: usize = 512;

pub struct Workbench {
    generation: String,
    authority: String,
    revision: Mutex<u64>,
    /// 完整工作台快照的构造与发布顺序；不覆盖会话执行或普通增量事件。
    workbench_publication: Mutex<()>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    models: Mutex<ModelConfigOwner>,
    sessions: Mutex<HashMap<String, Arc<ConversationSlot>>>,
    stream: broadcast::Sender<StreamEnvelope>,
}

struct ConversationSlot {
    conversation: Arc<Conversation>,
    state: Mutex<SlotState>,
}

struct SlotState {
    history: Option<Arc<singularity_runtime::ThreadSnapshot>>,
    session_revision: u64,
    active_turn: Option<ActiveTurnSnapshot>,
    active_compaction: Option<ActiveCompactionSnapshot>,
    terminal: Option<SessionTerminalSnapshot>,
}

impl Workbench {
    pub fn skills(
        &self,
        workspace_id: &str,
        session_id: Option<&str>,
    ) -> Result<singularity_protocol::SkillCatalog, RpcError> {
        let root = match session_id {
            Some(id) => self.session_directory(workspace_id, id)?,
            None => self.workspace(workspace_id)?.root,
        };
        let mut catalog = self.runner.skills(Path::new(&root));
        catalog.skills.retain(|skill| skill.user_invocable);
        Ok(singularity_protocol::SkillCatalog {
            skills: catalog
                .skills
                .into_iter()
                .map(|skill| singularity_protocol::SkillMetadata {
                    name: skill.name,
                    description: skill.description,
                    path: skill.path,
                    user_invocable: skill.user_invocable,
                    disable_model_invocation: skill.disable_model_invocation,
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }
    pub fn new(
        authority: String,
        runner: Arc<TurnRunner>,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: ModelConfigOwner,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            generation: Uuid::new_v4().to_string(),
            authority,
            revision: Mutex::new(0),
            workbench_publication: Mutex::new(()),
            runner,
            catalog,
            workspaces,
            models: Mutex::new(models),
            sessions: Mutex::new(HashMap::new()),
            stream,
        })
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    #[allow(clippy::expect_used)]
    pub fn revision(&self) -> u64 {
        *self.revision.lock().expect("stream revision lock poisoned")
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamEnvelope> {
        self.stream.subscribe()
    }

    pub fn ready_frame(&self) -> StreamEnvelope {
        StreamEnvelope {
            version: WORKBENCH_PROTOCOL_VERSION,
            generation: self.generation.clone(),
            revision: self.revision(),
            event: StreamEvent::Ready {
                payload: EmptyParams {},
            },
        }
    }

    pub fn bootstrap(&self) -> Result<WorkbenchBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        let mut threads = self.catalog.list_threads().map_err(catalog_error)?;
        let mut session_phases = std::collections::BTreeMap::new();
        let sessions: Vec<_> = self
            .lock_sessions()
            .iter()
            .map(|(id, slot)| (id.clone(), Arc::clone(slot)))
            .collect();
        for (id, slot) in sessions {
            let state = slot.lock_state();
            if let Some(history) = &state.history {
                threads.retain(|thread| thread.thread_id != id);
                threads.push(history.summary.clone());
            }
            session_phases.insert(id, slot.conversation.phase());
        }
        threads.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        let sessions_by_workspace =
            WorkspaceStore::group_threads(&workspaces, &threads).map_err(internal_error)?;
        Ok(WorkbenchBootstrap {
            session_phases,
            generation: self.generation.clone(),
            revision,
            endpoint: EndpointSnapshot {
                authority: self.authority.clone(),
            },
            workspaces,
            sessions_by_workspace,
            model_catalog: self.lock_models().redacted_catalog(),
        })
    }

    pub fn add_workspace(&self, root: &str) -> Result<Workspace, RpcError> {
        let workspace = self
            .workspaces
            .add(Path::new(root))
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(workspace)
    }

    pub fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<Workspace, RpcError> {
        let workspace = self
            .workspaces
            .rename(workspace_id, name)
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(workspace)
    }

    pub fn remove_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<singularity_protocol::WorkspaceRemoved, RpcError> {
        let workspace = self.workspace(workspace_id)?;
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let grouped = WorkspaceStore::group_threads(std::slice::from_ref(&workspace), &threads)
            .map_err(internal_error)?;
        for thread in grouped.get(workspace_id).into_iter().flatten() {
            let slot = self.lock_sessions().get(&thread.thread_id).cloned();
            let busy = match slot {
                Some(slot) => {
                    slot.conversation.phase() != SessionPhase::Idle
                        || !slot.conversation.pending_controls().is_empty()
                }
                None => self
                    .catalog
                    .read_snapshot(&thread.thread_id)
                    .map_err(catalog_error)?
                    .controls
                    .iter()
                    .any(singularity_protocol::ControlSnapshot::is_pending_input),
            };
            if busy {
                return Err(RpcError::new(
                    RpcErrorCode::WorkspaceBusy,
                    format!("项目 {} 仍有活动任务或待处理输入。", workspace.name),
                    "先停止运行并处理待处理输入队列。",
                ));
            }
        }
        self.workspaces
            .remove(workspace_id)
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(singularity_protocol::WorkspaceRemoved { removed: true })
    }

    pub fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
    ) -> Result<RedactedModelCatalog, RpcError> {
        self.update_models(|models| models.save_provider(provider))
    }

    pub fn set_api_key(
        &self,
        provider_id: &str,
        api_key: &str,
    ) -> Result<CredentialConfigured, RpcError> {
        self.update_models(|models| models.set_api_key(provider_id, api_key))
    }

    pub fn remove_provider(&self, provider_id: &str) -> Result<RedactedModelCatalog, RpcError> {
        self.update_models(|models| models.remove_provider(provider_id))
    }

    fn update_models<T>(
        &self,
        update: impl FnOnce(&mut ModelConfigOwner) -> Result<T, singularity_model::ProviderError>,
    ) -> Result<T, RpcError> {
        let mut models = self.lock_models();
        let result = update(&mut models).map_err(model_error);
        // Configuration and credentials are separate files. A failed second write
        // must not leave future turns using a snapshot of the old configuration.
        self.runner.refresh_provider_snapshot(models.snapshot());
        drop(models);
        self.publish_workbench_snapshot();
        result
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, RpcError> {
        let request = self
            .lock_models()
            .model_discovery_request(provider_id, base_url, api_key)
            .map_err(model_discovery_error)?;
        ModelConfigOwner::discover_models(request, base_url)
            .await
            .map_err(model_discovery_error)
    }

    pub fn create_session(
        &self,
        workspace_id: &str,
        selector: Option<String>,
    ) -> Result<SessionReadResult, RpcError> {
        let workspace = self.workspace(workspace_id)?;
        let selector = selector.or_else(|| self.runner.default_model_selector());
        if selector.is_some() {
            self.runner
                .validate_model_selector(selector.as_deref())
                .map_err(configuration_error)?;
        }
        let thread = self
            .catalog
            .create_thread(&workspace.root, selector)
            .map_err(catalog_error)?;
        let slot = self.insert_slot(thread)?;
        let result = self.read_from_slot(&slot, 100, None)?;
        self.publish_workbench_snapshot();
        Ok(result)
    }

    pub fn read_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid_request("limit must be between 1 and 100"));
        }
        let slot = self.open_slot(workspace_id, session_id)?;
        self.read_from_slot(&slot, limit, before_turn)
    }

    pub fn request_details(
        &self,
        workspace_id: &str,
        session_id: &str,
        request_id: &str,
    ) -> Result<singularity_protocol::ModelRequestSnapshot, RpcError> {
        self.open_slot(workspace_id, session_id)?;
        self.catalog
            .read_snapshot(session_id)
            .map_err(catalog_error)?
            .request_details(request_id)
            .map_err(catalog_error)
    }

    pub fn submit(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<ActionReceipt, RpcError> {
        if text.trim().is_empty() {
            return Err(invalid_request("任务内容不能为空。").preserve(text));
        }
        let slot = self.open_slot(workspace_id, session_id)?;
        let selector = slot.conversation.thread().model;
        self.runner
            .validate_model_selector(selector.as_deref())
            .map_err(|message| configuration_error(message).preserve(text.clone()))?;
        let reservation = slot
            .conversation
            .reserve_start()
            .map_err(|error| conversation_error(error).preserve(text.clone()))?;
        self.begin_turn(&slot, &text)?;
        let revision = self.emit_session_changed(session_id, &slot);
        let receipt = ActionReceipt {
            request_id: request_id.to_string(),
            accepted: true,
            generation: self.generation.clone(),
            revision,
            session_id: Some(session_id.to_string()),
            turn_id: None,
            control: None,
        };
        self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
            turn_terminal(reservation.run(&text, sink))
        });
        Ok(receipt)
    }

    pub fn steer(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let preserved = text.clone();
        self.apply_control(
            request_id,
            session_id,
            &slot,
            preserved,
            move |conversation| conversation.steer(text).map(Some),
        )
    }

    pub fn follow_up(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let preserved = text.clone();
        self.apply_control(
            request_id,
            session_id,
            &slot,
            preserved,
            move |conversation| conversation.submit_follow_up(text).map(Some),
        )
    }

    pub fn queue_withdraw(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(
            request_id,
            session_id,
            &slot,
            String::new(),
            |conversation| conversation.withdraw_follow_up(control_id).map(Some),
        )
    }

    pub fn queue_replace(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
        text: String,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let preserved = text.clone();
        self.apply_control(
            request_id,
            session_id,
            &slot,
            preserved,
            move |conversation| conversation.replace_follow_up(control_id, text).map(Some),
        )
    }

    pub fn queue_send_now(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let pending = slot
            .conversation
            .pending_controls()
            .into_iter()
            .find(|control| control.control_id == control_id)
            .ok_or_else(|| control_not_found(String::new()))?;
        let text = pending.text.unwrap_or_default();
        self.runner
            .validate_model_selector(slot.conversation.thread().model.as_deref())
            .map_err(|message| configuration_error(message).preserve(text.clone()))?;
        // 与 worker 的事件及结算共用 SlotState 顺序：控制从 Conversation
        // 转移到公开投影并发布之前，结算不能插入并被旧回执覆盖。
        let mut state = slot.lock_state();
        match slot
            .conversation
            .promote_follow_up(control_id)
            .map_err(|error| control_error(error, text.clone()))?
        {
            FollowUpPromotion::Injected(control) => Ok(self.complete_control_locked(
                request_id,
                session_id,
                &slot,
                &mut state,
                Some(control),
            )),
            FollowUpPromotion::Reserved {
                control,
                reservation,
            } => {
                self.begin_turn_locked(&slot, &mut state, &text)?;
                let revision = self.emit_session_snapshot(session_id, &slot, &state);
                drop(state);
                let result_control = control;
                self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
                    turn_terminal(reservation.run_promoted(sink))
                });
                Ok(receipt(
                    self,
                    request_id,
                    revision,
                    session_id,
                    Some(result_control),
                ))
            }
        }
    }

    pub fn abort(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(
            request_id,
            session_id,
            &slot,
            String::new(),
            Conversation::abort,
        )
    }

    /// 会话控制的接受、公开投影与发布共用 SlotState 顺序。闭包只执行
    /// Conversation 的短控制操作，不得覆盖 Agent 执行或调用事件 sink。
    fn apply_control(
        &self,
        request_id: &str,
        session_id: &str,
        slot: &ConversationSlot,
        preserved_input: String,
        apply: impl FnOnce(
            &Conversation,
        ) -> Result<
            Option<singularity_protocol::ControlSnapshot>,
            ConversationControlError,
        >,
    ) -> Result<ActionReceipt, RpcError> {
        let mut state = slot.lock_state();
        let control =
            apply(&slot.conversation).map_err(|error| control_error(error, preserved_input))?;
        Ok(self.complete_control_locked(request_id, session_id, slot, &mut state, control))
    }

    fn complete_control_locked(
        &self,
        request_id: &str,
        session_id: &str,
        slot: &ConversationSlot,
        state: &mut SlotState,
        control: Option<singularity_protocol::ControlSnapshot>,
    ) -> ActionReceipt {
        state.session_revision = state.session_revision.saturating_add(1);
        let revision = self.emit_session_snapshot(session_id, slot, state);
        receipt(self, request_id, revision, session_id, control)
    }

    pub fn compact(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<ActionReceipt, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let reservation = slot
            .conversation
            .reserve_compaction(CancellationToken::new())
            .map_err(conversation_error)?;
        self.begin_turn(&slot, "")?;
        slot.lock_state().active_compaction = Some(ActiveCompactionSnapshot {
            started_at: now_iso(),
        });
        let revision = self.emit_session_changed(session_id, &slot);
        self.spawn_operation(session_id, slot, reservation, move |reservation, _| {
            reservation
                .compact()
                .err()
                .map(|error| SessionTerminalSnapshot {
                    status: if matches!(
                        error,
                        ConversationError::Compaction(
                            singularity_runtime::CompactionRunError::Interrupted(_)
                        )
                    ) {
                        TurnStatus::Interrupted
                    } else {
                        TurnStatus::Failed
                    },
                    message: Some(error.to_string()),
                })
        });
        Ok(receipt(self, request_id, revision, session_id, None))
    }

    pub fn rename_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<ThreadSummary, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation.phase() != SessionPhase::Idle {
            return Err(session_busy(name.to_string()));
        }
        self.catalog
            .rename(session_id, name)
            .map_err(catalog_error)?;
        let summary = self
            .catalog
            .read_thread_summary(session_id)
            .map_err(catalog_error)?;
        self.publish_workbench_snapshot();
        Ok(summary)
    }

    pub fn archive_session(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<singularity_protocol::SessionArchived, RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation.phase() != SessionPhase::Idle
            || !slot.conversation.pending_controls().is_empty()
        {
            return Err(session_busy(String::new()));
        }
        self.catalog.archive(session_id).map_err(catalog_error)?;
        self.lock_sessions().remove(session_id);
        self.publish_workbench_snapshot();
        Ok(singularity_protocol::SessionArchived { archived: true })
    }

    pub fn update_settings(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        selector: &str,
    ) -> Result<ActionReceipt, RpcError> {
        self.runner
            .validate_model_selector(Some(selector))
            .map_err(configuration_error)?;
        let slot = self.open_slot(workspace_id, session_id)?;
        slot.conversation
            .update_settings(selector)
            .map_err(conversation_error)?;
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(receipt(self, request_id, revision, session_id, None))
    }

    fn open_slot(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Arc<ConversationSlot>, RpcError> {
        if let Some(slot) = self.lock_sessions().get(session_id).cloned() {
            self.verify_session_scope(workspace_id, &slot.conversation.thread().cwd)?;
            return Ok(slot);
        }
        let thread = self
            .catalog
            .resume_thread(session_id)
            .map_err(catalog_error)?;
        self.verify_session_scope(workspace_id, &thread.cwd)?;
        self.insert_slot(thread)
    }

    fn verify_session_scope(&self, workspace_id: &str, cwd: &str) -> Result<(), RpcError> {
        verify_workspace_thread(&self.workspace(workspace_id)?, cwd)
    }

    pub fn session_directory(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<String, RpcError> {
        Ok(self
            .open_slot(workspace_id, session_id)?
            .conversation
            .thread()
            .cwd)
    }

    fn insert_slot(
        &self,
        thread: singularity_protocol::Thread,
    ) -> Result<Arc<ConversationSlot>, RpcError> {
        let session_id = thread.thread_id.clone();
        let conversation =
            Conversation::new(Arc::clone(&self.runner), thread).map_err(conversation_error)?;
        let slot = Arc::new(ConversationSlot {
            conversation,
            state: Mutex::new(SlotState {
                history: None,
                session_revision: 0,
                active_turn: None,
                active_compaction: None,
                terminal: None,
            }),
        });
        Ok(self
            .lock_sessions()
            .entry(session_id)
            .or_insert_with(|| Arc::clone(&slot))
            .clone())
    }

    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        let mut state = slot.lock_state();
        let snapshot = if let Some(history) = &state.history {
            Arc::clone(history)
        } else {
            self.refresh_history(slot, &mut state)
                .map_err(catalog_error)?
        };
        let history = snapshot.page(limit, before_turn).map_err(catalog_error)?;
        Ok(SessionReadResult {
            summary: history.summary.clone(),
            history,
            runtime: slot.snapshot_from(&state),
        })
    }

    // Freeze the latest durable history before any events of this chain arrive.
    // Both start paths wait for the previous worker's complete Workbench settlement.
    fn begin_turn(&self, slot: &ConversationSlot, input: &str) -> Result<(), RpcError> {
        let mut state = slot.lock_state();
        self.begin_turn_locked(slot, &mut state, input)
    }

    fn begin_turn_locked(
        &self,
        slot: &ConversationSlot,
        state: &mut SlotState,
        input: &str,
    ) -> Result<(), RpcError> {
        state.history = Some(
            self.refresh_history(slot, state)
                .map_err(|error| catalog_error(error).preserve(input))?,
        );
        state.terminal = None;
        state.session_revision = state.session_revision.saturating_add(1);
        Ok(())
    }

    fn refresh_history(
        &self,
        slot: &ConversationSlot,
        state: &mut SlotState,
    ) -> Result<Arc<singularity_runtime::ThreadSnapshot>, CatalogError> {
        let snapshot = self
            .catalog
            .read_snapshot(&slot.conversation.thread().thread_id)?;
        state.active_turn = None;
        Ok(snapshot)
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, RpcError> {
        self.workspaces.find(workspace_id).ok_or_else(|| {
            RpcError::new(
                RpcErrorCode::WorkspaceNotFound,
                "项目不存在或已移除。",
                "刷新工作台并重新选择项目。",
            )
        })
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        // 控制处置变化归约为会话快照发布：控制事实只由会话快照一种表示
        // 承载，不进入活动 turn 的事件序列。
        if let TurnEvent::ControlChanged { .. } = &event {
            self.on_control_changed(session_id, slot);
            return;
        }
        let started_at = now_iso();
        let mut state = slot.lock_state();
        state.session_revision += 1;
        if let TurnEvent::TurnStarted { turn, .. } = &event {
            let active = state.active_turn.get_or_insert_with(|| ActiveTurnSnapshot {
                turn_id: turn.turn_id.clone(),
                events: Vec::new(),
                started_at: started_at.clone(),
            });
            active.turn_id = turn.turn_id.clone();
            active.started_at = started_at.clone();
        }
        let envelope = singularity_protocol::WorkbenchTurnEvent {
            event,
            session_revision: state.session_revision,
            started_at,
        };
        if let Some(active) = state.active_turn.as_mut() {
            // Updates are replaceable progress, not history. Keep at most one
            // output snapshot per running tool; the terminal carries full output.
            if let TurnEvent::ToolExecutionUpdate {
                turn_id,
                tool_call_id,
                ..
            }
            | TurnEvent::ToolExecutionEnd {
                turn_id,
                tool_call_id,
                ..
            } = &envelope.event
                && let Some(index) = active.events.iter().rposition(|previous| {
                    matches!(&previous.event, TurnEvent::ToolExecutionUpdate {
                        turn_id: previous_turn, tool_call_id: previous_call, ..
                    } if previous_turn == turn_id && previous_call == tool_call_id)
                })
            {
                active.events.remove(index);
            }
            active.events.push(envelope.clone());
        }
        self.emit(StreamEvent::TurnEvent {
            session_id: session_id.to_string(),
            payload: envelope,
        });
    }

    fn on_control_changed(&self, session_id: &str, slot: &ConversationSlot) {
        let mut state = slot.lock_state();
        state.session_revision = state.session_revision.saturating_add(1);
        self.emit_session_snapshot(session_id, slot, &state);
    }

    fn on_session_settled(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        terminal: Option<SessionTerminalSnapshot>,
        reservation: TurnReservation,
    ) {
        let mut state = slot.lock_state();
        state.active_compaction = None;
        // 终态来自执行链的可信提交：历史读取失败不改变它，读取错误由
        // 现有会话读取路径独立呈现（history 置空强制下一次读取重试）。
        state.terminal = terminal;
        state.active_turn = None;
        state.history = None;
        state.session_revision += 1;
        // 完整历史已接入后释放唯一操作预订；新操作的开始投影等待此锁。
        drop(reservation);
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: SessionSettledPayload {
                runtime: slot.snapshot_from(&state),
            },
        });
    }

    fn spawn_operation(
        self: &Arc<Self>,
        session_id: &str,
        slot: Arc<ConversationSlot>,
        mut reservation: TurnReservation,
        run: impl FnOnce(
            &mut TurnReservation,
            &mut dyn FnMut(TurnEvent),
        ) -> Option<SessionTerminalSnapshot>
        + Send
        + 'static,
    ) {
        let workbench = Arc::clone(self);
        let session_id = session_id.to_string();
        std::thread::spawn(move || {
            let terminal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let event_workbench = Arc::clone(&workbench);
                let event_slot = Arc::clone(&slot);
                let event_session_id = session_id.clone();
                let mut event_sink = move |event| {
                    event_workbench.on_turn_event(&event_session_id, &event_slot, event)
                };
                run(&mut reservation, &mut event_sink)
            }))
            .unwrap_or_else(|_| {
                Some(SessionTerminalSnapshot {
                    status: TurnStatus::Failed,
                    message: Some("任务执行异常，已停止。可重新提交或恢复会话。".into()),
                })
            });
            workbench.on_session_settled(&session_id, &slot, terminal, reservation);
        });
    }

    fn bump_and_emit_session(&self, session_id: &str, slot: &ConversationSlot) -> u64 {
        let mut state = slot.lock_state();
        state.session_revision = state.session_revision.saturating_add(1);
        self.emit_session_snapshot(session_id, slot, &state)
    }

    #[allow(clippy::expect_used)]
    fn emit_session_changed(&self, session_id: &str, slot: &ConversationSlot) -> u64 {
        let state = slot.lock_state();
        self.emit_session_snapshot(session_id, slot, &state)
    }

    fn emit_session_snapshot(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        state: &SlotState,
    ) -> u64 {
        self.emit(StreamEvent::SessionChanged {
            session_id: session_id.to_string(),
            payload: slot.snapshot_from(state),
        })
    }

    /// 发布完整工作台快照。快照构造失败不推翻任何已提交的操作结果：
    /// 读侧无法展示时经重同步通道要求客户端重新拉取基线。
    fn publish_workbench_snapshot(&self) {
        if let Err(error) = self.emit_workbench_changed_with(|| self.bootstrap()) {
            self.emit(StreamEvent::ResyncRequired {
                payload: ResyncRequiredPayload {
                    reason: format!("snapshot_unavailable: {error:?}"),
                },
            });
        }
    }

    /// 完整替换快照必须在同一发布临界区内构造并取得流序号；否则较早构造的
    /// payload 可以在较新快照之后获得更高 revision。该锁不参与会话事件发布，
    /// 避免形成全局发布锁 → SlotState 的反向锁序。
    #[allow(clippy::expect_used)]
    fn emit_workbench_changed_with(
        &self,
        snapshot: impl FnOnce() -> Result<WorkbenchBootstrap, RpcError>,
    ) -> Result<u64, RpcError> {
        let _publication = self
            .workbench_publication
            .lock()
            .expect("workbench publication lock poisoned");
        Ok(self.emit(StreamEvent::WorkbenchChanged {
            payload: snapshot()?,
        }))
    }

    #[allow(clippy::expect_used)]
    fn emit(&self, event: StreamEvent) -> u64 {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let revision = *order;
        let _ = self.stream.send(StreamEnvelope {
            version: WORKBENCH_PROTOCOL_VERSION,
            generation: self.generation.clone(),
            revision,
            event,
        });
        revision
    }

    #[allow(clippy::expect_used)]
    fn lock_models(&self) -> std::sync::MutexGuard<'_, ModelConfigOwner> {
        self.models
            .lock()
            .expect("model configuration lock poisoned")
    }

    #[allow(clippy::expect_used)]
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ConversationSlot>>> {
        self.sessions
            .lock()
            .expect("workbench session map lock poisoned (fail-stop)")
    }
}

#[allow(clippy::expect_used)]
impl ConversationSlot {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .expect("conversation slot lock poisoned (fail-stop)")
    }

    #[cfg(test)]
    fn snapshot(&self) -> SessionSnapshot {
        let state = self.lock_state();
        self.snapshot_from(&state)
    }

    fn snapshot_from(&self, state: &SlotState) -> SessionSnapshot {
        SessionSnapshot {
            session_revision: state.session_revision,
            phase: self.conversation.phase(),
            selector: self.conversation.thread().model,
            model_context_window: self.conversation.model_context_window(),
            controls: self.conversation.controls(),
            pending_controls: self.conversation.pending_controls(),
            active_turn: state.active_turn.clone(),
            active_compaction: state.active_compaction.clone(),
            terminal: state.terminal.clone(),
        }
    }
}

fn turn_terminal(
    result: Result<singularity_runtime::TurnOutcome, ConversationError>,
) -> Option<SessionTerminalSnapshot> {
    Some(match result {
        Ok(outcome) => SessionTerminalSnapshot {
            status: outcome.turn_status,
            message: outcome.error.map(|error| error.message),
        },
        Err(error) => SessionTerminalSnapshot {
            status: TurnStatus::Failed,
            message: Some(error.to_string()),
        },
    })
}

fn receipt(
    workbench: &Workbench,
    request_id: &str,
    revision: u64,
    session_id: &str,
    control: Option<singularity_protocol::ControlSnapshot>,
) -> ActionReceipt {
    ActionReceipt {
        request_id: request_id.to_string(),
        accepted: true,
        generation: workbench.generation.clone(),
        revision,
        session_id: Some(session_id.to_string()),
        turn_id: control.as_ref().map(|control| control.turn_id.clone()),
        control,
    }
}

fn verify_workspace_thread(workspace: &Workspace, cwd: &str) -> Result<(), RpcError> {
    let workspace = singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
        .map_err(internal_error)?;
    let thread =
        singularity_core::CanonicalWorkspacePath::from_saved(cwd).map_err(internal_error)?;
    if workspace.matches(&thread) {
        Ok(())
    } else {
        Err(RpcError::new(
            RpcErrorCode::Conflict,
            "Session 不属于所选 Workspace。",
            "刷新工作台并从所属 Workspace 打开该 Session。",
        ))
    }
}

pub(super) fn invalid_request(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidRequest, message, "检查输入后重试。")
}

fn internal_error(message: impl Into<String>) -> RpcError {
    RpcError::new(
        RpcErrorCode::Internal,
        message,
        "刷新工作台；若问题持续，检查启动终端中的错误。",
    )
}

fn configuration_error(message: impl Into<String>) -> RpcError {
    RpcError::new(
        RpcErrorCode::ConfigurationInvalid,
        message,
        "打开模型设置并修正配置。",
    )
}

fn model_error(error: singularity_model::ProviderError) -> RpcError {
    configuration_error(error.to_string())
}

fn model_discovery_error(error: singularity_model::ProviderError) -> RpcError {
    use singularity_model::ModelErrorCategory;
    match error.category() {
        ModelErrorCategory::ModelConfiguration | ModelErrorCategory::InvalidRequest => {
            configuration_error(error.to_string())
        }
        ModelErrorCategory::Authentication => RpcError::new(
            RpcErrorCode::ConfigurationInvalid,
            error.to_string(),
            "检查 API 地址和密钥；也可以手动添加模型。",
        ),
        ModelErrorCategory::Network
        | ModelErrorCategory::ProviderUnavailable
        | ModelErrorCategory::UnknownProviderError
        | ModelErrorCategory::JsonSchema => RpcError::new(
            RpcErrorCode::ProviderUnavailable,
            error.to_string(),
            "稍后重试；也可以手动添加模型。",
        ),
        ModelErrorCategory::Cancelled
        | ModelErrorCategory::ContextLengthExceeded
        | ModelErrorCategory::ContentFilter => internal_error(error.to_string()),
    }
}

fn conversation_error(error: ConversationError) -> RpcError {
    match error {
        ConversationError::TurnAlreadyActive => session_busy(String::new()),
        ConversationError::Configuration(message) => configuration_error(message),
        ConversationError::Compaction(error) => internal_error(error.to_string()),
        ConversationError::Turn(error) => internal_error(error.to_string()),
        ConversationError::Session(error) => internal_error(error.to_string()),
    }
}

fn catalog_error(error: CatalogError) -> RpcError {
    match error {
        CatalogError::NotFound(_) => RpcError::new(
            RpcErrorCode::SessionNotFound,
            "任务不存在或已归档。",
            "刷新项目的任务列表。",
        ),
        CatalogError::WriterActive => session_busy(String::new()),
        CatalogError::InvalidName => invalid_request(error.to_string()),
        CatalogError::AnchorNotFound(_) => invalid_request("历史分页位置已失效，请重新加载任务。"),
        other => internal_error(other.to_string()),
    }
}

fn workspace_error(error: WorkspaceError) -> RpcError {
    match error {
        WorkspaceError::InvalidInput(message) => invalid_request(message),
        WorkspaceError::NotFound => RpcError::new(
            RpcErrorCode::WorkspaceNotFound,
            "项目不存在或已移除。",
            "刷新工作台并重新选择项目。",
        ),
        other => internal_error(other.to_string()),
    }
}

fn session_busy(input: String) -> RpcError {
    let error = RpcError::new(
        RpcErrorCode::SessionBusy,
        "当前任务正在处理另一项操作。",
        "等待状态变为空闲，或使用当前阶段提供的控制动作。",
    );
    if input.is_empty() {
        error
    } else {
        error.preserve(input)
    }
}

fn control_not_found(input: String) -> RpcError {
    let error = RpcError::new(
        RpcErrorCode::ControlNotFound,
        "待处理输入已不存在或已经开始执行。",
        "刷新任务后确认待处理输入队列。",
    );
    if input.is_empty() {
        error
    } else {
        error.preserve(input)
    }
}

fn control_error(error: ConversationControlError, input: String) -> RpcError {
    match error {
        ConversationControlError::NotRunning => session_busy(input),
        ConversationControlError::InvalidInput => invalid_request("输入不能为空。").preserve(input),
        ConversationControlError::ControlNotFound => control_not_found(input),
        ConversationControlError::Storage(message) => internal_error(message).preserve(input),
    }
}

#[cfg(test)]
mod tests;
