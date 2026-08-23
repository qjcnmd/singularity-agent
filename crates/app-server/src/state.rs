//! 唯一 AppServer 运行时状态容器与 Thread 协调器注册表。
//!
//! Turn 执行全部委托给 [`singularity_runtime::Conversation`]；这里只维护
//! 会话索引、线程→协调器映射、存活 turn 注册表（供控制通道与投影判定），
//! 不复制 Agent 状态、取消或 usage。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::Thread;
use singularity_runtime::{Conversation, TurnRunner};

use super::*;

/// 协调 session 索引、信任和 Thread 协调器的有状态 JSON-RPC 服务。
#[derive(Clone)]
pub struct AppServer {
    pub(super) store: Arc<SessionIndex>,
    pub(super) sessions_dir: PathBuf,
    pub(super) initialized: bool,
    pub(super) initialized_acknowledged: bool,
    pub(super) shutdown_requested: bool,
    pub(super) provider_snapshot: ProviderConfigSnapshot,
    /// 共享 turn 执行核心：无状态、可共享。
    pub(super) turn_runner: Arc<TurnRunner>,
    /// thread_id → 长驻协调器；首次接触时按 JSONL 重开并修复。
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
    /// turn_id → thread_id：仅登记执行窗口（TurnStarted 后、终态事件前）。
    pub(super) live_turns: Arc<Mutex<HashMap<String, String>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// 由 stdio 传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// Narrow cloneable control seam for active-turn cancellation and input.
///
/// 只携带存活 turn 注册表与 Thread 协调器映射：steer/followUp/interrupt
/// 全部路由到协调器，不复制执行状态。
#[derive(Clone)]
pub struct AppServerControlHandle {
    pub(super) live_turns: Arc<Mutex<HashMap<String, String>>>,
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
}

impl AppServerControlHandle {
    /// Dispatch one control-lane JSON-RPC message against active-turn handles.
    ///
    /// Notifications are intentionally side-effect free at the protocol layer:
    /// they are accepted without producing a response, while request messages
    /// receive the typed control result or an error response from the caller.
    pub fn handle(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        // Request-only control methods arriving as notifications are true
        // no-ops: no cancellation, enqueue, or response is allowed.
        if message.is_notification() {
            return Ok(Vec::new());
        }
        match message.method_name() {
            Some("turn/interrupt") => self.turn_interrupt(message),
            Some("turn/steer") => self.turn_steer(message),
            Some("turn/followUp") => self.turn_follow_up(message),
            _ => crate::dispatch::invalid_params_response(message.required_id()),
        }
    }

    pub(crate) fn turn_interrupt(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = crate::dispatch::parse_params(&message)?;
        if !self.interrupt_turn(&params.turn_id) {
            return crate::dispatch::not_found_response(message.required_id(), TURN_NOT_FOUND);
        }
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnInterruptResult {
                    turn_id: params.turn_id,
                    status: TurnStatus::Interrupted,
                })?,
            )
            .to_wire_value(),
        ])
    }

    pub(crate) fn turn_steer(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.inject_turn_input(message, false)
    }

    pub(crate) fn turn_follow_up(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.inject_turn_input(message, true)
    }

    fn inject_turn_input(
        &self,
        message: JsonRpcMessage,
        follow_up: bool,
    ) -> AppServerResult<Vec<Value>> {
        let params: TurnInjectionParams = crate::dispatch::parse_params(&message)?;
        let payload = serde_json::to_value(&params.input)?;
        let text = crate::dispatch::input_items_to_text(&payload)?;
        let owner = self
            .live_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&params.turn_id)
            .cloned();
        let Some(thread_id) = owner else {
            return super::dispatch::not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        let Some(conversation) = self
            .conversations
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&thread_id)
            .cloned()
        else {
            return super::dispatch::not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        let accepted = if follow_up {
            conversation.submit_follow_up(text)
        } else {
            conversation.steer(text)
        };
        if !accepted {
            return super::dispatch::invalid_state_response(
                message.required_id(),
                "turn is no longer accepting input",
            );
        }
        crate::dispatch::json_response(
            message.required_id(),
            TurnInjectionResult {
                turn: Turn {
                    turn_id: params.turn_id,
                    thread_id,
                    status: TurnStatus::Running,
                    model_usage: None,
                },
            },
        )
    }

    fn interrupt_turn(&self, turn_id: &str) -> bool {
        let owner = self
            .live_turns
            .lock()
            .ok()
            .and_then(|turns| turns.get(turn_id).cloned());
        let Some(thread_id) = owner else {
            return false;
        };
        let Some(conversation) = self
            .conversations
            .lock()
            .ok()
            .and_then(|map| map.get(&thread_id).cloned())
        else {
            return false;
        };
        conversation.interrupt();
        true
    }
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并把取消传播到每个已登记的 Thread 协调器。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        let conversations: Vec<Arc<Conversation>> = self
            .conversations
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .values()
            .cloned()
            .collect();
        for conversation in conversations {
            conversation.interrupt();
        }
        Ok(())
    }

    /// 返回连接级 execution stop 是否已经广播。
    pub fn execution_stop_requested(&self) -> bool {
        self.execution_stopped.load(Ordering::SeqCst)
    }
}

