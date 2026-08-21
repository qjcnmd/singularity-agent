//! 唯一 AppServer 运行时状态容器与活动 turn 生命周期句柄。
//!
//! Dispatch/lifecycle/events 通过这里的单一状态容器协作；本模块不引入
//! Manager/Service，也不复制活动 turn、取消或 usage 状态。

use super::lifecycle::{agent_config_for_thread, terminal_metadata_for_status};
use super::*;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;

/// 协调 session 索引、信任和活动 turn 的有状态 JSON-RPC 服务。
pub struct AppServer {
    pub(super) store: SessionStore,
    pub(super) sessions_dir: PathBuf,
    pub(super) initialized: bool,
    pub(super) initialized_acknowledged: bool,
    pub(super) shutdown_requested: bool,
    pub(super) provider_snapshot: ProviderConfigSnapshot,
    /// 活动 turn 的单一注册表：取消令牌、线程归属与实时输入箱合一；
    /// 终态化完成后由 guard drop 移除。
    pub(super) active_turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
    /// 测试/诊断：统计本轮会话文件打开次数（每次 open_existing）。
    #[cfg(test)]
    pub(super) session_opens: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(super) terminalization_faults: Arc<Mutex<TerminalizationFaults>>,
    #[doc(hidden)]
    pub test_provider_override:
        Option<std::sync::Arc<dyn singularity_model::Provider + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveTurn {
    pub(super) thread_id: String,
    pub(super) cancellation: CancellationToken,
    /// steer/follow-up 注入句柄；准备阶段注册，关闭一次后永不复用。
    pub(super) inbox: Option<TurnInboxHandle>,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct TerminalizationFaults {
    pub(super) metadata_failures_remaining: usize,
    pub(super) event_failures_remaining: usize,
}

/// 由请求工作线程与 stdio 传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    pub(super) active_turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// Narrow cloneable control seam for active-turn cancellation and input.
///
/// It deliberately contains only the in-memory active-turn registry; ordinary
/// state requests continue to run through the single `AppServer` owner and
/// its SQLite connection.
#[derive(Clone)]
pub struct AppServerControlHandle {
    pub(super) active_turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并将取消传播到每个活动 turn。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        for active_turn in self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .values()
        {
            active_turn.cancellation.cancel();
        }
        Ok(())
    }

    /// 返回连接级 execution stop 是否已经广播。
    pub fn execution_stop_requested(&self) -> bool {
        self.execution_stopped.load(Ordering::SeqCst)
    }
}

pub(super) struct ActiveTurnGuard {
    pub(super) turn_id: String,
    pub(super) active_turns: Arc<Mutex<HashMap<String, ActiveTurn>>>,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
    }
}

impl AppServer {
    pub fn new(store: SessionStore, provider_snapshot: ProviderConfigSnapshot) -> Self {
        let sessions_dir = user_singularity_home()
            .map(|home| home.join(paths::SESSIONS_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(".singularity/sessions"));
        Self {
            store,
            sessions_dir,
            initialized: false,
            initialized_acknowledged: false,
            shutdown_requested: false,
            provider_snapshot,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            session_opens: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            terminalization_faults: Arc::new(Mutex::new(TerminalizationFaults::default())),
            test_provider_override: None,
        }
    }

    /// 仅测试：覆盖会话目录。
    #[doc(hidden)]
    pub fn with_sessions_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.sessions_dir = dir.as_ref().to_path_buf();
        self
    }

    /// 仅测试：注入动态 provider 覆盖。
    #[doc(hidden)]
    pub fn with_test_provider(
        mut self,
        provider: std::sync::Arc<dyn singularity_model::Provider + Send + Sync>,
    ) -> Self {
        self.test_provider_override = Some(provider);
        self
    }

    #[cfg(test)]
    pub(crate) fn inject_terminalization_faults(
        &self,
        metadata_failures: usize,
        event_failures: usize,
    ) {
        if let Ok(mut faults) = self.terminalization_faults.lock() {
            faults.metadata_failures_remaining = metadata_failures;
            faults.event_failures_remaining = event_failures;
        }
    }

