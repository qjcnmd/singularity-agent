//! 本地工作台的深模块：Workspace、Session、模型设置与运行态只有这一层组合。
//!
//! 工作区登记、目录查询与分组投影收在 `workspace` 子模块；单会话快照、活动
//! 事件折叠与终态归并收在 `session` 子模块；本模块保留装配、查找与范围检查、
//! 操作启动、发布入口和全局事件顺序。

mod session;
mod workspace;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use singularity_core::now_iso;
use singularity_model::ModelConfigOwner;
use singularity_protocol::{
    EmptyParams, ProviderConfigurationInput, RpcError, RpcErrorCode, SessionPhase,
    SessionReadResult, SessionSettledPayload, SessionTerminalSnapshot, SessionTerminalSource,
    StreamEnvelope, StreamEvent, TurnEvent, TurnStatus, WORKBENCH_PROTOCOL_VERSION,
    WorkbenchBootstrap,
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

pub struct Workbench {
    generation: String,
    revision: Mutex<u64>,
    /// 完整工作台快照的构造与发布顺序；不覆盖会话执行或普通增量事件。
    workbench_publication: Mutex<()>,
    /// 会话生命周期临界区：把「范围/成员校验 → slot 查找或创建 → 接受输入/
    /// 建立预订」与「占用检查 → 持久变更 → 注销」放进同一个短临界区，使
    /// 销毁操作不可能穿过启动占用之间尚未打开写者的窗口。它只保护这些短步骤，
    /// 绝不跨越模型请求、工具执行或整个任务。
    lifecycle: Mutex<()>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    /// 与 runner 共享的磁盘配置入口；每次读取都在短临界区内完成。
    models: Arc<Mutex<ModelConfigOwner>>,
    /// 应用主目录：宿主查询（技能发现）与执行链共用同一个事实。
    home: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Arc<ConversationSlot>>>,
    stream: broadcast::Sender<StreamEnvelope>,
    /// 测试注入点：未打开任务的目录读盘开始前调用一次，用于确定性证明该读盘
    /// 不占用会话 map 锁。
    #[cfg(test)]
    directory_read_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// 测试注入点：下一次操作启动改为按此错误失败，用于模拟 OS 线程创建失败。
    #[cfg(test)]
    spawn_failure: Mutex<Option<std::io::Error>>,
    /// 测试注入点：归档在占用检查之后、持久变更之前调用一次，用于确定性构造
    /// 「检查后、启动前」的交错。
    #[cfg(test)]
    archive_check_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Workbench {
    pub fn new(
        runner: Arc<TurnRunner>,
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<Mutex<ModelConfigOwner>>,
        home: std::path::PathBuf,
    ) -> Arc<Self> {
        let (stream, _) = broadcast::channel(STREAM_CAPACITY);
        Arc::new(Self {
            generation: Uuid::new_v4().to_string(),
            revision: Mutex::new(0),
            workbench_publication: Mutex::new(()),
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
        // 配置锁内只解析本次查询所用的凭据（显式输入优先，否则回退已存储的
        // key）；URL 解释、请求构造与发送都在发现实现内一次完成。
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
        // 创建、登记与首次读取在同一生命周期临界区内完成：新建的会话不允许
        // 在登记与读取之间被归档或移除。
        let _lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(workspace_id)?;
        let selector = selector.or_else(|| self.default_model_selector());
        if selector.is_some() {
            self.validate_model_selector(selector.as_deref())?;
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
        workspace::page_limit(limit)?;
        // 只有「查找或创建 slot」属于生命周期交接；整份历史读盘不占该临界区。
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
        // 查找或创建 slot 与建立执行预订是同一段生命周期交接：预订一成立，
        // 归档/移除的占用检查就必然看到它，销毁操作不可能插在两者之间。
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
        let history = self.read_persisted_history(&slot)?;
        {
            let mut state = slot.lock_state();
            state.begin_turn(history);
            self.publish_session_locked(session_id, &slot, &mut state);
        }
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

    /// 立即发送：`control_id` 为要提升的那一条，省略时提升当前队列中的全部
    /// 待处理输入。目标集合由队列 owner 在临界区内读取，前端不再按自己的
    /// 快照逐条请求。
    pub fn queue_send_now(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        control_id: Option<&str>,
    ) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        // 与 worker 的事件及结算共用 SlotState 顺序：控制从 Conversation
        // 转移到公开投影并发布之前，结算不能插入并被旧回执覆盖。
        let mut state = slot.lock_state();
        let promoted = slot
            .conversation()
            .promote_pending(control_id)
            .map_err(control_error)?;
        match promoted {
            // 空队列上的“全部发送”没有交接，也不是失败。
            FollowUpPromotion::Empty => Ok(()),
            FollowUpPromotion::Injected(_) => {
                self.publish_session_locked(session_id, &slot, &mut state);
                Ok(())
            }
            FollowUpPromotion::Reserved { reservation } => {
                // 预订成立即独占该会话；释放 slot 锁去取 history，再按同一顺序提交。
                drop(state);
                // 预订已把生命周期交接做完，后续读盘与启动不占全局临界区。
                drop(lifecycle);
                // 只有真正要启动新轮才解析未来模型配置：现轮注入与空队列
                // no-op 不受未来 selector 影响。校验失败时预订 guard 的 Drop
                // 把已提升的输入按接受序放回队列，输入不会丢失。
                self.validate_model_selector(slot.conversation().thread().model.as_deref())?;
                let history = self.read_persisted_history(&slot)?;
                let mut state = slot.lock_state();
                state.begin_turn(history);
                self.publish_session_locked(session_id, &slot, &mut state);
                drop(state);
                self.spawn_operation(session_id, slot, reservation, move |reservation, sink| {
                    Some(turn_terminal(reservation.run_promoted(sink)))
                })
            }
        }
    }

    pub fn abort(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, Conversation::abort)
    }

    /// 范围校验与控制接受共用生命周期锁，公开投影与发布共用 SlotState 顺序。闭包只执行
    /// Conversation 的短控制操作，不得覆盖 Agent 执行或调用事件 sink。
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

    /// 会话投影变化的一次发布：推进该会话的 revision，并按全局流序号广播。
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
        // 查找/创建 slot 与建立压缩预订同属一段生命周期交接。
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(workspace_id, session_id)?;
            let reservation = slot
                .conversation()
                .reserve_compaction()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        let history = self.read_persisted_history(&slot)?;
        {
            let mut state = slot.lock_state();
            state.begin_compaction(history, now_iso());
            self.publish_session_locked(session_id, &slot, &mut state);
        }
        self.spawn_operation(session_id, slot, reservation, move |reservation, _| {
            match reservation.compact() {
                // 摘要已经落盘：历史里的压缩条目就是这次操作的反馈。
                Ok(singularity_runtime::CompactionOutcome::Reduced) => None,
                // 没有可替换的内容：这是正常结果，界面照常给出“没有可压缩的
                // 内容”，不把任务标成失败。
                Ok(singularity_runtime::CompactionOutcome::NotNeeded) => {
                    Some((TurnStatus::Completed, None))
                }
                // 已接受的停止：状态是唯一事实，不附带通用取消文字；这与普通
                // 回合中断（终态不带 message）以及冷读从账本恢复的结果一致。
                Err(ConversationError::Compaction(
                    singularity_runtime::CompactionRunError::Interrupted(_),
                )) => Some((TurnStatus::Interrupted, None)),
                Err(error) => Some((TurnStatus::Failed, Some(error.to_string()))),
            }
            .map(|(status, message)| SessionTerminalSnapshot {
                source: SessionTerminalSource::Compaction,
                status,
                message,
            })
        })
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
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn archive_session(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        // 占用检查、持久归档与注销 slot 必须在同一临界区内：否则启动占用
        // 可能在检查之后、写者打开之前插进来，归档成功后旧 slot 仍会启动。
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        if session_occupied(slot.conversation()) {
            return Err(session_busy());
        }
        // 占用检查之后、持久变更之前：这一刻仍在同一临界区内，启动占用无法插进来。
        #[cfg(test)]
        take_pause(&self.archive_check_pause);
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
        // 全局 map 锁只覆盖这次的查找：scope 校验要读工作区，恢复未打开任务
        // 还要读盘，两者都不能在持锁期间发生。
        let workspace = self.workspace(workspace_id)?;
        let open = self.lock_sessions().get(session_id).cloned();
        if let Some(slot) = open {
            verify_workspace_thread(&workspace, &slot.conversation().thread().cwd)?;
            return Ok(slot);
        }
        // 未打开的任务把期望目录交给恢复路径：校验发生在会话头部解析之后、
        // 任何重写或修复之前，因此传错工作区不会改动目标文件。
        let thread = self
            .catalog
            .resume_thread(session_id, &workspace.root)
            .map_err(catalog_error)?;
        Ok(self.insert_slot(thread))
    }

    /// 建立 slot 并登记进全局 map；同一会话已打开时复用既有 slot。
    fn insert_slot(&self, thread: singularity_protocol::Thread) -> Arc<ConversationSlot> {
        let session_id = thread.thread_id.clone();
        let conversation = Conversation::new(Arc::clone(&self.runner), thread);
        let slot = Arc::new(ConversationSlot::new(conversation));
        self.lock_sessions()
            .entry(session_id)
            .or_insert_with(|| Arc::clone(&slot))
            .clone()
    }

    /// 一次会话读取：history、活动事件与运行态来自同一受保护状态。
    ///
    /// 整段读取在同一把 slot 锁内完成，包括冷路径的读盘。回合开始与结算都必须在
    /// 同一把锁内提交，因此锁内读到的 history、运行态与活动事件必然属于同一个瞬间：
    /// 「读盘期间插进一个回合」这个交错在结构上不存在，不需要比对 revision 重新取样。
    /// 代价是读盘期间 worker 的事件投影会等这一次读盘（整份会话解析）。
    ///
    /// 读取只获取投影：它不建立、不修复也不清空活动回合。
    fn read_from_slot(
        &self,
        slot: &ConversationSlot,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<SessionReadResult, RpcError> {
        let state = slot.lock_state();
        let capture = match state.frozen_history() {
            // 回合进行中（以及结算前的重复读取）：内存里的冻结 history 就是当前
            // 状态，不再读盘。
            Some(history) => slot.capture(&state, history),
            // 冷路径没有内存终态（slot 刚建立或宿主重启）：最近一次独立压缩的
            // 失败/中断就是当前操作反馈，从同一份持久快照恢复，使热读与冷读对
            // 同一操作给出一致结果。
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

    /// 测试互锁：把未打开任务的目录读盘停在会话 map 锁之外，供并发用例确定性地
    /// 观察该读盘期间的 map 访问。
    #[cfg(test)]
    fn run_directory_read_pause(&self) {
        take_pause(&self.directory_read_pause);
    }

    /// 测试注入点：一次性取走注入的启动错误，用于模拟 OS 线程创建失败。
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn take_spawn_failure(&self) -> Option<std::io::Error> {
        self.spawn_failure
            .lock()
            .expect("spawn failure lock poisoned")
            .take()
    }

    /// 测试注入点：让下一次操作启动按给定原因失败。
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn fail_next_spawn(&self, message: &str) {
        *self
            .spawn_failure
            .lock()
            .expect("spawn failure lock poisoned") =
            Some(std::io::Error::other(message.to_string()));
    }

    /// 读取最新的持久化 history。start 路径在 slot 锁外调用它：预订成立时上一个
    /// worker 已走完结算，读盘不与事件投影竞争；会话读取路径则在自己那把锁内调用。
    fn read_persisted_history(
        &self,
        slot: &ConversationSlot,
    ) -> Result<Arc<singularity_runtime::ThreadSnapshot>, RpcError> {
        self.catalog
            .read_snapshot(&slot.conversation().thread().thread_id)
            .map_err(catalog_error)
    }

    fn on_turn_event(&self, session_id: &str, slot: &ConversationSlot, event: TurnEvent) {
        // 控制处置变化归约为会话快照发布：控制事实只由会话快照一种表示
        // 承载，不进入活动 turn 的事件序列。
        if let TurnEvent::ControlChanged { .. } = &event {
            self.bump_and_emit_session(session_id, slot);
            return;
        }
        // 单会话投影（活动回合、进度替换、revision）由 slot 完成；本层在同一
        // 把锁内取得 envelope 并广播，折叠与广播之间不插入其他事件。
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
        // 终态来自执行链的可信提交：历史读取失败不改变它，读取错误由
        // 现有会话读取路径独立呈现（history 置空强制下一次读取重试）。
        state.settle(terminal);
        // 发布结算前释放操作预订；新操作的开始投影等待此锁。
        drop(reservation);
        self.emit(StreamEvent::SessionSettled {
            session_id: session_id.to_string(),
            payload: SessionSettledPayload {
                runtime: slot.runtime_from(&state),
            },
        });
    }

    /// 发布一次结算。结算路径本身可能因共享状态中毒而失败：此时没有可发布的
    /// 会话投影，但也不把界面留在“仍在运行”——按既有重同步通道要求客户端
    /// 重拉基线，不伪造终态，也不新建恢复状态。
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

    /// 启动执行 worker。返回 Err 时开始投影与预订都已归还，调用方据此回复
    /// 启动失败，绝不声称已接受执行。
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
        let workbench = Arc::clone(self);
        let session_id = session_id.to_string();
        // 启动失败发生在线程尚未存在时：清理需要一份独立的 slot 与身份副本。
        let cleanup_slot = Arc::clone(&slot);
        let cleanup_session_id = session_id.clone();
        #[cfg(test)]
        if let Some(error) = self.take_spawn_failure() {
            // 与 Builder::spawn 失败同一语义：闭包与预订一起丢弃（Drop 归还
            // 执行窗口与已提升输入），随后归还开始投影。
            drop(run);
            drop(reservation);
            return Err(self.abort_start(&cleanup_session_id, &cleanup_slot, error));
        }
        let spawned = std::thread::Builder::new().spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // 事件回调只在本 worker 内同步调用，直接借用所有者，不再复制句柄。
                let mut event_sink = |event| workbench.on_turn_event(&session_id, &slot, event);
                run(&mut reservation, &mut event_sink)
            }));
            let terminal = match outcome {
                Ok(terminal) => terminal,
                Err(payload) => {
                    // 宿主故障：先按既有交还规则归还本轮已接受但未交付的输入，
                    // 再以真实原因结算显示投影。显示投影不是持久账本，因此这里
                    // 不声称执行链已提交可信终态。
                    let abandoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        slot.conversation().abandon_turn();
                    }));
                    if abandoned.is_err() {
                        // 共享状态已中毒：交还无法完成，同样不把界面留在“仍在
                        // 运行”，按既有重同步通道要求客户端重拉基线。
                        workbench.require_resync();
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
            workbench.settle_operation(&session_id, &slot, terminal, reservation);
        });
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                // Builder::spawn 失败时闭包已被丢弃，预订随之归还；这里只需撤回
                // 开始投影并让客户端看到同一会话的归还。
                Err(self.abort_start(&cleanup_session_id, &cleanup_slot, error))
            }
        }
    }

    /// worker 未启动的开始失败清理：撤回开始投影（活动回合/压缩、冻结 history
    /// 与临时终态），并推进 revision 让客户端看到同一会话的归还。
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

    /// 发布完整工作台快照。快照构造失败不推翻任何已提交的操作结果：
    /// 读侧无法展示时经重同步通道要求客户端重新拉取基线。
    fn publish_workbench_snapshot(&self) {
        let _publication = self.lock_workbench_publication();
        self.publish_workbench_result(self.bootstrap());
    }

    /// 读侧无法继续用增量同步时要求客户端重拉基线；不改变任何已提交结果。
    fn require_resync(&self) {
        self.emit(StreamEvent::ResyncRequired {
            payload: EmptyParams {},
        });
    }

    fn publish_workbench_result(&self, snapshot: Result<WorkbenchBootstrap, RpcError>) {
        match snapshot {
            Ok(payload) => {
                self.emit(StreamEvent::WorkbenchChanged { payload });
            }
            Err(_) => self.require_resync(),
        }
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

    /// 会话生命周期临界区。锁序为 lifecycle → publication → sessions →
    /// SlotState，调用方只在本文件公开入口的最外层取得它。
    #[allow(clippy::expect_used)]
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .expect("workbench lifecycle lock poisoned")
    }

    /// 发布一个流事件：全局流序号在此推进，并随 StreamEnvelope 交付给消费者，
    /// 不作为函数返回值沿调用链传递。
    #[allow(clippy::expect_used)]
    fn emit(&self, event: StreamEvent) {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let revision = *order;
        let _ = self.stream.send(StreamEnvelope {
            version: WORKBENCH_PROTOCOL_VERSION,
            generation: self.generation.clone(),
            revision,
            event,
        });
    }

    #[allow(clippy::expect_used)]
    fn lock_models(&self) -> std::sync::MutexGuard<'_, ModelConfigOwner> {
        self.models
            .lock()
            .expect("model configuration lock poisoned")
    }

    /// 目录声明的默认模型 selector（未配置时为 None）：宿主直接读配置快照。
    fn default_model_selector(&self) -> Option<String> {
        self.lock_models().snapshot().resolved_default_selector()
    }

    /// 执行前的 selector 预检查：配置侧已有的纯校验，不构造 provider。
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
            .expect("workbench session map lock poisoned (fail-stop)")
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

/// 会话不属于所选 Workspace 的唯一错误形状：热 slot 的校验与会话恢复
/// 路径的失败共用同一份公开分类与引导。
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

/// 测试互锁：取出并执行一次性的注入点。取值即被取走，后续调用不再停下。
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