/// 存活 turn 的执行窗口守卫：drop 时从注册表移除。
pub(super) struct LiveTurnGuard {
    pub(super) turn_id: String,
    pub(super) live_turns: Arc<Mutex<HashMap<String, String>>>,
}

impl LiveTurnGuard {
    /// 登记 turn 执行窗口；同 id 重复登记保持首见归属。
    pub(super) fn register(
        live_turns: Arc<Mutex<HashMap<String, String>>>,
        turn_id: &str,
        thread_id: &str,
    ) -> Self {
        if let Ok(mut turns) = live_turns.lock() {
            turns
                .entry(turn_id.to_string())
                .or_insert(thread_id.to_string());
        }
        Self {
            turn_id: turn_id.to_string(),
            live_turns,
        }
    }
}

impl Drop for LiveTurnGuard {
    fn drop(&mut self) {
        if let Ok(mut turns) = self.live_turns.lock() {
            turns.remove(&self.turn_id);
        }
    }
}

impl AppServer {
    pub fn new(store: SessionIndex, provider_snapshot: ProviderConfigSnapshot) -> Self {
        let sessions_dir = user_singularity_home()
            .map(|home| home.join(paths::SESSIONS_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(".singularity/sessions"));
        let turn_runner = Arc::new(TurnRunner::new(
            sessions_dir.clone(),
            provider_snapshot.clone(),
        ));
        Self {
            store: Arc::new(store),
            sessions_dir,
            initialized: false,
            initialized_acknowledged: false,
            shutdown_requested: false,
            provider_snapshot,
            turn_runner,
            conversations: Arc::new(Mutex::new(HashMap::new())),
            live_turns: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 仅测试：覆盖会话目录。
    #[doc(hidden)]
    pub fn with_sessions_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.sessions_dir = dir.as_ref().to_path_buf();
        self.turn_runner = Arc::new(TurnRunner::new(
            self.sessions_dir.clone(),
            self.provider_snapshot.clone(),
        ));
        self
    }

    /// 仅测试：替换共享 turn 执行核心（可注入 provider 覆盖与故障钩子）。
    #[doc(hidden)]
    pub fn with_turn_runner(mut self, runner: Arc<TurnRunner>) -> Self {
        self.turn_runner = runner;
        self
    }

    /// 仅测试：以固定 provider 取代快照解析结果。
    #[doc(hidden)]
    pub fn with_test_provider(
        mut self,
        provider: Arc<dyn singularity_model::Provider + Send + Sync>,
    ) -> Self {
        self.turn_runner = Arc::new(
            TurnRunner::new(self.sessions_dir.clone(), self.provider_snapshot.clone())
                .with_provider_override(provider),
        );
        self
    }

    #[cfg(test)]
    pub(crate) fn inject_terminalization_faults(
        &self,
        metadata_failures: usize,
        event_failures: usize,
    ) {
        #[allow(unused_variables)]
        let _ = event_failures;
        self.turn_runner
            .inject_terminalization_faults(metadata_failures, 0);
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn store(&self) -> &SessionIndex {
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
            conversations: Arc::clone(&self.conversations),
            execution_stopped: Arc::clone(&self.execution_stopped),
        }
    }

    pub fn control_handle(&self) -> AppServerControlHandle {
        AppServerControlHandle {
            live_turns: Arc::clone(&self.live_turns),
            conversations: Arc::clone(&self.conversations),
        }
    }

    /// 为单一 turn 工作线程共享同一会话索引与协调器注册表。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: Arc::clone(&self.store),
            sessions_dir: self.sessions_dir.clone(),
            initialized: true,
            initialized_acknowledged: true,
            shutdown_requested: false,
            provider_snapshot: self.provider_snapshot.clone(),
            turn_runner: Arc::clone(&self.turn_runner),
            conversations: Arc::clone(&self.conversations),
            live_turns: Arc::clone(&self.live_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
        })
    }