    #[cfg(test)]
    pub(super) fn consume_terminal_metadata_failure(&self) -> bool {
        let Ok(mut faults) = self.terminalization_faults.lock() else {
            return false;
        };
        if faults.metadata_failures_remaining == 0 {
            return false;
        }
        faults.metadata_failures_remaining -= 1;
        true
    }

    #[cfg(not(test))]
    pub(super) fn consume_terminal_metadata_failure(&self) -> bool {
        false
    }

    #[cfg(test)]
    pub(crate) fn consume_terminal_event_failure(&self, method: &str) -> bool {
        if !matches!(method, "turn/completed" | "turn/error" | "item/failed") {
            return false;
        }
        let Ok(mut faults) = self.terminalization_faults.lock() else {
            return false;
        };
        if faults.event_failures_remaining == 0 {
            return false;
        }
        faults.event_failures_remaining -= 1;
        true
    }

    #[cfg(not(test))]
    pub(crate) fn consume_terminal_event_failure(&self, _method: &str) -> bool {
        false
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn ready_for_turn_worker(&self) -> bool {
        // 就绪点 = initialize 请求处理完成（回执已发出）；`initialized` 通知
        // 继续把守 ordinary 门禁，不再作为 turn lane 的前置条件。
        self.initialized
    }

    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.cancellation_handle().request_execution_stop()
    }

    pub fn cancellation_handle(&self) -> AppServerCancellationHandle {
        AppServerCancellationHandle {
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
        }
    }

    /// 为单一 turn 工作线程打开独立索引连接，同时共享停止与注入状态。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: self.store.trusted_reopen()?,
            sessions_dir: self.sessions_dir.clone(),
            initialized: true,
            initialized_acknowledged: true,
            shutdown_requested: false,
            provider_snapshot: self.provider_snapshot.clone(),
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
            #[cfg(test)]
            session_opens: Arc::clone(&self.session_opens),
            #[cfg(test)]
            terminalization_faults: Arc::clone(&self.terminalization_faults),
            test_provider_override: self.test_provider_override.clone(),
        })
    }

    pub(crate) fn activate_turn(
        &self,
        turn_id: &str,
        thread_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        if active_turns
            .values()
            .any(|turn| turn.thread_id == thread_id)
        {
            return Err(AppServerError::Workspace(
                "another turn is already running for this session".to_string(),
            ));
        }
        if active_turns.contains_key(turn_id) {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is already active"
            )));
        }
        let cancellation = CancellationToken::new();
        if self.execution_stopped.load(Ordering::SeqCst) {
            cancellation.cancel();
        }
        active_turns.insert(
            turn_id.to_string(),
            ActiveTurn {
                thread_id: thread_id.to_string(),
                cancellation: cancellation.clone(),
                inbox: None,
            },
        );
        drop(active_turns);
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
        };
        Ok((cancellation, guard))
    }

    /// 把 steer/follow-up 注入句柄注册进活动 turn；必须在发布 turn/started
    /// 之前完成，保证 started 后立即注入必成功。
    pub(crate) fn register_turn_inbox(
        &self,
        turn_id: &str,
        inbox: TurnInboxHandle,
    ) -> AppServerResult<()> {
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        let Some(active_turn) = active_turns.get_mut(turn_id) else {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is not active"
            )));
        };
        active_turn.inbox = Some(inbox);
        Ok(())
    }
}

