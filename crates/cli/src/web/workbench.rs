//! 本地工作台的深模块：Workspace、Session、模型设置与运行态只有这一层组合。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::{ModelConfigOwner, split_model_selector};
use singularity_protocol::{
    ActionReceipt, ActiveCompactionSnapshot, ActiveTurnSnapshot, CommandDescriptor,
    CredentialConfigured, EndpointSnapshot, ExecutionSnapshot, FileAccess,
    ProviderConfigurationInput, RedactedModelCatalog, RpcErrorCode, SessionPhase,
    SessionReadResult, SessionSnapshot, SessionTerminalSnapshot, SettingsApplyTiming,
    StreamEnvelope, StreamType, ThreadSummary, TurnEvent, TurnStatus, WORKBENCH_PROTOCOL_VERSION,
    WorkbenchBootstrap, Workspace,
};
use singularity_runtime::{
    Conversation, ConversationControlError, ConversationError, FollowUpPromotion, ReasoningPatch,
    ResumeError, SettingsApplyTiming as RuntimeSettingsTiming, SettingsPatch, ThreadCatalog,
    TurnRunner, WorkspaceStore,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;
use uuid::Uuid;

const STREAM_CAPACITY: usize = 512;

#[derive(Debug, Clone)]
pub struct WorkbenchError {
    pub code: RpcErrorCode,
    pub message: String,
    pub recovery: String,
    pub preserved_input: Option<String>,
}

impl WorkbenchError {
    pub fn new(
        code: RpcErrorCode,
        message: impl Into<String>,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
            preserved_input: None,
        }
    }

    pub fn preserve(mut self, input: impl Into<String>) -> Self {
        self.preserved_input = Some(input.into());
        self
    }
}

pub struct Workbench {
    generation: String,
    authority: String,
    revision: Mutex<u64>,
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
    history: singularity_protocol::ThreadReadPage,
    session_revision: u64,
    phase: SessionPhase,
    controls: Vec<singularity_protocol::ControlSnapshot>,
    active_turn: Option<ActiveTurnSnapshot>,
    active_compaction: Option<ActiveCompactionSnapshot>,
    terminal: Option<SessionTerminalSnapshot>,
    compaction_cancellation: Option<CancellationToken>,
}

