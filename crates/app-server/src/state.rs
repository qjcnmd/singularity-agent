//! 唯一 AppServer 运行时状态容器与活动 turn 生命周期句柄。
//!
//! Dispatch/lifecycle/events 通过这里的单一状态容器协作；本模块不引入
//! Manager/Service，也不复制活动 turn、取消或 usage 状态。

use super::lifecycle::{agent_config_for_thread, terminal_metadata_for_status};
use super::*;

/// 协调 session 索引、信任和活动 turn 的有状态 JSON-RPC 服务。
pub struct AppServer {
    pub(super) store: SessionStore,
    pub(super) sessions_dir: PathBuf,
    pub(super) initialized: bool,
    pub(super) initialized_acknowledged: bool,
    pub(super) shutdown_requested: bool,
    pub(super) provider_snapshot: ProviderConfigSnapshot,
    pub(super) active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 当前活动 turn id -> thread。终态后移除，避免把输入误认为已排队。
    pub(super) turn_threads: Arc<Mutex<HashMap<String, TurnReference>>>,
    /// 每个活动 turn 的 steer/follow-up 注入句柄（turn/steer、turn/followUp）。
    pub(super) turn_inboxes: Arc<Mutex<HashMap<String, TurnInboxHandle>>>,
    /// 已提交 turn 的运行时 usage 缓存；权威副本是同一 session JSONL 的 usage metadata。
    pub(super) usage_by_turn: Arc<Mutex<HashMap<String, singularity_model::ModelUsage>>>,
    pub(super) usage_complete_by_turn: Arc<Mutex<HashMap<String, bool>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
    pub(super) terminalization_faults: Arc<Mutex<TerminalizationFaults>>,
    #[doc(hidden)]
    pub test_provider_override:
        Option<std::sync::Arc<dyn singularity_model::Provider + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub(super) struct TurnReference {
    pub(super) thread_id: String,
}

#[derive(Debug, Default)]
pub(super) struct TerminalizationFaults {
    pub(super) metadata_failures_remaining: usize,
    pub(super) event_failures_remaining: usize,
}

/// 由请求工作线程与 stdio 传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    pub(super) active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// Narrow cloneable control seam for active-turn cancellation and input.
///
/// It deliberately contains only the in-memory active-turn maps; ordinary
/// state requests continue to run through the single `AppServer` owner and
/// its SQLite connection.
#[derive(Clone)]
pub struct AppServerControlHandle {
    pub(super) active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub(super) turn_threads: Arc<Mutex<HashMap<String, TurnReference>>>,
    pub(super) turn_inboxes: Arc<Mutex<HashMap<String, TurnInboxHandle>>>,
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并将取消传播到每个活动 turn。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        for cancellation in self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .values()
        {
            cancellation.cancel();
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
    pub(super) active_turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    pub(super) turn_inboxes: Arc<Mutex<HashMap<String, TurnInboxHandle>>>,
    pub(super) turn_threads: Arc<Mutex<HashMap<String, TurnReference>>>,
    pub(super) usage_by_turn: Arc<Mutex<HashMap<String, singularity_model::ModelUsage>>>,
    pub(super) usage_complete_by_turn: Arc<Mutex<HashMap<String, bool>>>,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut active_turns) = self.active_turns.lock() {
            active_turns.remove(&self.turn_id);
        }
        if let Ok(mut turn_inboxes) = self.turn_inboxes.lock() {
            turn_inboxes.remove(&self.turn_id);
        }
        if let Ok(mut usage_by_turn) = self.usage_by_turn.lock() {
            usage_by_turn.remove(&self.turn_id);
        }
        if let Ok(mut usage_complete_by_turn) = self.usage_complete_by_turn.lock() {
            usage_complete_by_turn.remove(&self.turn_id);
        }
        if let Ok(mut turn_threads) = self.turn_threads.lock() {
            turn_threads.remove(&self.turn_id);
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
            turn_threads: Arc::new(Mutex::new(HashMap::new())),
            turn_inboxes: Arc::new(Mutex::new(HashMap::new())),
            usage_by_turn: Arc::new(Mutex::new(HashMap::new())),
            usage_complete_by_turn: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
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

    /// 仅测试：注入 terminalization 故障计数。
    #[doc(hidden)]
    pub fn inject_terminalization_faults(&self, metadata_failures: usize, event_failures: usize) {
        if let Ok(mut faults) = self.terminalization_faults.lock() {
            faults.metadata_failures_remaining = metadata_failures;
            faults.event_failures_remaining = event_failures;
        }
    }

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
        self.initialized_acknowledged
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
            turn_threads: Arc::clone(&self.turn_threads),
            turn_inboxes: Arc::clone(&self.turn_inboxes),
            usage_by_turn: Arc::clone(&self.usage_by_turn),
            usage_complete_by_turn: Arc::clone(&self.usage_complete_by_turn),
            execution_stopped: Arc::clone(&self.execution_stopped),
            terminalization_faults: Arc::clone(&self.terminalization_faults),
            test_provider_override: self.test_provider_override.clone(),
        })
    }

    pub(crate) fn activate_turn(
        &self,
        turn_id: &str,
        thread_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let mut turn_threads = self
            .turn_threads
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        if turn_threads.values().any(|r| r.thread_id == thread_id) {
            return Err(AppServerError::Workspace(
                "another turn is already running for this session".to_string(),
            ));
        }
        let cancellation = CancellationToken::new();
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        if active_turns.contains_key(turn_id) {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is already active"
            )));
        }
        if self.execution_stopped.load(Ordering::SeqCst) {
            cancellation.cancel();
        }
        active_turns.insert(turn_id.to_string(), cancellation.clone());
        turn_threads.insert(
            turn_id.to_string(),
            TurnReference {
                thread_id: thread_id.to_string(),
            },
        );
        drop(active_turns);
        drop(turn_threads);
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
            turn_inboxes: Arc::clone(&self.turn_inboxes),
            turn_threads: Arc::clone(&self.turn_threads),
            usage_by_turn: Arc::clone(&self.usage_by_turn),
            usage_complete_by_turn: Arc::clone(&self.usage_complete_by_turn),
        };
        Ok((cancellation, guard))
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
    ) -> AppServerResult<(Arc<dyn Provider + Send + Sync>, AgentConfig)> {
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
        let config = agent_config_for_thread(thread, provider.as_ref(), &self.provider_snapshot)?;
        Ok((provider, config))
    }

    pub(crate) fn open_session_for_thread(
        &self,
        thread: &Thread,
    ) -> AppServerResult<SessionManager> {
        let record = self.store.get_session(&thread.thread_id)?;
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

    pub(crate) fn update_session_status_and_usage(
        &self,
        session_id: &str,
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
            self.append_terminal_metadata_if_missing(session_id, turn_id, metadata)?;
            let usage_value =
                serde_json::to_value(usage_to_wire_with_completeness(usage, usage_complete))?;
            self.append_usage_metadata_if_missing(session_id, turn_id, usage_value)?;
        }
        let token_usage =
            serde_json::to_value(usage_to_wire_with_completeness(usage, usage_complete))?;
        Ok(self.store.update_session(
            session_id,
            SessionMetadataUpdate {
                status: Some(status),
                token_usage: Some(&token_usage),
                ..SessionMetadataUpdate::default()
            },
        )?)
    }

    fn append_terminal_metadata_if_missing(
        &self,
        session_id: &str,
        turn_id: &str,
        metadata: singularity_agent::session::SessionMetadata,
    ) -> AppServerResult<()> {
        let record = self.store.get_session(session_id)?;
        let mut session = SessionManager::open_existing(Path::new(&record.rollout_path))?;
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
        session_id: &str,
        turn_id: &str,
        usage: Value,
    ) -> AppServerResult<()> {
        let record = self.store.get_session(session_id)?;
        let mut session = SessionManager::open_existing(Path::new(&record.rollout_path))?;
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

    pub(crate) fn append_turn_started_metadata(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> AppServerResult<()> {
        let record = self.store.get_session(session_id)?;
        let mut session = SessionManager::open_existing(Path::new(&record.rollout_path))?;
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
        // 活跃判定同时要求取消令牌和当前 turn→thread 映射存在。
        let active = self.active_turns.lock();
        let threads = self.turn_threads.lock();
        match (active, threads) {
            (Ok(active), Ok(turn_threads)) => active.keys().any(|turn_id| {
                turn_threads
                    .get(turn_id)
                    .is_some_and(|reference| reference.thread_id == session_id)
            }),
            // 锁中毒视为没有存活 turn：宁可投影为终态也不伪装运行中。
            _ => false,
        }
    }

    pub(crate) fn turn_with_usage(&self, turn: Turn) -> Turn {
        let usage = self
            .usage_by_turn
            .lock()
            .ok()
            .and_then(|cache| {
                cache.get(&turn.turn_id).cloned().map(|usage| {
                    let usage_complete = self
                        .usage_complete_by_turn
                        .lock()
                        .ok()
                        .and_then(|complete| complete.get(&turn.turn_id).copied())
                        .unwrap_or(true);
                    (usage, usage_complete)
                })
            })
            .or_else(|| self.persisted_usage_for_turn(&turn));
        match usage {
            Some((usage, usage_complete)) => Turn {
                model_usage: Some(usage_to_wire_with_completeness(&usage, usage_complete)),
                ..turn
            },
            None => turn,
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
            turn_threads: Arc::clone(&self.turn_threads),
            turn_inboxes: Arc::clone(&self.turn_inboxes),
        }
    }

    fn persisted_usage_for_turn(&self, turn: &Turn) -> Option<(ModelUsage, bool)> {
        let record = self.store.get_session(&turn.thread_id).ok()?;
        let session = SessionManager::open_existing(Path::new(&record.rollout_path)).ok()?;
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

    pub(crate) fn remember_usage(&self, turn_id: &str, usage: &ModelUsage, usage_complete: bool) {
        let _ = self.usage_by_turn.lock().map(|mut cache| {
            cache.insert(turn_id.to_string(), usage.clone());
        });
        let _ = self.usage_complete_by_turn.lock().map(|mut cache| {
            cache.insert(turn_id.to_string(), usage_complete);
        });
    }

    /// 关闭已结束 turn 的实时注入窗口；活动映射保留到 guard drop，
    /// 让并发 session/delete 仍能观察到终态化 worker。终态后的输入必须通过新的 turn/start。
    pub(crate) fn close_turn_inputs(&self, turn_id: &str) {
        if let Ok(mut inboxes) = self.turn_inboxes.lock() {
            inboxes.remove(turn_id);
        }
    }
}