    /// 取得（或按 JSONL 重开并修复）Thread 的长驻协调器。
    ///
    /// 进程内首次接触的线程从会话文件重投影（`resume_thread` 同时执行崩溃
    /// 修复），并把持久化的 settings/状态带回索引行。
    pub(crate) fn conversation_for(&self, session_id: &str) -> AppServerResult<Arc<Conversation>> {
        if let Some(conversation) = self
            .conversations
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(session_id)
            .cloned()
        {
            return Ok(conversation);
        }
        let thread =
            singularity_runtime::store::resume_thread(self.turn_runner.sessions_dir(), session_id)
                .map_err(|error| match error {
                    singularity_runtime::store::ResumeError::NotFound(_) => {
                        AppServerError::Store(SessionIndexError::NotFound(session_id.to_string()))
                    }
                    singularity_runtime::store::ResumeError::Store(message) => {
                        AppServerError::Workspace(format!("failed to resume thread: {message}"))
                    }
                })?;
        // 索引投影与持久化事实对齐：JSONL 是权威，索引行模型/尺寸以重投影为准。
        self.store
            .update_session(
                session_id,
                SessionMetadataUpdate {
                    model: Some(thread.model.as_deref()),
                    ..SessionMetadataUpdate::default()
                },
            )
            .ok();
        let conversation = Conversation::new(Arc::clone(&self.turn_runner), thread);
        self.conversations
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .insert(session_id.to_string(), Arc::clone(&conversation));
        Ok(conversation)
    }

    /// 该会话当前是否存在存活 turn（执行窗口内）。
    pub(crate) fn thread_has_live_turn(&self, session_id: &str) -> bool {
        let has_live = self
            .live_turns
            .lock()
            .ok()
            .is_some_and(|turns| turns.values().any(|owner| owner == session_id));
        if has_live {
            return true;
        }
        // 单写者执行期不经过 live_turns（inline 执行路径）：协调器自身的
        // 活动标记是权威。
        self.conversations
            .lock()
            .ok()
            .and_then(|map| map.get(session_id).cloned())
            .is_some_and(|conversation| conversation.has_active_turn())
    }

    /// 该会话当前是否正被删除拒绝（存在存活 turn 或活动协调器）。
    pub(crate) fn thread_turn_active(&self, session_id: &str) -> bool {
        self.thread_has_live_turn(session_id)
    }

    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        self.turn_runner
            .validate_model_selector(selector)
            .map_err(|_| AppServerError::InvalidParams("invalid model selector".to_string()))
    }

    /// 注册活动窗口；返回的守卫在终态事件后 drop 时移除注册。
    pub(super) fn register_live_turn(&self, turn_id: &str, thread_id: &str) -> LiveTurnGuard {
        LiveTurnGuard::register(Arc::clone(&self.live_turns), turn_id, thread_id)
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
}
