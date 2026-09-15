//! 本地工作台的深模块：Workspace、Session、模型设置与运行态只有这一层组合。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use singularity_core::{CancellationToken, now_iso};
use singularity_model::ModelConfigOwner;
use singularity_protocol::{
    ActiveCompactionSnapshot, ActiveTurnRuntimeSnapshot, EmptyParams, ProviderConfigurationInput,
    RedactedModelCatalog, ResyncRequiredPayload, RpcError, RpcErrorCode, SessionPhase,
    SessionReadResult, SessionRuntime, SessionSettledPayload, SessionTerminalSnapshot,
    StreamEnvelope, StreamEvent, TurnEvent, TurnStatus, WORKBENCH_PROTOCOL_VERSION,
    WorkbenchBootstrap, Workspace,
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
    revision: Mutex<u64>,
    /// 完整工作台快照的构造与发布顺序；不覆盖会话执行或普通增量事件。
    workbench_publication: Mutex<()>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    /// 与 runner 共享的磁盘配置入口；每次读取都在短临界区内完成。
    models: Arc<Mutex<ModelConfigOwner>>,
    sessions: Mutex<HashMap<String, Arc<ConversationSlot>>>,
    stream: broadcast::Sender<StreamEnvelope>,
}

struct ConversationSlot {
    conversation: Arc<Conversation>,
    state: Mutex<SlotState>,
}

struct ActiveTurn {
    turn_id: String,
    events: Vec<singularity_protocol::WorkbenchTurnEvent>,
    started_at: String,
}