impl AppServer {
    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        if let Some(selector) = selector
            && (self.provider_snapshot.has_explicit_model_selection()
                || selector.contains('/')
                || selector.contains('#'))
        {
            self.provider_snapshot
                .provider_for_selector(Some(selector))
                .map(|_| ())
                .map_err(|_| AppServerError::InvalidParams("invalid model selector".to_string()))?;
        }
        Ok(())
    }

    fn provider_for_thread(
        &self,
        thread: &Thread,
    ) -> Result<singularity_model::OpenAiProvider, singularity_model::ProviderError> {
        self.provider_snapshot
            .provider_for_selector(thread.model.as_deref())
    }

    pub(super) fn provider_and_config_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<(Arc<dyn Provider + Send + Sync>, AgentConfig, bool)> {
        let provider: Arc<dyn Provider + Send + Sync> =
            if let Some(test_provider) = &self.test_provider_override {
                Arc::clone(test_provider)
            } else {
                Arc::new(self.provider_for_thread(thread).map_err(|error| {
                    AppServerError::TurnExecution {
                        stage: TurnFailureStage::AgentLoop,
                        cause: TurnFailureCause::Internal,
                        original: Some(error.to_string()),
                    }
                })?)
            };
        let (config, instructions_truncated) =
            agent_config_for_thread(thread, provider.as_ref(), &self.provider_snapshot)?;
        Ok((provider, config, instructions_truncated))
    }

    pub(crate) fn open_session_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<SessionManager> {
        let record = self.store.get_session(&thread.thread_id)?;
        #[cfg(test)]
        self.session_opens.fetch_add(1, Ordering::SeqCst);
        let session = SessionManager::open_existing(Path::new(&record.rollout_path))?;
        if session.session_id() != record.session_id {
            return Err(AppServerError::Store(StoreError::InvalidState(format!(
                "rollout header id {} does not match index session id {}",
                session.session_id(),
                record.session_id
            ))));
        }
        Ok(session)
    }

    pub(crate) fn open_and_repair_session_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<SessionManager> {
        let mut session = self.open_session_for_thread(thread)?;
        session
            .repair_interrupted_turns()
            .map_err(AppServerError::Session)?;
        session
            .repair_orphaned_tool_calls()
            .map_err(AppServerError::Session)?;
        refresh_session_index_from_open_session(&self.store, &session)?;
        Ok(session)
    }

    /// 终态化：复用本轮已打开的单一 `SessionManager` 落盘 terminal metadata 与
    /// usage（JSONL 是事实源），再更新索引投影（D-011 先持久再索引）。
    pub(crate) fn update_session_status_and_usage(
        &self,
        session: &mut SessionManager,
        turn_id: Option<&str>,
        status: SessionStatus,
        usage: &ModelUsage,
        usage_complete: bool,
    ) -> AppServerResult<SessionRecord> {
        if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
        ) && self.consume_terminal_metadata_failure()
        {
            return Err(AppServerError::Store(StoreError::InvalidState(
                "injected terminal metadata failure".to_string(),
            )));
        }
        if let Some(turn_id) = turn_id
            && let Some(metadata) = terminal_metadata_for_status(turn_id, status)
        {
            self.append_terminal_metadata_if_missing(session, turn_id, metadata)?;
            let usage_value =
                serde_json::to_value(usage_to_wire_with_completeness(usage, usage_complete))?;
            self.append_usage_metadata_if_missing(session, turn_id, usage_value)?;
        }
        let token_usage =
            serde_json::to_value(usage_to_wire_with_completeness(usage, usage_complete))?;
        Ok(self.store.update_session(
            session.session_id(),
            SessionMetadataUpdate {
                status: Some(status),
                token_usage: Some(&token_usage),
                ..SessionMetadataUpdate::default()
            },
        )?)
    }

    fn append_terminal_metadata_if_missing(
        &self,
        session: &mut SessionManager,
        turn_id: &str,
        metadata: singularity_agent::session::SessionMetadata,
    ) -> AppServerResult<()> {
        let already_terminal = session
            .metadata_entries()
            .iter()
            .any(|entry| entry.turn_id() == Some(turn_id) && entry.kind().matches_turn_terminal());
        if !already_terminal {
            session.append_metadata(metadata)?;
        }
        Ok(())
    }

    fn append_usage_metadata_if_missing(
        &self,
        session: &mut SessionManager,
        turn_id: &str,
        usage: Value,
    ) -> AppServerResult<()> {
        let already_persisted = session.metadata_entries().iter().any(|entry| {
            entry.kind() == SessionMetadataKind::Usage && entry.turn_id() == Some(turn_id)
        });
        if !already_persisted {
            session.append_metadata(singularity_agent::session::SessionMetadata::usage(
                turn_id, usage,
            )?)?;
        }
        Ok(())
    }

    /// turn_started 通过本轮已打开的同一 `SessionManager` 落盘（开始标记）。
    pub(crate) fn append_turn_started_metadata(
        &self,
        session: &mut SessionManager,
        turn_id: &str,
    ) -> AppServerResult<()> {
        let already_started = session.metadata_entries().iter().any(|entry| {
            entry.turn_id() == Some(turn_id) && entry.kind() == SessionMetadataKind::TurnStarted
        });
        if !already_started {
            session.append_metadata(singularity_agent::session::SessionMetadata::turn_started(
                turn_id,
            ))?;
        }
        Ok(())
    }

    /// wire 可见的 thread 摘要：持久化 `Active` 只有在本进程存在该会话的
    /// 存活 turn 时才成立；崩溃遗留的 `Active` 投影为 `interrupted`，读取
    /// 不回写索引（终态只能由 turn 的真实结束写入）。
    pub(crate) fn project_thread(&self, record: &SessionRecord) -> Thread {
        let mut thread = thread_from_record(record);
        if thread.last_turn_status == Some(singularity_protocol::ThreadStatus::Active)
            && !self.thread_has_live_turn(&record.session_id)
        {
            thread.last_turn_status = Some(singularity_protocol::ThreadStatus::Interrupted);
        }
        thread
    }

    fn thread_has_live_turn(&self, session_id: &str) -> bool {
        // 单一注册表：存在归属该会话的活动 turn 即视为存活。
        self.active_turns
            .lock()
            // 锁中毒视为没有存活 turn：宁可投影为终态也不伪装运行中。
            .ok()
            .is_some_and(|turns| turns.values().any(|turn| turn.thread_id == session_id))
    }

    /// 终态 turn 的 usage 投影：优先使用本轮已在手的 model_usage；真正缺失
    /// （provider 未上报 usage）时回退到同一会话 JSONL 已持久化的 usage metadata。
    pub(crate) fn terminal_turn_with_usage(
        &self,
        session: &SessionManager,
        turn: Turn,
        usage: &ModelUsage,
        usage_complete: bool,
    ) -> Turn {
        let model_usage = if usage.usage_present {
            Some(usage_to_wire_with_completeness(usage, usage_complete))
        } else {
            self.persisted_usage_for_turn(session, &turn)
                .map(|(persisted, complete)| usage_to_wire_with_completeness(&persisted, complete))
        };
        Turn {
            model_usage,
            ..turn
        }
    }

    /// Return the narrow active-turn handle used by the stdio control lane.
    ///
    /// The handle contains no session-store owner and therefore cannot process
    /// ordinary state requests. It is cloneable so interrupt/steer/follow-up
    /// requests can bypass the ordinary request queue without duplicating the
    /// application state owner.
    pub fn control_handle(&self) -> AppServerControlHandle {
        AppServerControlHandle {
            active_turns: Arc::clone(&self.active_turns),
        }
    }

    fn persisted_usage_for_turn(
        &self,
        session: &SessionManager,
        turn: &Turn,
    ) -> Option<(ModelUsage, bool)> {
        let value = session
            .metadata_entries()
            .into_iter()
            .rev()
            .find(|entry| {
                entry.kind() == SessionMetadataKind::Usage
                    && entry.turn_id() == Some(turn.turn_id.as_str())
            })?
            .field("usage")
            .cloned()?;
        let wire: singularity_protocol::TurnModelUsage = serde_json::from_value(value).ok()?;
        Some((
            ModelUsage {
                input_tokens: wire.input_tokens,
                output_tokens: wire.output_tokens,
                total_tokens: wire.total_tokens,
                cached_input_tokens: wire.cached_input_tokens,
                reasoning_tokens: wire.reasoning_tokens,
                usage_present: wire.usage_present,
            },
            wire.usage_complete,
        ))
    }

    /// 关闭已结束 turn 的实时注入窗口；活动映射保留到 guard drop，
    /// 让并发 session/delete 仍能观察到终态化 worker。终态后的输入必须通过新的 turn/start。
    pub(crate) fn close_turn_inputs(&self, turn_id: &str) {
        if let Ok(mut active_turns) = self.active_turns.lock()
            && let Some(active_turn) = active_turns.get_mut(turn_id)
        {
            active_turn.inbox = None;
        }
    }
}
