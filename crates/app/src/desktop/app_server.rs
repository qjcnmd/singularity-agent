//! 本地工作台的深模块：Workspace、Session、模型设置和运行态只在这一层组合。
//! 工作区登记、目录查询和分组投影在 `workspace` 子模块；单会话快照、活动事件折叠和
//! 终态归并在 `session` 子模块；本模块负责装配、会话查找、操作启动、发布和全局事件顺序。

mod actions;
mod activity;
mod background;
mod errors;
mod session;
mod shutdown;
mod workspace;

pub(super) use self::errors::invalid_request;
use self::errors::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use singularity_model::{ModelConfigManager, ProviderError};
use singularity_protocol::{
    AppBootstrap, ProviderApiProtocol, ProviderConfigurationInput, RpcError, RpcErrorCode, SessionReadResult,
    SessionTerminalSnapshot, SessionTerminalSource, StreamEnvelope, StreamEvent, TurnEvent, TurnStatus,
};
use singularity_runtime::UserInput;
use singularity_runtime::{
    CatalogError, Conversation, ConversationError, OperationReservation, OperationResult, ThreadCatalog,
    TurnRunner,
};
use tokio::sync::broadcast;

use super::workspace_store::{WorkspaceError, WorkspaceStore};
use session::{ConversationSlot, SlotState};

const STREAM_CAPACITY: usize = 512;

/// 提交 RPC 在消息入队后返回；后续准备、保存和执行状态通过会话事件发布。
pub struct AppServer {
    weak: std::sync::Weak<Self>,
    revision: Mutex<u64>,
    /// 管住完整工作台快照的构造和发布顺序；不插手会话执行，也不管普通增量事件。
    app_publication: Mutex<()>,
    /// 会话生命周期临界区：把「查找或创建 slot → 接受输入/建立预订」
    /// 和「占用检查 → 持久变更 → 注销」放进同一个短临界区，销毁操作就插不进启动占用到
    /// 写者打开之间；它只保护这几步短操作，不会横跨模型请求、工具执行或整个任务。
    lifecycle: Mutex<()>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    /// 和 runner 共用的磁盘配置入口；每次读取都在短临界区里完成。
    models: Arc<ModelConfigManager>,
    pub(super) mcp: Arc<singularity_mcp::McpManager>,
    /// 应用主目录：技能发现这类宿主查询和执行链读的是同一个事实。
    home: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Arc<ConversationSlot>>>,
    stream: broadcast::Sender<StreamEnvelope>,
}