struct SlotState {
    history: Option<Arc<singularity_runtime::ThreadSnapshot>>,
    session_revision: u64,
    active_turn: Option<ActiveTurn>,
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
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }
    pub fn new(
        runner: Arc<TurnRunner>,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<Mutex<ModelConfigOwner>>,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            generation: Uuid::new_v4().to_string(),
            revision: Mutex::new(0),
            workbench_publication: Mutex::new(()),
            runner,
            catalog,
            workspaces,
            models,
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
        // 目录读取在独立短作用域内完成：模型锁不得带入会话锁与页面发布。
        let catalog = {
            let models = self.lock_models();
            models.redacted_catalog()
        };
        self.bootstrap_with_catalog(catalog)
    }

    fn bootstrap_with_catalog(
        &self,
        model_catalog: RedactedModelCatalog,
    ) -> Result<WorkbenchBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        // 当前任务目录以 catalog 为唯一权威：冻结历史只服务于执行内容恢复，
        // 不再回填目录摘要。阶段读取只问 Conversation，不取 Slot 状态锁。
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let session_phases = self
            .lock_sessions()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.conversation.phase()))
            .collect();
        let sessions_by_workspace =
            WorkspaceStore::group_threads(&workspaces, &threads).map_err(internal_error)?;
        Ok(WorkbenchBootstrap {
            session_phases,
            generation: self.generation.clone(),
            revision,
            workspaces,
            sessions_by_workspace,
            model_catalog,
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

    pub fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<(), RpcError> {
        self.workspaces
            .rename(workspace_id, name)
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn remove_workspace(&self, workspace_id: &str) -> Result<(), RpcError> {
        let workspace = self.workspace(workspace_id)?;
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let grouped = WorkspaceStore::group_threads(std::slice::from_ref(&workspace), &threads)
            .map_err(internal_error)?;
        for thread in grouped.get(workspace_id).into_iter().flatten() {
            let slot = self.lock_sessions().get(&thread.thread_id).cloned();
            let busy = slot.is_some_and(|slot| session_occupied(&slot.conversation));
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
        Ok(())
    }

    pub fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), RpcError> {
        self.update_models(|models| models.save_provider(provider, api_key))
    }

    pub fn set_api_key(&self, provider_id: &str, api_key: &str) -> Result<(), RpcError> {
        self.update_models(|models| models.set_api_key(provider_id, api_key))
    }

    pub fn remove_provider(&self, provider_id: &str) -> Result<(), RpcError> {
        self.update_models(|models| models.remove_provider(provider_id))
    }

    fn update_models(
        &self,
        update: impl FnOnce(&mut ModelConfigOwner) -> Result<(), singularity_model::ProviderError>,
    ) -> Result<(), RpcError> {
        let _publication = self.lock_workbench_publication();
        let mut models = self.lock_models();
        let result = update(&mut models).map_err(model_error);
        // 配置与凭据是两个独立文件。第二次写入失败时，
        // 不能让后续 turn 继续使用旧配置的快照；
        // 共享 owner 会重新读取文件，因此其他对象无需刷新。
        let catalog = models.redacted_catalog();
        drop(models);
        self.publish_workbench_result(self.bootstrap_with_catalog(catalog));
        result
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, RpcError> {
        // 配置快照在锁内取得，网络请求在锁外直接进入发现实现。
        let request = self
            .lock_models()
            .model_discovery_request(provider_id, base_url, api_key)
            .map_err(model_discovery_error)?;
        singularity_model::discover_models(request, base_url)
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
        let slot = self.insert_slot(thread);
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

    pub fn submit(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        if text.trim().is_empty() {
            return Err(invalid_request("任务内容不能为空。"));
        }
        let slot = self.open_slot(workspace_id, session_id)?;
        let selector = slot.conversation.thread().model;
        self.runner
            .validate_model_selector(selector.as_deref())
            .map_err(configuration_error)?;
        let reservation = slot
            .conversation
            .reserve_start()
            .map_err(conversation_error)?;
        let history = self.freeze_history(&slot)?;
        {
            let mut state = slot.lock_state();
            self.begin_turn_locked(&mut state, history);
            self.publish_session_locked(session_id, &slot, &mut state);
        }
        self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
            turn_terminal(reservation.run(&text, sink))
        });
        Ok(())
    }

    pub fn steer(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(session_id, &slot, move |conversation| {
            conversation.steer(text).map(|_| ())
        })
    }

    pub fn follow_up(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(session_id, &slot, move |conversation| {
            conversation.submit_follow_up(text).map(|_| ())
        })
    }

    pub fn queue_withdraw(
        &self,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(session_id, &slot, |conversation| {
            conversation.withdraw_follow_up(control_id).map(|_| ())
        })
    }

    pub fn queue_replace(
        &self,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(session_id, &slot, move |conversation| {
            conversation.replace_follow_up(control_id, text).map(|_| ())
        })
    }

    pub fn queue_send_now(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.runner
            .validate_model_selector(slot.conversation.thread().model.as_deref())
            .map_err(configuration_error)?;
        // 与 worker 的事件及结算共用 SlotState 顺序：控制从 Conversation
        // 转移到公开投影并发布之前，结算不能插入并被旧回执覆盖。
        let mut state = slot.lock_state();
        let promoted = slot
            .conversation
            .promote_follow_up(control_id)
            .map_err(control_error)?;
        match promoted {
            FollowUpPromotion::Injected(_) => {
                self.publish_session_locked(session_id, &slot, &mut state);
                Ok(())
            }
            FollowUpPromotion::Reserved { reservation, .. } => {
                // 预订成立即独占该会话；释放 slot 锁去取 history，再按同一顺序提交。
                drop(state);
                let history = self.freeze_history(&slot)?;
                let mut state = slot.lock_state();
                self.begin_turn_locked(&mut state, history);
                self.publish_session_locked(session_id, &slot, &mut state);
                drop(state);
                self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
                    turn_terminal(reservation.run_promoted(sink))
                });
                Ok(())
            }
        }
    }

    pub fn abort(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        self.apply_control(session_id, &slot, Conversation::abort)
    }

    /// 会话控制的接受、公开投影与发布共用 SlotState 顺序。闭包只执行
    /// Conversation 的短控制操作，不得覆盖 Agent 执行或调用事件 sink。
    fn apply_control(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        apply: impl FnOnce(&Conversation) -> Result<(), ConversationControlError>,
    ) -> Result<(), RpcError> {
        let mut state = slot.lock_state();
        apply(&slot.conversation).map_err(control_error)?;
        self.publish_session_locked(session_id, slot, &mut state);
        Ok(())
    }

    fn publish_session_locked(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        state: &mut SlotState,
    ) -> u64 {
        state.session_revision = state.session_revision.saturating_add(1);
        self.emit(StreamEvent::SessionChanged {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(state),
        })
    }

    pub fn compact(self: &Arc<Self>, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        let reservation = slot
            .conversation
            .reserve_compaction(CancellationToken::new())
            .map_err(conversation_error)?;
        let history = self.freeze_history(&slot)?;
        {
            let mut state = slot.lock_state();
            self.begin_turn_locked(&mut state, history);
            state.active_compaction = Some(ActiveCompactionSnapshot {
                started_at: now_iso(),
            });
            self.publish_session_locked(session_id, &slot, &mut state);
        }
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
        Ok(())
    }

    pub fn rename_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation.phase() != SessionPhase::Idle {
            return Err(session_busy());
        }
        self.catalog
            .rename(session_id, name)
            .map_err(catalog_error)?;
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn archive_session(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        if session_occupied(&slot.conversation) {
            return Err(session_busy());
        }
        self.catalog.archive(session_id).map_err(catalog_error)?;
        self.lock_sessions().remove(session_id);
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn update_settings(
        &self,
        workspace_id: &str,
        session_id: &str,
        selector: &str,
    ) -> Result<(), RpcError> {
        let slot = self.open_slot(workspace_id, session_id)?;
        slot.conversation
            .update_settings(selector)
            .map_err(conversation_error)?;
        self.bump_and_emit_session(session_id, &slot);
        Ok(())
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
        Ok(self.insert_slot(thread))
    }

    fn verify_session_scope(&self, workspace_id: &str, cwd: &str) -> Result<(), RpcError> {
        verify_workspace_thread(&self.workspace(workspace_id)?, cwd)
    }

    /// cwd 查询：已打开的任务用其运行态线程，未打开的任务用目录摘要。
    /// 查询本身不恢复会话，也不为拿目录而创建 Conversation 或写日志。
    pub fn session_directory(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<String, RpcError> {
        let cwd = match self.lock_sessions().get(session_id) {
            Some(slot) => slot.conversation.thread().cwd,
            None => {
                self.catalog
                    .read_thread_summary(session_id)
                    .map_err(catalog_error)?
                    .cwd
            }
        };
        self.verify_session_scope(workspace_id, &cwd)?;
        Ok(cwd)
    }

    fn insert_slot(&self, thread: singularity_protocol::Thread) -> Arc<ConversationSlot> {
        let session_id = thread.thread_id.clone();
        let conversation = Conversation::new(Arc::clone(&self.runner), thread);
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
        self.lock_sessions()
            .entry(session_id)
            .or_insert_with(|| Arc::clone(&slot))
            .clone()
    }

    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        // 目录读取在 slot 锁之外完成：缓存未命中时它读盘并解析整份会话，
        // 而 slot 锁同时服务 worker 的事件投影。
        let cached = { slot.lock_state().history.clone() };
        let snapshot = match &cached {
            Some(history) => Arc::clone(history),
            None => self.freeze_history(slot)?,
        };
        let mut state = slot.lock_state();
        if cached.is_none() {
            // 刷新持久化 history 同时清掉活动投影，与 begin_turn 冻结一致。
            state.active_turn = None;
        }
        let history = snapshot.page(limit, before_turn).map_err(catalog_error)?;
        Ok(SessionReadResult {
            history,
            runtime: slot.runtime_from(&state),
            active_events: state
                .active_turn
                .as_ref()
                .map(|active| active.events.clone())
                .unwrap_or_default(),
        })
    }

    // 在本链的任何事件到达前冻结最新的持久化 history，随后在 slot 锁内提交。
    // 两条 start 路径都等待上一个 worker 完成 Workbench 结算：预订成立时它已
    // 走完结算，因此这里的读盘不与事件投影竞争，可以放在锁外。
    fn begin_turn_locked(
        &self,
        state: &mut SlotState,
        history: Arc<singularity_runtime::ThreadSnapshot>,
    ) {
        state.history = Some(history);
        state.active_turn = None;
        state.terminal = None;
    }

    /// 读取最新的持久化 history；调用方负责在 slot 锁内提交它。
    fn freeze_history(
        &self,
        slot: &ConversationSlot,
    ) -> Result<Arc<singularity_runtime::ThreadSnapshot>, RpcError> {
        self.catalog
            .read_snapshot(&slot.conversation.thread().thread_id)
            .map_err(catalog_error)
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, RpcError> {
        // 缺失工作区的公开错误与其余工作区操作同源（workspace_error）。
        self.workspaces
            .find(workspace_id)
            .ok_or_else(|| workspace_error(WorkspaceError::NotFound))
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        // 控制处置变化归约为会话快照发布：控制事实只由会话快照一种表示
        // 承载，不进入活动 turn 的事件序列。
        if let TurnEvent::ControlChanged { .. } = &event {
            self.bump_and_emit_session(session_id, slot);
            return;
        }
        let mut state = slot.lock_state();
        state.session_revision += 1;
        if let TurnEvent::TurnStarted { turn, started_at } = &event {
            let active = state.active_turn.get_or_insert_with(|| ActiveTurn {
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
        };
        if let Some(active) = state.active_turn.as_mut() {
            // 恢复快照中已完成内容替换其进度；实时广播仍为增量。
            let replaced = match &envelope.event {
                TurnEvent::ToolExecutionUpdate { turn_id, item, .. }
                | TurnEvent::ToolExecutionEnd { turn_id, item, .. }
                | TurnEvent::ItemCompleted {
                    turn_id,
                    item,
                    content: Some(_),
                    ..
                }
                | TurnEvent::ItemFailed {
                    turn_id,
                    item,
                    content: Some(_),
                    ..
                } => Some((turn_id, &item.item_id)),
                _ => None,
            };
            if let Some((turn, item_id)) = replaced {
                active.events.retain(|previous| {
                    let progress = match &previous.event {
                        TurnEvent::ToolExecutionUpdate { turn_id, item, .. }
                        | TurnEvent::AssistantDelta { turn_id, item, .. }
                        | TurnEvent::AssistantThinkingDelta { turn_id, item, .. }
                        | TurnEvent::ItemStarted { turn_id, item, .. } => {
                            Some((turn_id, &item.item_id))
                        }
                        _ => None,
                    };
                    progress != Some((turn, item_id))
                });
            }
            active.events.push(envelope.clone());
        }
        self.emit(StreamEvent::TurnEvent {
            session_id: session_id.to_string(),
            payload: envelope,
        });
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
        // 发布结算前释放操作预订；新操作的开始投影等待此锁。
        drop(reservation);
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: SessionSettledPayload {
                runtime: slot.runtime_from(&state),
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
                // 事件回调只在本 worker 内同步调用，直接借用所有者，不再复制句柄。
                let mut event_sink = |event| workbench.on_turn_event(&session_id, &slot, event);
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
        self.publish_session_locked(session_id, slot, &mut state)
    }

    /// 发布完整工作台快照。快照构造失败不推翻任何已提交的操作结果：
    /// 读侧无法展示时经重同步通道要求客户端重新拉取基线。
    fn publish_workbench_snapshot(&self) {
        let _publication = self.lock_workbench_publication();
        self.publish_workbench_result(self.bootstrap());
    }

    fn publish_workbench_result(&self, snapshot: Result<WorkbenchBootstrap, RpcError>) {
        self.emit(match snapshot {
            Ok(payload) => StreamEvent::WorkbenchChanged { payload },
            Err(error) => StreamEvent::ResyncRequired {
                payload: ResyncRequiredPayload {
                    reason: format!("snapshot_unavailable: {error:?}"),
                },
            },
        });
    }

    /// 完整替换快照必须在同一发布临界区内构造并取得流序号；否则较早构造的
    /// payload 可以在较新快照之后获得更高 revision。该锁不参与会话事件发布，
    /// 避免形成全局发布锁 → SlotState 的反向锁序。
    #[allow(clippy::expect_used)]
    fn lock_workbench_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.workbench_publication
            .lock()
            .expect("workbench publication lock poisoned")
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

    fn runtime_from(&self, state: &SlotState) -> SessionRuntime {
        // 会话侧字段来自同一次读取；Slot 自己的状态仍由本方法补充。
        let conversation = self.conversation.snapshot();
        SessionRuntime {
            session_revision: state.session_revision,
            phase: conversation.phase,
            selector: conversation.selector,
            model_context_window: conversation.model_context_window,
            pending_controls: conversation.pending_controls,
            active_turn: state
                .active_turn
                .as_ref()
                .map(|active| ActiveTurnRuntimeSnapshot {
                    turn_id: active.turn_id.clone(),
                    started_at: active.started_at.clone(),
                }),
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
    match error.code.as_deref() {
        Some(singularity_model::CREDENTIAL_SAVE_FAILED_CODE) => {
            partially_saved(error, "重试保存 API 密钥。")
        }
        Some(singularity_model::CREDENTIAL_DELETE_FAILED_CODE) => {
            partially_saved(error, "重试删除 API 密钥。")
        }
        _ => configuration_error(error.to_string()),
    }
}

/// 配置已部分生效、剩余凭据写入失败：界面按同一分类给出重试该操作的引导。
fn partially_saved(error: singularity_model::ProviderError, recovery: &str) -> RpcError {
    RpcError::new(
        RpcErrorCode::ConfigurationPartiallySaved,
        error.to_string(),
        recovery,
    )
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
        ConversationError::TurnAlreadyActive => session_busy(),
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
        CatalogError::WriterActive => session_busy(),
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

fn session_busy() -> RpcError {
    RpcError::new(
        RpcErrorCode::SessionBusy,
        "当前任务正在处理另一项操作。",
        "等待状态变为空闲，或使用当前阶段提供的控制动作。",
    )
}

/// 会话仍在执行或仍有待处理输入：影响会话归属的两个事实取自同一次读取。
fn session_occupied(conversation: &singularity_runtime::Conversation) -> bool {
    let snapshot = conversation.snapshot();
    snapshot.phase != SessionPhase::Idle || !snapshot.pending_controls.is_empty()
}

fn control_error(error: ConversationControlError) -> RpcError {
    match error {
        ConversationControlError::NotRunning => session_busy(),
        ConversationControlError::InvalidInput => invalid_request("输入不能为空。"),
        ConversationControlError::ControlNotFound => RpcError::new(
            RpcErrorCode::ControlNotFound,
            "待处理输入已不存在或已经开始执行。",
            "刷新任务后确认待处理输入队列。",
        ),
    }
}

#[cfg(test)]
mod tests;
