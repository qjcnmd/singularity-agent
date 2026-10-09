//! 本地工作台的深模块：Workspace、Session、模型设置和运行态只在这一层组合。
//! 工作区登记、目录查询和分组投影在 `workspace` 子模块；单会话快照、活动事件折叠和
//! 终态归并在 `session` 子模块；本模块负责装配、会话查找、操作启动、发布和全局事件顺序。

mod actions;
mod errors;
mod session;
mod shutdown;
mod workspace;

pub(super) use self::errors::invalid_request;
use self::errors::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use singularity_core::now_iso;
use singularity_model::{ModelConfigManager, ModelConfigUpdate};
use singularity_protocol::{
    AppBootstrap, ProviderConfigurationInput, RpcError, RpcErrorCode, SessionReadResult,
    SessionTerminalSnapshot, SessionTerminalSource, StreamEnvelope, StreamEvent, TurnEvent, TurnStatus,
};
use singularity_runtime::UserInput;
use singularity_runtime::{
    CatalogError, Conversation, ConversationControlError, ConversationError, FollowUpPromotion,
    OperationGuard, OperationReservation, OperationResult, ThreadCatalog, TurnRunner,
};
use tokio::sync::broadcast;

use super::workspace_store::{WorkspaceError, WorkspaceStore};
use session::{ConversationSlot, SlotState};

const STREAM_CAPACITY: usize = 512;

/// 首次提交的 RPC 在对应用户消息落盘后才成功；尚未保存的失败直接交回提交方。
struct SubmissionReceipt {
    item_id: String,
    sender: std::sync::mpsc::SyncSender<Result<(), RpcError>>,
}

