//! 本地工作台的深模块：Workspace、Session、模型设置和运行态只在这一层组合。
//!
//! 工作区登记、目录查询和分组投影收在 `workspace` 子模块；单会话快照、活动
//! 事件折叠和终态归并收在 `session` 子模块；本模块留下装配、查找和范围检查、
//! 操作启动、发布入口以及全局事件顺序。

mod actions;
mod errors;
mod session;
mod workspace;

pub(super) use self::errors::invalid_request;
use self::errors::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use singularity_core::now_iso;
use singularity_model::ModelConfigManager;
use singularity_protocol::{
    AppBootstrap, EmptyParams, PROTOCOL_VERSION, ProviderConfigurationInput, RpcError,
    RpcErrorCode, SessionPhase, SessionReadResult, SessionTerminalSnapshot, SessionTerminalSource,
    StreamEnvelope, StreamEvent, TurnEvent, TurnStatus,
};
use singularity_runtime::{
    CatalogError, Conversation, ConversationControlError, ConversationError, FollowUpPromotion,
    ThreadCatalog, TurnReservation, TurnRunner, WorkspaceError, WorkspaceStore,
};
use tokio::sync::broadcast;
use uuid::Uuid;

use session::{ConversationSlot, SlotState};
use workspace::verify_workspace_thread;

const STREAM_CAPACITY: usize = 512;

enum Operation {
    Turn(String),
    Promoted,
    Compaction,
}

