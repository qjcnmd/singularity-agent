//! 本地工作台的深模块：Workspace、Session、模型设置和运行态只在这一层组合。
//!
//! 工作区登记、目录查询和分组投影收在 `workspace` 子模块；单会话快照、活动
//! 事件折叠和终态归并收在 `session` 子模块；本模块留下装配、查找和范围检查、
//! 操作启动、发布入口以及全局事件顺序。

mod session;
mod workspace;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    /// 测试注入点：让下一次操作启动按这个错误失败，用来模拟 OS 线程创建失败。
    #[cfg(test)]
    spawn_failure: Mutex<Option<std::io::Error>>,
    /// 测试注入点：归档在占用检查之后、持久变更之前调用一次，用来构造交错。
    #[cfg(test)]
    archive_check_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl AppServer {
    pub fn new(
        runner: Arc<TurnRunner>,
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
            catalog,
            workspaces,
            models,
            home,
            sessions: Mutex::new(HashMap::new()),
            stream,
            #[cfg(test)]
            directory_read_pause: Mutex::new(None),
            #[cfg(test)]
            spawn_failure: Mutex::new(None),
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

    pub fn create_session(
        &self,
        workspace_id: &str,
        selector: Option<String>,
    ) -> Result<SessionReadResult, RpcError> {
        // 创建、登记和首次读取都在同一个生命周期临界区里做完：新会话不能在
        // 登记和读取之间被归档或移除。
        let _lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(workspace_id)?;
        let selector = selector.or_else(|| self.default_model_selector());
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

    pub fn submit(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        if text.trim().is_empty() {
            return Err(invalid_request("任务内容不能为空。"));
        }
        // 查找或创建 slot 和建立执行预订属于同一段生命周期交接，归档或移除插不进这两步之间。
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(workspace_id, session_id)?;
            let selector = slot.conversation().thread().model;
            self.validate_model_selector(selector.as_deref())?;
            let reservation = slot
                .conversation()
                .reserve_start()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
        self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
            Some(turn_terminal(reservation.run(&text, sink)))
        })
    }

    pub fn steer(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.steer(text).map(|_| ())
        })
    }

    pub fn follow_up(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.submit_follow_up(text).map(|_| ())
        })
    }

    pub fn queue_withdraw(
        &self,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, |conversation| {
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
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.replace_follow_up(control_id, text).map(|_| ())
        })
    }

    /// 立即发送：`control_id` 指定要提升的那一条，省略就提升队列里全部待处理
    /// 输入。要提升哪些由队列 owner 在临界区里读，前端不用照自己的快照逐条请求。
    pub fn queue_send_now(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        control_id: Option<&str>,
    ) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        // 和 worker 的事件、结算共用同一把 SlotState 锁：控制从 Conversation
        // 转到公开投影并发布完之前，结算不能插进来把旧回执盖掉。
        let mut state = slot.lock_state();
        let promoted = slot
            .conversation()
            .promote_pending(control_id)
            .map_err(control_error)?;
        match promoted {
            FollowUpPromotion::Empty => Ok(()),
            FollowUpPromotion::Injected => {
                self.publish_session_locked(session_id, &slot, &mut state);
                Ok(())
            }
            FollowUpPromotion::Reserved { reservation } => {
                // 预订成立就等于独占了该会话；先放开 slot 锁去取 history，再按同样的顺序提交。
                drop(state);
                // 生命周期交接已经由预订做完，后面的读盘和启动不再占全局临界区。
                drop(lifecycle);
                // 只有真要启动新一轮时才解析未来的模型配置：往当前轮注入和
                // 空队列 no-op 都不受这个 selector 影响。校验失败时，预订 guard
                // 的 Drop 会把已提升的输入按接受顺序放回队列，输入不会丢。
                self.validate_model_selector(slot.conversation().thread().model.as_deref())?;
                self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
                self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
                    Some(turn_terminal(reservation.run_promoted(sink)))
                })
            }
        }
    }

    pub fn abort(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, Conversation::abort)
    }

    /// 范围校验和控制接受共用生命周期锁，公开投影和发布共用 SlotState 顺序。
    /// 闭包里只做 Conversation 的短控制操作，不能覆盖 Agent 执行或调用事件 sink。
    fn apply_control(
        &self,
        workspace_id: &str,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<(), ConversationControlError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        let mut state = slot.lock_state();
        apply(slot.conversation()).map_err(control_error)?;
        self.publish_session_locked(session_id, &slot, &mut state);
        Ok(())
    }

    /// 预订成立后先读 history，再在同一把状态锁里初始化并发布这次操作的投影。
    fn begin_operation(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        begin: impl FnOnce(&mut SlotState, Arc<singularity_runtime::ThreadSnapshot>),
    ) -> Result<(), RpcError> {
        let history = self.read_persisted_history(slot)?;
        let mut state = slot.lock_state();
        begin(&mut state, history);
        self.publish_session_locked(session_id, slot, &mut state);
        Ok(())
    }

    fn publish_session_locked(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        state: &mut SlotState,
    ) {
        state.bump_revision();
        self.emit(StreamEvent::SessionChanged {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(state),
        });
    }

    pub fn compact(self: &Arc<Self>, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(workspace_id, session_id)?;
            let reservation = slot
                .conversation()
                .reserve_compaction()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, |state, history| {
            state.begin_compaction(history, now_iso());
        })?;
        self.spawn_operation(
            session_id,
            slot,
            reservation,
            move |reservation, _| match reservation.compact() {
                Ok(outcome) => outcome.terminal(),
                Err(error) => Some(SessionTerminalSnapshot {
                    source: SessionTerminalSource::Compaction,
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                }),
            },
        )
    }

    pub fn rename_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation().phase() != SessionPhase::Idle {
            return Err(session_busy());
        }
        self.catalog
            .rename(session_id, name)
            .map_err(catalog_error)?;
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn archive_session(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        // 占用检查、持久归档和注销 slot 必须在同一个临界区里，否则归档完的旧 slot 还会被启动。
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation().is_occupied() {
            return Err(session_busy());
        }
        #[cfg(test)]
        take_pause(&self.archive_check_pause);
        self.catalog.archive(session_id).map_err(catalog_error)?;
        self.lock_sessions().remove(session_id);
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn update_settings(
        &self,
        workspace_id: &str,
        session_id: &str,
        selector: &str,
    ) -> Result<(), RpcError> {
        let slot = {
            let _lifecycle = self.lock_lifecycle();
            self.open_slot(workspace_id, session_id)?
        };
        slot.conversation()
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

    /// 取走注入的启动错误（只取一次）。
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn take_spawn_failure(&self) -> Option<std::io::Error> {
        self.spawn_failure
            .lock()
            .expect("spawn failure lock poisoned")
            .take()
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn fail_next_spawn(&self, message: &str) {
        *self
            .spawn_failure
            .lock()
            .expect("spawn failure lock poisoned") =
            Some(std::io::Error::other(message.to_string()));
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

    /// 启动执行 worker。返回 Err 时，开始投影和预订都已经归还，调用方据此
    /// 回复启动失败，绝不声称已经接受执行。
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
    ) -> Result<(), RpcError> {
        let app_server = Arc::clone(self);
        let session_id = session_id.to_string();
        // 启动失败发生在线程还不存在的时候：清理要用一份独立的 slot 和身份副本。
        let cleanup_slot = Arc::clone(&slot);
        let cleanup_session_id = session_id.clone();
        #[cfg(test)]
        if let Some(error) = self.take_spawn_failure() {
            // 和 Builder::spawn 失败的语义一致：闭包和预订一起丢弃（Drop 会归还执行
            // 窗口和已提升的输入），然后再归还开始投影。
            drop(run);
            drop(reservation);
            return Err(self.abort_start(&cleanup_session_id, &cleanup_slot, error));
        }
        let spawned = std::thread::Builder::new().spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // 事件回调只在这个 worker 里同步调用，直接借用所有者，不复制句柄。
                let mut event_sink = |event| app_server.on_turn_event(&session_id, &slot, event);
                run(&mut reservation, &mut event_sink)
            }));
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
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                // Builder::spawn 失败时闭包和预订已经随 Drop 归还，这里只要撤回开始投影。
                Err(self.abort_start(&cleanup_session_id, &cleanup_slot, error))
            }
        }
    }

    /// worker 没启动起来时的清理：撤回开始投影（活动回合或压缩、冻结的 history
    /// 和临时终态），并推进 revision，让客户端看到同一会话已经还回来。
    fn abort_start(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        error: std::io::Error,
    ) -> RpcError {
        let mut state = slot.lock_state();
        state.settle(None);
        self.publish_session_locked(session_id, slot, &mut state);
        internal_error(format!("无法启动任务执行线程：{error}"))
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

fn turn_terminal(
    result: Result<singularity_runtime::TurnOutcome, ConversationError>,
) -> SessionTerminalSnapshot {
    match result {
        Ok(outcome) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: outcome.turn_status,
            message: outcome.error.map(|error| error.message),
        },
        Err(error) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: TurnStatus::Failed,
            message: Some(error.to_string()),
        },
    }
}

/// 「会话不属于所选 Workspace」只有这一种错误形状：热 slot 的校验和会话恢复
/// 路径的失败共用同一份公开分类和引导。
fn session_scope_conflict() -> RpcError {
    RpcError::new(
        RpcErrorCode::Conflict,
        "Session 不属于所选 Workspace。",
        "刷新工作台并从所属 Workspace 打开该 Session。",
    )
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

/// 配置已经部分生效、剩下凭据没写成功：界面按同一分类提示重试这次操作。
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
        CatalogError::ScopeMismatch(_) => session_scope_conflict(),
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

/// 测试互锁：取出并执行一次性的注入点。取走就没了，后续调用不再停下。
#[cfg(test)]
fn take_pause(pause: &Mutex<Option<Arc<dyn Fn() + Send + Sync>>>) {
    let taken = pause
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(taken) = taken {
        taken();
    }
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