impl AppServer {
    pub fn new(
        runner: Arc<TurnRunner>,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<ModelConfigManager>,
        mcp: Arc<singularity_mcp::McpManager>,
        home: std::path::PathBuf,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            revision: Mutex::new(0),
            app_publication: Mutex::new(()),
            lifecycle: Mutex::new(()),
            runner,
            catalog,
            workspaces,
            models,
            mcp,
            home,
            sessions: Mutex::new(HashMap::new()),
            stream,
        })
    }

    pub fn revision(&self) -> u64 {
        *self.revision.lock().expect("stream revision lock poisoned")
    }

    pub(super) fn mcp_directory(&self, workspace: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
        workspace
            .map(|id| self.workspace(id).map(|workspace| workspace.root.into()))
            .unwrap_or_else(|| Ok(self.home.clone()))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamEnvelope> {
        self.stream.subscribe()
    }

    pub fn frame(&self, event: StreamEvent) -> StreamEnvelope {
        StreamEnvelope { revision: self.revision(), event }
    }

    pub fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), RpcError> {
        self.update_models(|models| models.save_provider(provider, api_key))
    }

    pub fn remove_provider(&self, provider_id: &str) -> Result<(), RpcError> {
        self.update_models(|models| models.remove_provider(provider_id))
    }

    fn update_models(
        &self,
        update: impl FnOnce(&ModelConfigManager) -> Result<(), ProviderError>,
    ) -> Result<(), RpcError> {
        let _publication = self.lock_app_publication();
        let result = update(&self.models);
        self.publish_app_snapshot_locked();
        result.map_err(model_error)
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
        api_protocol: ProviderApiProtocol,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, RpcError> {
        // 配置入口返回本次查询使用的凭据，网络等待不占配置的临界区。
        let api_key =
            self.models.discovery_credential(provider_id, api_key).map_err(model_discovery_error)?;
        singularity_model::discover_models(base_url, &api_key, api_protocol)
            .await
            .map_err(model_discovery_error)
    }

    /// 收到第一条消息才建立会话；空白页没有后端身份或日志文件。
    pub fn start_session(
        self: &Arc<Self>,
        workspace_id: &str,
        selector: Option<String>,
        input: UserInput,
    ) -> Result<singularity_protocol::SessionStartResult, RpcError> {
        let lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(workspace_id)?;
        let selector = selector.or_else(|| self.models.snapshot().resolved_default_selector());
        let thread = self.catalog.create_thread(&workspace.root, selector).map_err(catalog_error)?;
        let slot = self.insert_slot(thread);
        let id = slot.conversation().thread().thread_id;
        // 先完成目录读取，再接受输入和预订执行。
        let mut bootstrap = self.bootstrap()?;
        let reservation = slot
            .conversation()
            .submit(&input)
            .map_err(conversation_error)?
            .expect("new session reserves its first turn");
        slot.lock_state().begin_turn(slot.conversation().usage());
        bootstrap.session_phases.insert(id.clone(), slot.conversation().phase());
        drop(lifecycle);
        self.emit(StreamEvent::AppChanged { payload: bootstrap.clone() });
        self.spawn_operation(&id, slot, reservation);
        Ok(singularity_protocol::SessionStartResult { session_id: id, bootstrap })
    }

    pub fn read_session(
        &self,
        session_id: &str,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        // 只有「查找或创建 slot」这一段算生命周期交接；历史投影不占这个临界区。
        let slot = {
            let _lifecycle = self.lock_lifecycle();
            self.open_slot(session_id)?
        };
        self.read_from_slot(&slot, limit, before_turn)
    }

    fn open_slot(&self, session_id: &str) -> Result<Arc<ConversationSlot>, RpcError> {
        // 全局 map 锁只管查找；恢复未打开任务的读盘在锁外完成。
        let open = self.lock_sessions().get(session_id).cloned();
        if let Some(slot) = open {
            return Ok(slot);
        }
        // 首次打开恢复未结束的操作，并登记会话。
        let thread = self.catalog.open_session(session_id).map_err(catalog_error)?;
        Ok(self.insert_slot(thread))
    }

    /// 生命周期锁内检查操作准入；归档等待只占用目标任务，不阻塞读取或其他任务。
    fn open_action_slot(&self, session_id: &str) -> Result<Arc<ConversationSlot>, RpcError> {
        let slot = self.open_slot(session_id)?;
        if slot.is_archiving() {
            return Err(session_busy());
        }
        Ok(slot)
    }

    /// 在生命周期临界区内登记新 slot；已有任务由 open_slot 在创建前返回。
    fn insert_slot(&self, session: singularity_runtime::SessionManager) -> Arc<ConversationSlot> {
        let id = session.session_id().to_owned();
        let (conversation, updates) = Conversation::new(Arc::clone(&self.runner), session);
        let slot = Arc::new(ConversationSlot::new(conversation));
        self.lock_sessions().insert(id.clone(), Arc::clone(&slot));
        self.listen_background(id, Arc::clone(&slot), updates);
        slot
    }

    /// 在同一把 slot 锁内读取执行快照和后续事件，避免读取跨过操作结算。
    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        let state = slot.lock_state();
        let snapshot = slot.conversation().history();
        let mut runtime = slot.runtime_from(&state);
        runtime.usage = Some(snapshot.summary.usage.clone());
        let mut history = snapshot.page(limit, before_turn).map_err(catalog_error)?;
        if before_turn.is_none() {
            state.merge_activity(&mut history);
        }
        Ok(SessionReadResult { history, runtime })
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        // 提问和压缩阶段由会话快照表示，不进入活动 turn 的条目序列。
        if let TurnEvent::SessionStateChanged { .. } = &event {
            self.bump_and_emit_session(session_id, slot);
            return;
        }
        // 单会话的投影（活动回合、进度替换、revision）由 slot 做；本层在同一把锁里
        // 拿到 envelope 就广播，折叠和广播之间插不进别的事件。
        let mut state = slot.lock_state();
        let envelope = state.apply_turn_event(event, slot.conversation().phase());
        self.emit(StreamEvent::TurnEvent {
            session_id: session_id.to_string(),
            payload: Box::new(envelope),
        });
    }

    fn on_session_settled(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        terminal: Option<SessionTerminalSnapshot>,
    ) {
        let mut state = slot.lock_state();
        // 发布结果前清空当前操作内容；下一次读取直接使用保存记录。
        state.settle(terminal);
        // 结算和新提交共用这把锁，当前操作释放后才能开始下一轮。
        slot.conversation().finish_operation();
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(&state),
        });
    }

    /// 交给 Tokio 执行，完成后统一结算并处理待运行输入。
    fn spawn_operation(
        self: &Arc<Self>,
        session_id: &str,
        slot: Arc<ConversationSlot>,
        reservation: OperationReservation,
    ) {
        let app_server = Arc::clone(self);
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            let mut event_sink = |event| app_server.on_turn_event(&session_id, &slot, event);
            let result = reservation.execute(&mut event_sink).await;
            let terminal = match result {
                OperationResult::Turn(result) => Some(turn_terminal(result)),
                OperationResult::Compaction(result) => match result {
                    Ok(()) => None,
                    Err(error) => Some(SessionTerminalSnapshot {
                        source: SessionTerminalSource::Compaction,
                        status: TurnStatus::Failed,
                        manually_stopped: false,
                        error: Some(error.detail()),
                    }),
                },
            };
            if terminal.as_ref().is_some_and(|terminal| terminal.status == TurnStatus::Failed) {
                slot.conversation().pause_background_resume();
            }
            app_server.on_session_settled(&session_id, &slot, terminal);
            app_server.start_background_turn(&session_id, &slot);
        });
    }

    fn bump_and_emit_session(&self, session_id: &str, slot: &ConversationSlot) {
        let mut state = slot.lock_state();
        self.publish_session_locked(session_id, slot, &mut state);
    }

    /// 发布完整的工作台快照；构造失败不推翻已保存的操作结果。
    /// 生命周期变更提交并释放其锁后调用，目录读盘期间其他任务仍可接受控制。
    fn publish_app_snapshot(&self) {
        let _publication = self.lock_app_publication();
        self.publish_app_snapshot_locked();
    }

    /// 配置修改已持有发布锁；失败后也重读实际目录，避免界面保留过时设置。
    fn publish_app_snapshot_locked(&self) {
        self.publish_app_result(self.bootstrap_with_catalog(self.models.redacted_catalog()));
    }

    fn publish_app_result(&self, snapshot: Result<AppBootstrap, RpcError>) {
        match snapshot {
            Ok(payload) => {
                self.emit(StreamEvent::AppChanged { payload });
            }
            // 操作已保存，RPC 仍返回其真实结果；基线重读失败交给客户端已有的
            // 列表错误状态呈现，不静默留旧列表。
            Err(_) => self.emit(StreamEvent::ResyncRequired),
        }
    }

    /// RPC 读取与完整快照发布都在同一临界区里构造并取得流序号；否则先读取的
    /// payload 可能在更新的快照之后拿到更高的 revision。这把锁不参与会话事件
    /// 发布，免得形成「全局发布锁 → SlotState」的反向锁序。
    fn lock_app_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.app_publication.lock().expect("app_server publication lock poisoned")
    }

    /// 会话生命周期临界区。锁序是 lifecycle → publication → sessions →
    /// SlotState；调用方只在本文件公开入口的最外层拿它。
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle.lock().expect("app_server lifecycle lock poisoned")
    }

    /// 发布一个流事件：全局流序号在这里推进，随 StreamEnvelope 一起交给消费者。
    fn emit(&self, event: StreamEvent) {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let _ = self.stream.send(StreamEnvelope { revision: *order, event });
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ConversationSlot>>> {
        self.sessions.lock().expect("app_server session map lock poisoned (fail-stop)")
    }
}