pub struct AppServer {
    generation: String,
    revision: Mutex<u64>,
    /// 管住完整工作台快照的构造和发布顺序；不插手会话执行，也不管普通增量事件。
    app_publication: Mutex<()>,
    /// 会话生命周期临界区：把「范围/成员校验 → 查找或创建 slot → 接受输入/建立预订」
    /// 和「占用检查 → 持久变更 → 注销」放进同一个短临界区，销毁操作就插不进启动占用到
    /// 写者打开之间；它只保护这几步短操作，绝不横跨模型请求、工具执行或整个任务。
    lifecycle: Mutex<()>,
    runner: Arc<TurnRunner>,
    runtime_handle: tokio::runtime::Handle,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    /// 和 runner 共用的磁盘配置入口；每次读取都在短临界区里完成。
    models: Arc<Mutex<ModelConfigManager>>,
    /// 应用主目录：技能发现这类宿主查询和执行链读的是同一个事实。
    home: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Arc<ConversationSlot>>>,
    stream: broadcast::Sender<StreamEnvelope>,
    /// 测试注入点：未打开任务的目录读盘开始前调用一次。
    #[cfg(test)]
    directory_read_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// 测试注入点：归档在占用检查之后、持久变更之前调用一次，用来构造交错。
    #[cfg(test)]
    archive_check_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl AppServer {
    pub fn new(
        runner: Arc<TurnRunner>,
        runtime_handle: tokio::runtime::Handle,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<Mutex<ModelConfigManager>>,
        home: std::path::PathBuf,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            generation: Uuid::new_v4().to_string(),
            revision: Mutex::new(0),
            app_publication: Mutex::new(()),
            lifecycle: Mutex::new(()),
            runner,
            runtime_handle,
            catalog,
            workspaces,
            models,
            home,
            sessions: Mutex::new(HashMap::new()),
            stream,
            #[cfg(test)]
            directory_read_pause: Mutex::new(None),
            #[cfg(test)]
            archive_check_pause: Mutex::new(None),
        })
    }

    #[allow(clippy::expect_used)]
    pub fn revision(&self) -> u64 {
        *self.revision.lock().expect("stream revision lock poisoned")
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StreamEnvelope> {
        self.stream.subscribe()
    }

    pub fn frame(&self, event: StreamEvent) -> StreamEnvelope {
        self.envelope(self.revision(), event)
    }

    fn envelope(&self, revision: u64, event: StreamEvent) -> StreamEnvelope {
        StreamEnvelope {
            version: PROTOCOL_VERSION,
            generation: self.generation.clone(),
            revision,
            event,
        }
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
        update: impl FnOnce(&mut ModelConfigManager) -> Result<(), singularity_model::ProviderError>,
    ) -> Result<(), RpcError> {
        let _publication = self.lock_app_publication();
        let mut models = self.lock_models();
        let result = update(&mut models).map_err(model_error);
        // 配置和凭据是两个独立文件。第二次写入失败时，不能让后面的 turn 继续用旧配置的
        // 快照；配置入口是共享的、会重新读文件，所以其他对象不用自己刷新。
        let catalog = models.redacted_catalog();
        drop(models);
        self.publish_app_result(self.bootstrap_with_catalog(catalog));
        result
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, RpcError> {
        // 只在配置锁内解析这次查询要用的凭据（显式传入的优先，否则回退到已
        // 存储的 key）；URL 解析、请求构造和发送都在发现实现里一次做完。
        let api_key = {
            self.lock_models()
                .discovery_credential(provider_id, api_key)
                .map_err(model_discovery_error)?
        };
        singularity_model::discover_models(base_url, &api_key)
            .await
            .map_err(model_discovery_error)
    }

    pub fn create_session(&self, workspace_id: &str) -> Result<SessionReadResult, RpcError> {
        // 创建、登记和首次读取都在同一个生命周期临界区里做完：新会话不能在
        // 登记和读取之间被归档或移除。
        let _lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(workspace_id)?;
        let selector = self.default_model_selector();
        // 没有任何可用模型时跳过校验，任务仍可先建出来。
        if selector.is_some() {
            self.validate_model_selector(selector.as_deref())?;
        }
        let thread = self
            .catalog
            .create_thread(&workspace.root, selector)
            .map_err(catalog_error)?;
        let slot = self.insert_slot(thread);
        let result = self.read_from_slot(&slot, 100, None)?;
        self.publish_app_snapshot();
        Ok(result)
    }

    pub fn read_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        workspace::page_limit(limit)?;
        // 只有「查找或创建 slot」这一段算生命周期交接；整份历史的读盘不占这个临界区。
        let slot = {
            let _lifecycle = self.lock_lifecycle();
            self.open_slot(workspace_id, session_id)?
        };
        self.read_from_slot(&slot, limit, before_turn)
    }

    fn open_slot(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Arc<ConversationSlot>, RpcError> {
        // 全局 map 锁只管这一次查找：范围校验要读工作区，恢复未打开的任务还要
        // 读盘，这两件事都不能在持锁期间做。
        let workspace = self.workspace(workspace_id)?;
        let open = self.lock_sessions().get(session_id).cloned();
        if let Some(slot) = open {
            verify_workspace_thread(&workspace, &slot.conversation().thread().cwd)?;
            return Ok(slot);
        }
        // 未打开的任务把期望目录交给恢复路径：校验发生在会话头部解析之后、任何
        // 重写或修复之前，所以传错工作区也不会改动目标文件。
        let thread = self
            .catalog
            .resume_thread(session_id, &workspace.root)
            .map_err(catalog_error)?;
        Ok(self.insert_slot(thread))
    }

    /// 建好 slot 并登记到全局 map；同一会话已经打开时就复用原来那个。
    fn insert_slot(&self, thread: singularity_protocol::Thread) -> Arc<ConversationSlot> {
        let session_id = thread.thread_id.clone();
        let conversation = Conversation::new(Arc::clone(&self.runner), thread);
        let slot = Arc::new(ConversationSlot::new(conversation));
        self.lock_sessions()
            .entry(session_id)
            .or_insert_with(|| Arc::clone(&slot))
            .clone()
    }

    /// 一次会话读取：history、活动事件和运行态来自同一份受保护状态。整段读取都在同一把
    /// slot 锁里完成，冷路径的读盘也不例外；回合开始和结算也在同一把锁里提交，所以锁内
    /// 读到的三样必然属于同一个瞬间，不用再比对 revision 重新取样。代价是读盘期间 worker
    /// 的事件投影要等这次读盘做完。
    ///
    /// 读取只取投影：它不建立、不修复，也不清空活动回合。
    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        let state = slot.lock_state();
        let capture = match state.frozen_history() {
            // 回合进行中（含结算前的重复读取）：内存里冻结的 history 就是当前状态，不用读盘。
            Some(history) => slot.capture(&state, history),
            // 冷路径内存里没有终态（slot 刚建立或宿主重启过）：最近一次独立压缩的失败或
            // 中断就是当前的操作反馈，从同一份持久快照里恢复，热读和冷读才会一致。
            None => {
                let history = self.read_persisted_history(slot)?;
                let mut capture = slot.capture(&state, history);
                if capture.runtime.terminal.is_none() {
                    capture.runtime.terminal = capture.history.terminal.clone();
                }
                capture
            }
        };
        drop(state);
        let history = capture
            .history
            .page(limit, before_turn)
            .map_err(catalog_error)?;
        Ok(SessionReadResult {
            history,
            runtime: capture.runtime,
            active_events: capture.active_events,
        })
    }

    /// 测试互锁：让未打开任务的目录读盘停在会话 map 锁之外，供并发用例确定性地观察 map 访问。
    #[cfg(test)]
    fn run_directory_read_pause(&self) {
        take_pause(&self.directory_read_pause);
    }

    /// 读取最新的持久化 history。启动路径在 slot 锁外调用：预订成立时上一个
    /// worker 已经结算完，读盘不会和事件投影抢；会话读取路径则在它自己那把锁里调用。
    fn read_persisted_history(
        &self,
        slot: &ConversationSlot,
    ) -> Result<Arc<singularity_runtime::ThreadSnapshot>, RpcError> {
        self.catalog
            .read_snapshot(&slot.conversation().thread().thread_id)
            .map_err(catalog_error)
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
        // 终态来自执行链的可信提交：历史读取失败也改不了它，读取错误由现有的
        // 会话读取路径单独呈现（history 被置空，下一次读取必然重试）。
        state.settle(terminal);
        // 发布结算之前先放掉操作预订；新操作的开始投影要等这把锁。
        drop(reservation);
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(&state),
        });
    }

    /// 发布一次结算。结算本身也可能因为共享状态中毒而失败：这时没有可发布的会话投影，
    /// 但也不能把界面晾在「仍在运行」——走原有的重同步通道，不伪造终态，也不另建恢复状态。
    fn settle_operation(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        terminal: Option<SessionTerminalSnapshot>,
        reservation: TurnReservation,
    ) {
        let settled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_session_settled(session_id, slot, terminal, reservation);
        }));
        if settled.is_err() {
            self.require_resync();
        }
    }

    /// 将执行交给 Tokio task；预订由 task 持有直到投影结算。
    fn spawn_operation(
        self: &Arc<Self>,
        session_id: &str,
        slot: Arc<ConversationSlot>,
        mut reservation: TurnReservation,
        operation: Operation,
    ) {
        let app_server = Arc::clone(self);
        let session_id = session_id.to_string();
        self.runtime_handle.spawn(async move {
            let outcome = std::panic::AssertUnwindSafe(async {
                let mut event_sink = |event| app_server.on_turn_event(&session_id, &slot, event);
                match operation {
                    Operation::Turn(text) => {
                        Some(turn_terminal(reservation.run(&text, &mut event_sink).await))
                    }
                    Operation::Promoted => Some(turn_terminal(
                        reservation.run_promoted(&mut event_sink).await,
                    )),
                    Operation::Compaction => match reservation.compact().await {
                        Ok(outcome) => outcome.terminal(),
                        Err(error) => Some(SessionTerminalSnapshot {
                            source: SessionTerminalSource::Compaction,
                            status: TurnStatus::Failed,
                            message: Some(error.to_string()),
                        }),
                    },
                }
            })
            .catch_unwind()
            .await;
            let terminal = match outcome {
                Ok(terminal) => terminal,
                Err(payload) => {
                    // 宿主故障：先按原有交还规则，把本轮已接受但没交付的输入还回去，
                    // 再拿真实原因结算显示投影。显示投影不是持久账本，所以这里
                    // 不声称执行链已经提交了可信终态。
                    let abandoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        slot.conversation().abandon_turn();
                    }));
                    if abandoned.is_err() {
                        // 交还做不完（状态已中毒），同样只能让客户端重拉基线。
                        app_server.require_resync();
                    }
                    Some(SessionTerminalSnapshot {
                        source: SessionTerminalSource::Turn,
                        status: TurnStatus::Failed,
                        message: Some(format!(
                            "任务执行异常，已停止：{}",
                            singularity_core::panic_message(payload.as_ref())
                        )),
                    })
                }
            };
            app_server.settle_operation(&session_id, &slot, terminal, reservation);
        });
    }

    fn bump_and_emit_session(&self, session_id: &str, slot: &ConversationSlot) {
        let mut state = slot.lock_state();
        self.publish_session_locked(session_id, slot, &mut state);
    }

    /// 发布完整的工作台快照；构造失败不推翻任何已提交的操作结果，只让客户端重拉基线。
    fn publish_app_snapshot(&self) {
        let _publication = self.lock_app_publication();
        self.publish_app_result(self.bootstrap());
    }

    /// 读侧没法继续用增量同步时，让客户端重拉基线；不改动任何已提交的结果。
    fn require_resync(&self) {
        self.emit(StreamEvent::ResyncRequired {
            payload: EmptyParams {},
        });
    }

    fn publish_app_result(&self, snapshot: Result<AppBootstrap, RpcError>) {
        match snapshot {
            Ok(payload) => {
                self.emit(StreamEvent::AppChanged { payload });
            }
            Err(_) => self.require_resync(),
        }
    }

    /// 完整替换快照必须在同一个发布临界区里构造并取得流序号；否则先构造的
    /// payload 可能在更新的快照之后拿到更高的 revision。这把锁不参与会话事件
    /// 发布，免得形成「全局发布锁 → SlotState」的反向锁序。
    #[allow(clippy::expect_used)]
    fn lock_app_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.app_publication
            .lock()
            .expect("app_server publication lock poisoned")
    }

    /// 会话生命周期临界区。锁序是 lifecycle → publication → sessions →
    /// SlotState；调用方只在本文件公开入口的最外层拿它。
    #[allow(clippy::expect_used)]
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .expect("app_server lifecycle lock poisoned")
    }

    /// 发布一个流事件：全局流序号在这里推进，随 StreamEnvelope 一起交给消费者，
    /// 不靠函数返回值沿调用链往回传。
    #[allow(clippy::expect_used)]
    fn emit(&self, event: StreamEvent) {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let _ = self.stream.send(self.envelope(*order, event));
    }

    #[allow(clippy::expect_used)]
    fn lock_models(&self) -> std::sync::MutexGuard<'_, ModelConfigManager> {
        self.models
            .lock()
            .expect("model configuration lock poisoned")
    }

    /// 配置里声明的默认模型 selector（没配置就是 None）：宿主直接读配置快照。
    fn default_model_selector(&self) -> Option<String> {
        self.lock_models().snapshot().resolved_default_selector()
    }

    /// 执行前的 selector 预检查：用配置侧现成的纯校验，不构造 provider。
    fn validate_model_selector(&self, selector: Option<&str>) -> Result<(), RpcError> {
        self.lock_models()
            .snapshot()
            .validate_selector(selector)
            .map_err(|error| configuration_error(format!("invalid model selector: {error}")))
    }

    #[allow(clippy::expect_used)]
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ConversationSlot>>> {
        self.sessions
            .lock()
            .expect("app_server session map lock poisoned (fail-stop)")
    }
}

#[cfg(test)]
mod tests;