impl Workbench {
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
            event_type: StreamType::Ready,
            session_id: None,
            payload: json!({}),
        }
    }

    pub fn bootstrap(&self) -> Result<WorkbenchBootstrap, WorkbenchError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        let mut threads = self.catalog.list_threads().map_err(internal_error)?;
        let mut session_phases = std::collections::BTreeMap::new();
        for (id, slot) in self.lock_sessions().iter() {
            let state = slot.lock_state();
            if state.phase != SessionPhase::Idle {
                threads.retain(|thread| &thread.thread_id != id);
                threads.push(state.history.summary.clone());
            }
            session_phases.insert(id.clone(), state.phase);
        }
        threads.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        let sessions_by_workspace = self
            .workspaces
            .group_threads(&threads)
            .map_err(internal_error)?;
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
            execution: ExecutionSnapshot {
                file_access: FileAccess::FullLocalAccess,
            },
            commands: commands(),
        })
    }

    pub fn add_workspace(&self, root: &str) -> Result<Workspace, WorkbenchError> {
        let workspace = self.workspaces.add(Path::new(root)).map_err(|message| {
            WorkbenchError::new(
                RpcErrorCode::InvalidRequest,
                message,
                "选择一个存在且尚未登记的目录。",
            )
        })?;
        self.emit_workbench_changed()?;
        Ok(workspace)
    }

    pub fn remove_workspace(&self, workspace_id: &str) -> Result<Value, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let threads = self.catalog.list_threads().map_err(internal_error)?;
        let grouped = self
            .workspaces
            .group_threads(&threads)
            .map_err(internal_error)?;
        for thread in grouped.get(workspace_id).into_iter().flatten() {
            if let Some(slot) = self.lock_sessions().get(&thread.thread_id).cloned() {
                let state = slot.lock_state();
                if state.phase != SessionPhase::Idle
                    || !slot.conversation.pending_controls().is_empty()
                {
                    return Err(WorkbenchError::new(
                        RpcErrorCode::WorkspaceBusy,
                        format!("Workspace {} 仍有活动或待处理会话。", workspace.name),
                        "先停止运行并处理 Follow-up 队列。",
                    ));
                }
            }
        }
        self.workspaces.remove(workspace_id).map_err(|message| {
            WorkbenchError::new(
                RpcErrorCode::WorkspaceNotFound,
                message,
                "刷新工作台后重试。",
            )
        })?;
        self.emit_workbench_changed()?;
        Ok(json!({"removed": true}))
    }

    pub fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
    ) -> Result<RedactedModelCatalog, WorkbenchError> {
        let mut models = self.lock_models();
        let catalog = models.save_provider(provider).map_err(model_error)?;
        self.runner.refresh_provider_snapshot(models.snapshot());
        drop(models);
        self.emit_workbench_changed()?;
        Ok(catalog)
    }

    pub fn set_api_key(
        &self,
        provider_id: &str,
        api_key: &str,
    ) -> Result<CredentialConfigured, WorkbenchError> {
        let mut models = self.lock_models();
        let configured = models
            .set_api_key(provider_id, api_key)
            .map_err(model_error)?;
        self.runner.refresh_provider_snapshot(models.snapshot());
        drop(models);
        self.emit_workbench_changed()?;
        Ok(configured)
    }

    pub fn create_session(
        &self,
        workspace_id: &str,
        selector: Option<String>,
    ) -> Result<SessionReadResult, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let selector = selector.or_else(|| self.runner.default_model_selector());
        if selector.is_some() {
            self.runner
                .validate_model_selector(selector.as_deref())
                .map_err(configuration_error)?;
        }
        let thread = self
            .catalog
            .create_thread(&workspace.root, selector)
            .map_err(internal_error)?;
        let slot = self.insert_slot(thread)?;
        let result = self.read_from_slot(&slot, 100, None)?;
        self.emit_workbench_changed()?;
        Ok(result)
    }

    pub fn read_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, WorkbenchError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid_request("limit must be between 1 and 100"));
        }
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        self.read_from_slot(&slot, limit, before_turn)
    }

    pub fn submit(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<ActionReceipt, WorkbenchError> {
        if text.trim().is_empty() {
            return Err(invalid_request("任务内容不能为空。").preserve(text));
        }
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let selector = slot.conversation.thread().model;
        self.runner
            .validate_model_selector(selector.as_deref())
            .map_err(|message| configuration_error(message).preserve(text.clone()))?;
        let reservation = slot.conversation.reserve_start().map_err(|_| {
            WorkbenchError::new(
                RpcErrorCode::SessionBusy,
                "当前 Session 已有活动任务。",
                "使用 Steer 或 Follow-up，或等待当前任务结束。",
            )
            .preserve(text.clone())
        })?;
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
        self.spawn_operation(session_id, slot, move |sink| {
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
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let control = slot
            .conversation
            .steer(text.clone())
            .map_err(|error| control_error(error, text))?;
        slot.record_control(control.clone());
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(receipt(
            self,
            request_id,
            revision,
            session_id,
            Some(control),
        ))
    }

    pub fn follow_up(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let control = slot
            .conversation
            .submit_follow_up(text.clone())
            .map_err(|error| control_error(error, text))?;
        slot.record_control(control.clone());
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(receipt(
            self,
            request_id,
            revision,
            session_id,
            Some(control),
        ))
    }

    pub fn queue_withdraw(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let control = slot
            .conversation
            .withdraw_follow_up(control_id)
            .map_err(|error| control_error(error, String::new()))?;
        slot.record_control(control.clone());
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(receipt(
            self,
            request_id,
            revision,
            session_id,
            Some(control),
        ))
    }

    pub fn queue_replace(
        &self,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
        text: String,
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let replacement = slot
            .conversation
            .replace_follow_up(control_id, text.clone())
            .map_err(|error| control_error(error, text.clone()))?;
        slot.record_control(replacement.clone());
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(receipt(
            self,
            request_id,
            revision,
            session_id,
            Some(replacement),
        ))
    }

    pub fn queue_send_now(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
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
        match slot
            .conversation
            .promote_follow_up(control_id)
            .map_err(|error| control_error(error, text.clone()))?
        {
            FollowUpPromotion::Injected(control) => {
                slot.record_control(control.clone());
                let revision = self.bump_and_emit_session(session_id, &slot);
                Ok(receipt(
                    self,
                    request_id,
                    revision,
                    session_id,
                    Some(control),
                ))
            }
            FollowUpPromotion::Reserved {
                control,
                reservation,
            } => {
                self.begin_turn(&slot, &text)?;
                slot.record_control(control.clone());
                let revision = self.emit_session_changed(session_id, &slot);
                let result_control = control;
                self.spawn_operation(session_id, slot, move |sink| {
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
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let phase = slot.lock_state().phase;
        let control = match phase {
            SessionPhase::Reserved | SessionPhase::Running | SessionPhase::Stopping => Some(
                slot.conversation
                    .interrupt()
                    .map_err(|error| control_error(error, String::new()))?,
            ),
            SessionPhase::Compacting => {
                let mut state = slot.lock_state();
                state
                    .compaction_cancellation
                    .as_ref()
                    .ok_or_else(|| session_busy(String::new()))?
                    .cancel();
                state.phase = SessionPhase::Stopping;
                None
            }
            SessionPhase::Idle => return Err(session_busy(String::new())),
        };
        if let Some(control) = &control {
            slot.record_control(control.clone());
        }
        {
            let mut state = slot.lock_state();
            state.phase = SessionPhase::Stopping;
            state.session_revision = state.session_revision.saturating_add(1);
        }
        let revision = self.emit_session_changed(session_id, &slot);
        Ok(receipt(self, request_id, revision, session_id, control))
    }

    pub fn compact(
        self: &Arc<Self>,
        request_id: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<ActionReceipt, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let cancellation = CancellationToken::new();
        {
            let mut state = slot.lock_state();
            if state.phase != SessionPhase::Idle || !slot.conversation.pending_controls().is_empty()
            {
                return Err(session_busy(String::new()));
            }
            state.phase = SessionPhase::Compacting;
            state.active_compaction = Some(ActiveCompactionSnapshot { started_at: now() });
            state.compaction_cancellation = Some(cancellation.clone());
            state.terminal = None;
            state.session_revision = state.session_revision.saturating_add(1);
        }
        let revision = self.emit_session_changed(session_id, &slot);
        let conversation = Arc::clone(&slot.conversation);
        self.spawn_operation(session_id, slot, move |_| {
            conversation
                .compact(&cancellation)
                .err()
                .map(|error| SessionTerminalSnapshot {
                    status: if matches!(error, ConversationError::CompactionInterrupted(_)) {
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
    ) -> Result<ThreadSummary, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        if slot.lock_state().phase != SessionPhase::Idle {
            return Err(session_busy(name.to_string()));
        }
        self.catalog
            .rename(session_id, name)
            .map_err(internal_error)?;
        let summary = self
            .catalog
            .read_thread_summary(session_id)
            .map_err(resume_error)?;
        slot.lock_state().history.summary = summary.clone();
        self.emit_workbench_changed()?;
        Ok(summary)
    }

    pub fn archive_session(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Value, WorkbenchError> {
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        if slot.lock_state().phase != SessionPhase::Idle
            || !slot.conversation.pending_controls().is_empty()
        {
            return Err(session_busy(String::new()));
        }
        self.catalog.archive(session_id).map_err(resume_error)?;
        self.lock_sessions().remove(session_id);
        self.emit_workbench_changed()?;
        Ok(json!({"archived": true}))
    }

    pub fn update_settings(
        &self,
        workspace_id: &str,
        session_id: &str,
        selector: &str,
    ) -> Result<Value, WorkbenchError> {
        self.runner
            .validate_model_selector(Some(selector))
            .map_err(configuration_error)?;
        let workspace = self.require_workspace(workspace_id)?;
        let slot = self.open_slot(&workspace, session_id)?;
        let parts = split_model_selector(selector);
        let timing = slot
            .conversation
            .update_settings(SettingsPatch {
                provider: parts.provider.map(str::to_string),
                model: parts.model.map(str::to_string),
                reasoning: parts.effort.map_or(ReasoningPatch::Clear, |value| {
                    ReasoningPatch::Set(value.to_string())
                }),
            })
            .map_err(conversation_error)?;
        let timing = match timing {
            RuntimeSettingsTiming::NothingToApply => SettingsApplyTiming::NothingToApply,
            RuntimeSettingsTiming::AppliedNow => SettingsApplyTiming::NextTurn,
        };
        let revision = self.bump_and_emit_session(session_id, &slot);
        Ok(json!({
            "selector": slot.conversation.thread().model,
            "applyTiming": timing,
            "revision": revision
        }))
    }

    fn open_slot(
        &self,
        workspace: &Workspace,
        session_id: &str,
    ) -> Result<Arc<ConversationSlot>, WorkbenchError> {
        if let Some(slot) = self.lock_sessions().get(session_id).cloned() {
            verify_workspace_thread(workspace, &slot.conversation.thread().cwd)?;
            return Ok(slot);
        }
        let thread = self
            .catalog
            .resume_thread(session_id)
            .map_err(resume_error)?;
        verify_workspace_thread(workspace, &thread.cwd)?;
        self.insert_slot(thread)
    }

    fn insert_slot(
        &self,
        thread: singularity_protocol::Thread,
    ) -> Result<Arc<ConversationSlot>, WorkbenchError> {
        let session_id = thread.thread_id.clone();
        let conversation =
            Conversation::new(Arc::clone(&self.runner), thread).map_err(conversation_error)?;
        let (history, controls) = self
            .catalog
            .read_snapshot(&session_id)
            .map_err(resume_error)?;
        let slot = Arc::new(ConversationSlot {
            conversation,
            state: Mutex::new(SlotState {
                history,
                session_revision: 0,
                phase: SessionPhase::Idle,
                controls,
                active_turn: None,
                active_compaction: None,
                terminal: None,
                compaction_cancellation: None,
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
    ) -> Result<SessionReadResult, WorkbenchError> {
        let mut state = slot.lock_state();
        if state.phase == SessionPhase::Idle {
            self.refresh_history(slot, &mut state)
                .map_err(resume_error)?;
        }
        let history = singularity_runtime::page_history(&state.history, limit, before_turn)
            .map_err(resume_error)?;
        Ok(SessionReadResult {
            summary: history.summary.clone(),
            history,
            runtime: slot.snapshot_from(&state),
        })
    }

    // Freeze the latest durable history before any events of this chain arrive.
    // Both start paths wait for the previous worker's complete Workbench settlement.
    fn begin_turn(&self, slot: &ConversationSlot, input: &str) -> Result<(), WorkbenchError> {
        let mut state = slot.lock_state();
        if state.phase != SessionPhase::Idle {
            return Err(session_busy(input.to_string()));
        }
        self.refresh_history(slot, &mut state)
            .map_err(|error| resume_error(error).preserve(input))?;
        state.phase = SessionPhase::Reserved;
        state.terminal = None;
        state.session_revision = state.session_revision.saturating_add(1);
        Ok(())
    }

    fn refresh_history(
        &self,
        slot: &ConversationSlot,
        state: &mut SlotState,
    ) -> Result<(), ResumeError> {
        let (history, controls) = self
            .catalog
            .read_snapshot(&slot.conversation.thread().thread_id)?;
        state.history = history;
        state.controls = controls;
        state.active_turn = None;
        Ok(())
    }

    fn require_workspace(&self, workspace_id: &str) -> Result<Workspace, WorkbenchError> {
        self.workspaces.find(workspace_id).ok_or_else(|| {
            WorkbenchError::new(
                RpcErrorCode::WorkspaceNotFound,
                "Workspace 不存在或已移除。",
                "刷新工作台并重新选择 Workspace。",
            )
        })
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, WorkbenchError> {
        self.require_workspace(workspace_id)
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        let mut payload = singularity_protocol::turn_event_envelope(&event);
        let started_at = now();
        if matches!(
            event,
            TurnEvent::ToolExecutionStart { .. } | TurnEvent::TurnStarted { .. }
        ) && let Some(params) = payload.get_mut("params").and_then(Value::as_object_mut)
        {
            params.insert("startedAt".to_string(), Value::String(started_at.clone()));
        }
        let mut state = slot.lock_state();
        if state.phase != SessionPhase::Stopping {
            state.phase = SessionPhase::Running;
        }
        state.session_revision += 1;
        payload["sessionRevision"] = json!(state.session_revision);
        if let TurnEvent::TurnStarted { turn, .. } = &event {
            let active = state.active_turn.get_or_insert_with(|| ActiveTurnSnapshot {
                turn_id: turn.turn_id.clone(),
                events: Vec::new(),
                started_at: started_at.clone(),
            });
            active.turn_id = turn.turn_id.clone();
            active.started_at = started_at;
        }
        if let Some(active) = state.active_turn.as_mut() {
            active.events.push(payload.clone());
        }
        self.emit(StreamType::TurnEvent, Some(session_id), payload);
    }

    fn on_session_settled(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        terminal: Option<SessionTerminalSnapshot>,
    ) {
        {
            let mut state = slot.lock_state();
            state.phase = SessionPhase::Idle;
            state.active_compaction = None;
            state.compaction_cancellation = None;
            state.terminal = terminal;
            if let Err(error) = self.refresh_history(slot, &mut state) {
                state.terminal = Some(SessionTerminalSnapshot {
                    status: TurnStatus::Failed,
                    message: Some(format!("会话结果无法读取：{error}")),
                });
            }
            state.session_revision += 1;
        }
        self.emit(
            StreamType::SessionSettled,
            Some(session_id),
            json!({"runtime": slot.snapshot()}),
        );
    }

    fn spawn_operation(
        self: &Arc<Self>,
        session_id: &str,
        slot: Arc<ConversationSlot>,
        run: impl FnOnce(&mut dyn FnMut(TurnEvent)) -> Option<SessionTerminalSnapshot> + Send + 'static,
    ) {
        let workbench = Arc::clone(self);
        let session_id = session_id.to_string();
        std::thread::spawn(move || {
            let terminal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&mut |event| workbench.on_turn_event(&session_id, &slot, event))
            }))
            .unwrap_or_else(|_| {
                Some(SessionTerminalSnapshot {
                    status: TurnStatus::Failed,
                    message: Some("任务执行异常，已停止。可重新提交或恢复会话。".into()),
                })
            });
            workbench.on_session_settled(&session_id, &slot, terminal);
        });
    }

    fn bump_and_emit_session(&self, session_id: &str, slot: &ConversationSlot) -> u64 {
        {
            let mut state = slot.lock_state();
            state.session_revision = state.session_revision.saturating_add(1);
        }
        self.emit_session_changed(session_id, slot)
    }

    fn emit_session_changed(&self, session_id: &str, slot: &ConversationSlot) -> u64 {
        self.emit(
            StreamType::SessionChanged,
            Some(session_id),
            serde_json::to_value(slot.snapshot()).unwrap_or_else(|_| json!({})),
        )
    }

    fn emit_workbench_changed(&self) -> Result<u64, WorkbenchError> {
        let payload = serde_json::to_value(self.bootstrap()?).map_err(|error| {
            internal_error(format!(
                "workbench projection could not be serialized: {error}"
            ))
        })?;
        Ok(self.emit(StreamType::WorkbenchChanged, None, payload))
    }

    #[allow(clippy::expect_used)]
    fn emit(&self, event_type: StreamType, session_id: Option<&str>, payload: Value) -> u64 {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let revision = *order;
        let _ = self.stream.send(StreamEnvelope {
            version: WORKBENCH_PROTOCOL_VERSION,
            generation: self.generation.clone(),
            revision,
            event_type,
            session_id: session_id.map(str::to_string),
            payload,
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

    fn snapshot(&self) -> SessionSnapshot {
        let state = self.lock_state();
        self.snapshot_from(&state)
    }

    fn snapshot_from(&self, state: &SlotState) -> SessionSnapshot {
        SessionSnapshot {
            session_revision: state.session_revision,
            phase: state.phase,
            selector: self.conversation.thread().model,
            controls: state.controls.clone(),
            pending_controls: self.conversation.pending_controls(),
            active_turn: state.active_turn.clone(),
            active_compaction: state.active_compaction.clone(),
            terminal: state.terminal.clone(),
        }
    }

    fn record_control(&self, control: singularity_protocol::ControlSnapshot) {
        let mut state = self.lock_state();
        match state
            .controls
            .iter()
            .position(|existing| existing.control_id == control.control_id)
        {
            Some(index) => state.controls[index] = control,
            None => {
                state.controls.push(control);
                state.controls.sort_by_key(|entry| entry.sequence);
            }
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

fn verify_workspace_thread(workspace: &Workspace, cwd: &str) -> Result<(), WorkbenchError> {
    let workspace = singularity_core::canonicalize_workspace(&workspace.root)
        .map_err(|error| internal_error(error.to_string()))?;
    let thread = singularity_core::canonicalize_workspace(cwd)
        .map_err(|error| internal_error(error.to_string()))?;
    if workspace.matches(&thread) {
        Ok(())
    } else {
        Err(WorkbenchError::new(
            RpcErrorCode::Conflict,
            "Session 不属于所选 Workspace。",
            "刷新工作台并从所属 Workspace 打开该 Session。",
        ))
    }
}

fn commands() -> Vec<CommandDescriptor> {
    vec![
        CommandDescriptor {
            name: "/compact".to_string(),
            description: "压缩当前 Session 的上下文".to_string(),
            availability: "idle".to_string(),
        },
        CommandDescriptor {
            name: "/model".to_string(),
            description: "打开模型设置".to_string(),
            availability: "always".to_string(),
        },
        CommandDescriptor {
            name: "/help".to_string(),
            description: "查看工作台帮助".to_string(),
            availability: "always".to_string(),
        },
    ]
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn invalid_request(message: impl Into<String>) -> WorkbenchError {
    WorkbenchError::new(RpcErrorCode::InvalidRequest, message, "检查输入后重试。")
}

fn internal_error(message: impl Into<String>) -> WorkbenchError {
    WorkbenchError::new(
        RpcErrorCode::Internal,
        message,
        "刷新工作台；若问题持续，检查启动终端中的错误。",
    )
}

fn configuration_error(message: impl Into<String>) -> WorkbenchError {
    WorkbenchError::new(
        RpcErrorCode::ConfigurationInvalid,
        message,
        "打开 Models 并修正配置。",
    )
}

fn model_error(error: singularity_model::ProviderError) -> WorkbenchError {
    configuration_error(error.to_string())
}

fn conversation_error(error: ConversationError) -> WorkbenchError {
    match error {
        ConversationError::TurnAlreadyActive => session_busy(String::new()),
        ConversationError::Configuration(message) => configuration_error(message),
        ConversationError::CompactionInterrupted(message) => internal_error(message),
        ConversationError::Turn(error) => internal_error(error.to_string()),
    }
}

fn resume_error(error: ResumeError) -> WorkbenchError {
    match error {
        ResumeError::NotFound(_) => WorkbenchError::new(
            RpcErrorCode::SessionNotFound,
            "Session 不存在或已归档。",
            "刷新 Workspace 的 Session 列表。",
        ),
        ResumeError::WriterActive => session_busy(String::new()),
        other => internal_error(other.to_string()),
    }
}

fn session_busy(input: String) -> WorkbenchError {
    let error = WorkbenchError::new(
        RpcErrorCode::SessionBusy,
        "当前 Session 正在处理另一项操作。",
        "等待状态变为空闲，或使用当前阶段提供的控制动作。",
    );
    if input.is_empty() {
        error
    } else {
        error.preserve(input)
    }
}

fn control_not_found(input: String) -> WorkbenchError {
    let error = WorkbenchError::new(
        RpcErrorCode::ControlNotFound,
        "待处理输入已不存在或已经开始执行。",
        "刷新 Session 后确认当前 Follow-up 队列。",
    );
    if input.is_empty() {
        error
    } else {
        error.preserve(input)
    }
}

fn control_error(error: ConversationControlError, input: String) -> WorkbenchError {
    match error {
        ConversationControlError::NotRunning => session_busy(input),
        ConversationControlError::InvalidInput => invalid_request("输入不能为空。").preserve(input),
        ConversationControlError::ControlNotFound => control_not_found(input),
        ConversationControlError::Storage(message) => internal_error(message).preserve(input),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::{Duration, Instant};

    use singularity_agent::session::test_support::WorkspaceFixture;
    use singularity_model::{
        ModelError, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
        ProviderStreamEvent,
    };
    use singularity_protocol::{HistoryItem, RpcErrorCode, StreamType};

    use super::*;

    struct BlockingProvider {
        started: Sender<String>,
        release: Mutex<Receiver<()>>,
    }

    impl Provider for BlockingProvider {
        fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
            singularity_runtime::test_support::test_model_configuration()
        }

        fn complete_stream(
            &self,
            request: &ModelTurnRequest,
            cancellation: &CancellationToken,
            _on_event: &mut dyn FnMut(ProviderStreamEvent),
            _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
        ) -> Result<ModelTurnResponse, ProviderError> {
            let input = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == singularity_model::ModelRole::User)
                .map(|message| message.content.clone())
                .unwrap_or_default();
            let panic_requested = input == "panic-provider";
            self.started.send(input).expect("report request");
            assert!(!panic_requested, "injected provider panic");
            self.release.lock().expect("release lock").recv().ok();
            if cancellation.is_cancelled() {
                return Err(ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::Cancelled,
                    "cancelled by test",
                )));
            }
            Ok(ModelTurnResponse::completed(
                request.request_id.clone(),
                "response",
                "done".to_string(),
            ))
        }
    }

    #[test]
    fn three_sessions_run_without_a_browser_and_keep_inputs_isolated() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let provider = Arc::new(BlockingProvider {
            started: started_tx,
            release: Mutex::new(release_rx),
        });
        let fixture = fixture(provider);
        let workspace = fixture
            .workbench
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .expect("workspace");
        let sessions: Vec<_> = (0..3)
            .map(|_| {
                fixture
                    .workbench
                    .create_session(&workspace.workspace_id, None)
                    .expect("session")
                    .summary
                    .thread_id
            })
            .collect();

        for (index, session_id) in sessions.iter().enumerate() {
            fixture
                .workbench
                .submit(
                    &format!("request-{index}"),
                    &workspace.workspace_id,
                    session_id,
                    format!("input-{index}"),
                )
                .expect("submit");
        }
        let mut started: Vec<_> = (0..3)
            .map(|_| {
                started_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("all sessions reached provider")
            })
            .collect();
        started.sort();
        assert_eq!(started, vec!["input-0", "input-1", "input-2"]);

        for session_id in &sessions {
            let snapshot = fixture
                .workbench
                .read_session(&workspace.workspace_id, session_id, 100, None)
                .expect("running snapshot");
            assert!(
                snapshot.history.turns.is_empty(),
                "live turns are excluded from durable history until settled"
            );
            assert!(snapshot.runtime.active_turn.is_some());
        }

        let duplicate = fixture.workbench.submit(
            "duplicate",
            &workspace.workspace_id,
            &sessions[0],
            "keep this text".to_string(),
        );
        assert!(matches!(duplicate, Err(ref error)
            if error.code == RpcErrorCode::SessionBusy
                && error.preserved_input.as_deref() == Some("keep this text")));

        for (index, session_id) in sessions.iter().enumerate() {
            fixture
                .workbench
                .abort(
                    &format!("abort-{index}"),
                    &workspace.workspace_id,
                    session_id,
                )
                .expect("abort");
        }
        for _ in 0..3 {
            release_tx.send(()).expect("release provider");
        }
        wait_for_idle(&fixture.workbench, &workspace, &sessions);

        for (index, session_id) in sessions.iter().enumerate() {
            let snapshot = fixture
                .workbench
                .read_session(&workspace.workspace_id, session_id, 100, None)
                .expect("read session");
            let messages: Vec<_> = snapshot
                .history
                .turns
                .iter()
                .flat_map(|turn| &turn.items)
                .filter_map(|item| match item {
                    HistoryItem::Message { role, text, .. } if role == "user" => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(messages, vec![format!("input-{index}")]);
            assert_eq!(snapshot.runtime.phase, SessionPhase::Idle);
        }
    }

    #[test]
    fn stream_is_bounded_and_reports_lag_without_blocking_emitters() {
        let fixture = fixture(Arc::new(
            singularity_model::test_support::ScriptedProvider::new([
                singularity_model::test_support::ScriptedAttempt::success("done"),
            ]),
        ));
        let mut receiver = fixture.workbench.subscribe();
        for index in 0..=STREAM_CAPACITY {
            fixture
                .workbench
                .emit(StreamType::WorkbenchChanged, None, json!({"index": index}));
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(1))
        ));
    }

    #[test]
    fn creating_a_session_preserves_its_requested_selector() {
        let fixture = fixture(Arc::new(
            singularity_model::test_support::ScriptedProvider::new([]),
        ));
        let workspace = fixture
            .workbench
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .expect("workspace");
        let snapshot = fixture
            .workbench
            .create_session(
                &workspace.workspace_id,
                Some("openai_compatible/chosen-model".to_string()),
            )
            .expect("session");
        assert_eq!(
            snapshot.runtime.selector.as_deref(),
            Some("openai_compatible/chosen-model")
        );
    }

    #[test]
    fn worker_panic_settles_the_slot_and_allows_another_turn() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let fixture = fixture(Arc::new(BlockingProvider {
            started: started_tx,
            release: Mutex::new(release_rx),
        }));
        let workspace = fixture
            .workbench
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .expect("workspace");
        let session = fixture
            .workbench
            .create_session(&workspace.workspace_id, None)
            .expect("session");
        let id = session.summary.thread_id;
        fixture
            .workbench
            .submit(
                "panic",
                &workspace.workspace_id,
                &id,
                "panic-provider".to_string(),
            )
            .expect("submit");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("started");
        wait_for_idle(&fixture.workbench, &workspace, std::slice::from_ref(&id));
        let snapshot = fixture
            .workbench
            .read_session(&workspace.workspace_id, &id, 100, None)
            .expect("settled snapshot");
        assert_eq!(
            snapshot.runtime.terminal.expect("terminal").status,
            TurnStatus::Failed
        );
        fixture
            .workbench
            .submit("retry", &workspace.workspace_id, &id, "retry".to_string())
            .expect("next submit");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("next started");
        release_tx.send(()).expect("release");
        wait_for_idle(&fixture.workbench, &workspace, &[id]);
    }

    #[test]
    fn idle_reads_and_new_chains_use_the_latest_durable_history() {
        let provider = Arc::new(singularity_model::test_support::ScriptedProvider::new([
            singularity_model::test_support::ScriptedAttempt::success("first"),
            singularity_model::test_support::ScriptedAttempt::success("second"),
        ]));
        let fixture = fixture(provider);
        let host = &fixture.workbench;
        let workspace = host
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .unwrap();
        let created = host.create_session(&workspace.workspace_id, None).unwrap();
        let id = created.summary.thread_id;
        let external = Conversation::new(
            Arc::clone(&host.runner),
            host.catalog.resume_thread(&id).unwrap(),
        )
        .unwrap();
        external
            .run_turn("first external input", &mut |_| {})
            .unwrap();
        let bootstrap = host.bootstrap().unwrap();
        assert_eq!(
            bootstrap.sessions_by_workspace[&workspace.workspace_id][0].turn_count,
            1
        );
        let read = host
            .read_session(&workspace.workspace_id, &id, 40, None)
            .unwrap();
        assert_eq!(read.history.turns.len(), 1);
        external
            .run_turn("second external input", &mut |_| {})
            .unwrap();

        // Starting without a prior browser read must freeze both external turns.
        let slot = host.open_slot(&workspace, &id).unwrap();
        let reservation = slot.conversation.reserve_start().unwrap();
        host.begin_turn(&slot, "new chain").unwrap();
        let read = host
            .read_session(&workspace.workspace_id, &id, 40, None)
            .unwrap();
        assert_eq!(read.history.turns.len(), 2);
        assert_eq!(read.runtime.phase, SessionPhase::Reserved);
        assert!(read.runtime.active_turn.is_none());
        drop(reservation);
        host.on_session_settled(&id, &slot, None);
    }

    #[test]
    fn send_now_waits_for_workbench_settlement_and_keeps_the_pending_input() {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let fixture = fixture(Arc::new(BlockingProvider {
            started: started_tx,
            release: Mutex::new(release_rx),
        }));
        let host = &fixture.workbench;
        let workspace = host
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .unwrap();
        let created = host.create_session(&workspace.workspace_id, None).unwrap();
        let id = created.summary.thread_id;
        let slot = host.open_slot(&workspace, &id).unwrap();
        let reservation = slot.conversation.reserve_start().unwrap();
        host.begin_turn(&slot, "first").unwrap();
        let worker = {
            let host = Arc::clone(host);
            let slot = Arc::clone(&slot);
            let id = id.clone();
            std::thread::spawn(move || {
                reservation.run("first", &mut |event| {
                    host.on_turn_event(&id, &slot, event);
                })
            })
        };
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let pending = host
            .follow_up("queue", &workspace.workspace_id, &id, "next".into())
            .unwrap()
            .control
            .unwrap();
        host.abort("abort", &workspace.workspace_id, &id).unwrap();
        release_tx.send(()).unwrap();
        let outcome = worker.join().unwrap();

        // The runtime reservation is released, but the old Workbench worker has
        // not settled yet. This is an explicit scheduling boundary, not a sleep.
        let rejected =
            host.queue_send_now("early", &workspace.workspace_id, &id, &pending.control_id);
        assert!(matches!(rejected, Err(error) if error.code == RpcErrorCode::SessionBusy));
        assert_eq!(slot.conversation.pending_controls(), vec![pending.clone()]);
        assert_eq!(slot.lock_state().phase, SessionPhase::Stopping);
        host.on_session_settled(&id, &slot, turn_terminal(outcome));
        host.queue_send_now("retry", &workspace.workspace_id, &id, &pending.control_id)
            .unwrap();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "next"
        );
        release_tx.send(()).unwrap();
        wait_for_idle(host, &workspace, &[id]);
    }

    struct Fixture {
        _home: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
        workspace: WorkspaceFixture,
        workbench: Arc<Workbench>,
    }

    fn fixture(provider: Arc<dyn Provider + Send + Sync>) -> Fixture {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("sessions")).expect("sessions");
        let config = json!({
            "version": 1,
            "default_provider": "openai_compatible",
            "default_model": "openai_compatible/base-model",
            "providers": {
                "openai_compatible": {
                    "base_url": "http://127.0.0.1:9/v1",
                    "models": {
                        "base-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128000,
                            "max_output_tokens": 4096
                        },
                        "chosen-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128000,
                            "max_output_tokens": 4096
                        }
                    }
                }
            }
        });
        std::fs::write(home.path().join("config.json"), config.to_string()).expect("config");
        std::fs::write(
            home.path().join("auth.json"),
            json!({
                "schema_version": 1,
                "providers": {"openai_compatible": {"api_key": "test-key"}}
            })
            .to_string(),
        )
        .expect("auth");
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let models = ModelConfigOwner::open_at(home.path().to_path_buf(), runtime.handle().clone());
        let runner = Arc::new(
            TurnRunner::new(home.path().join("sessions"), models.snapshot())
                .with_provider_override(provider),
        );
        let catalog = ThreadCatalog::new(&runner);
        let workspaces = WorkspaceStore::open(home.path()).expect("workspace store");
        let workbench = Workbench::new(
            "127.0.0.1:3080".to_string(),
            runner,
            catalog,
            workspaces,
            models,
        );
        Fixture {
            _home: home,
            _runtime: runtime,
            workspace: WorkspaceFixture::new(),
            workbench,
        }
    }

    fn wait_for_idle(workbench: &Workbench, workspace: &Workspace, sessions: &[String]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let idle = sessions.iter().all(|session_id| {
                workbench
                    .read_session(&workspace.workspace_id, session_id, 100, None)
                    .is_ok_and(|snapshot| snapshot.runtime.phase == SessionPhase::Idle)
            });
            if idle {
                return;
            }
            assert!(Instant::now() < deadline, "sessions did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