pub struct AppServer {
    revision: Mutex<u64>,
    /// 管住完整工作台快照的构造和发布顺序；不插手会话执行，也不管普通增量事件。
    app_publication: Mutex<()>,
    /// 会话生命周期临界区：把「查找或创建 slot → 接受输入/建立预订」
    /// 和「占用检查 → 持久变更 → 注销」放进同一个短临界区，销毁操作就插不进启动占用到
    /// 写者打开之间；它只保护这几步短操作，不会横跨模型请求、工具执行或整个任务。
    lifecycle: Mutex<()>,
    runner: Arc<TurnRunner>,
    runtime_handle: tokio::runtime::Handle,
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
        runtime_handle: tokio::runtime::Handle,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<ModelConfigManager>,
        mcp: Arc<singularity_mcp::McpManager>,
        home: std::path::PathBuf,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            revision: Mutex::new(0),
            app_publication: Mutex::new(()),
            lifecycle: Mutex::new(()),
            runner,
            runtime_handle,
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
        update: impl FnOnce(&ModelConfigManager) -> ModelConfigUpdate,
    ) -> Result<(), RpcError> {
        let _publication = self.lock_app_publication();
        let ModelConfigUpdate { result, catalog } = update(&self.models);
        self.publish_app_result(self.bootstrap_with_catalog(catalog));
        result.map_err(model_error)
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
        api_protocol: &str,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, RpcError> {
        // 配置入口返回本次查询使用的凭据，网络等待不占配置的临界区。
        let api_key =
            self.models.discovery_credential(provider_id, api_key).map_err(model_discovery_error)?;
        singularity_model::discover_models(base_url, &api_key, api_protocol)
            .await
            .map_err(model_discovery_error)
    }

    /// 创建并登记任务，返回包含首次历史读取的运行态快照。
    pub fn create_session(&self, workspace_id: &str) -> Result<SessionReadResult, RpcError> {
        // 创建、登记和首次读取都在同一个生命周期临界区里做完：新会话不能在
        // 登记和读取之间被归档或移除。
        let lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(workspace_id)?;
        let selector = self.models.snapshot().resolved_default_selector();
        let thread = self.catalog.create_thread(&workspace.root, selector).map_err(catalog_error)?;
        let slot = self.insert_slot(thread);
        let result = self.read_from_slot(&slot, 100, None)?;
        drop(lifecycle);
        self.publish_app_snapshot();
        Ok(result)
    }

    pub fn read_session(
        &self,
        session_id: &str,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        // 只有「查找或创建 slot」这一段算生命周期交接；整份历史的读盘不占这个临界区。
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
        let thread = self.catalog.resume_thread(session_id).map_err(catalog_error)?;
        Ok(self.insert_slot(thread))
    }

    /// 在生命周期临界区内登记新 slot；已有任务由 open_slot 在创建前返回。
    fn insert_slot(&self, thread: singularity_protocol::Thread) -> Arc<ConversationSlot> {
        let id = thread.thread_id.clone();
        let conversation = Conversation::new(Arc::clone(&self.runner), thread);
        let slot = Arc::new(ConversationSlot::new(conversation));
        self.lock_sessions().insert(id, Arc::clone(&slot));
        slot
    }

    /// history、活动事件和运行态在同一把 slot 锁内读取，冷路径的读盘也不例外，回合开始
    /// 和结算也在同一把锁里提交，所以锁内读到的三样必属同一个瞬间，不用比对 revision
    /// 重新取样；代价是读盘期间 worker 的事件投影要等这次读盘做完。
    ///
    /// 读取只取投影：它不建立、不修复，也不清空活动回合。
    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        let state = slot.lock_state();
        let history = match state.frozen_history() {
            // 回合进行中（含结算前的重复读取）：内存里冻结的 history 就是当前状态，不用读盘。
            Some(history) => history,
            None => self.read_persisted_history(slot)?,
        };
        let runtime = slot.runtime_from(&state);
        let active_events = state.active_events().to_vec();
        drop(state);
        let history = history.page(limit, before_turn).map_err(catalog_error)?;
        Ok(SessionReadResult { history, runtime, active_events })
    }

    /// 读取最新的持久化 history。启动路径在 slot 锁外调用：预订成立时上一个
    /// worker 已经结算完，读盘不会和事件投影抢；会话读取路径则在它自己那把锁里调用。
    fn read_persisted_history(
        &self,
        slot: &ConversationSlot,
    ) -> Result<Arc<singularity_runtime::ThreadSnapshot>, RpcError> {
        self.catalog.read_snapshot(&slot.conversation().thread().thread_id).map_err(catalog_error)
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        // 控制处置的变化一律归到会话快照发布：控制事实只有会话快照这一种表示，
        // 不进入活动 turn 的事件序列。
        if let TurnEvent::ControlChanged { .. } = &event {
            self.bump_and_emit_session(session_id, slot);
            return;
        }
        // 单会话的投影（活动回合、进度替换、revision）由 slot 做；本层在同一把锁里
        // 拿到 envelope 就广播，折叠和广播之间插不进别的事件。
        let mut state = slot.lock_state();
        let envelope = state.apply_turn_event(event);
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
        guard: OperationGuard,
    ) {
        let mut state = slot.lock_state();
        // 保存本次执行结果或错误反馈；后续历史读取失败不会覆盖它。清空冻结的
        // history 后，下一次会话读取会重新尝试读盘并单独呈现读取错误。
        state.settle(terminal);
        // 发布结算之前释放占用守卫；新操作的开始投影要等这把锁。
        drop(guard);
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(&state),
        });
    }

    /// 将执行交给 Tokio task；占用守卫由 task 持有直到投影结算。
    fn spawn_operation(
        self: &Arc<Self>,
        session_id: &str,
        slot: Arc<ConversationSlot>,
        reservation: OperationReservation,
        mut receipt: Option<SubmissionReceipt>,
    ) {
        let app_server = Arc::clone(self);
        let session_id = session_id.to_string();
        self.runtime_handle.spawn(async move {
            let mut event_sink = |event| {
                let committed = matches!(&event, TurnEvent::UserMessage { item, .. }
                    if receipt.as_ref().is_some_and(|receipt| receipt.item_id == item.item_id));
                app_server.on_turn_event(&session_id, &slot, event);
                if committed {
                    let _ = receipt.take().expect("matched submitted message").sender.send(Ok(()));
                }
            };
            let (result, guard) = reservation.execute(&mut event_sink).await;
            let uncommitted = receipt.map(|receipt| {
                let OperationResult::Turn(Err(error)) = &result else {
                    unreachable!("an uncommitted submitted input must fail execution");
                };
                (receipt.sender, internal_error(error.to_string()))
            });
            let terminal = match result {
                OperationResult::Turn(result) => Some(turn_terminal(result)),
                OperationResult::Compaction(result) => match result {
                    Ok(_) => None,
                    Err(error) => Some(SessionTerminalSnapshot {
                        source: SessionTerminalSource::Compaction,
                        status: TurnStatus::Failed,
                        manually_stopped: false,
                        message: Some(error.to_string()),
                    }),
                },
            };
            app_server.on_session_settled(&session_id, &slot, terminal, guard);
            if let Some((sender, error)) = uncommitted {
                let _ = sender.send(Err(error));
            }
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
        self.publish_app_result(self.bootstrap_with_catalog(self.models.redacted_catalog()));
    }

    fn publish_app_result(&self, snapshot: Result<AppBootstrap, RpcError>) {
        match snapshot {
            Ok(payload) => {
                self.emit(StreamEvent::AppChanged { payload });
            }
            // 操作已保存，RPC 仍返回其真实结果；基线重读失败由客户端已有的
            // 列表错误状态呈现，避免无提示地保留旧列表。
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

    /// 发布一个流事件：全局流序号在这里推进，随 StreamEnvelope 一起交给消费者，
    /// 不靠函数返回值沿调用链往回传。
    fn emit(&self, event: StreamEvent) {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let _ = self.stream.send(StreamEnvelope { revision: *order, event });
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ConversationSlot>>> {
        self.sessions.lock().expect("app_server session map lock poisoned (fail-stop)")
    }
}
