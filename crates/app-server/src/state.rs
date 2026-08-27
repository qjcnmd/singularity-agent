//! 唯一 AppServer 运行时状态容器与 Thread 协调器注册表。
//!
//! Turn 执行全部委托给 [`singularity_runtime::Conversation`]；这里只维护
//! 线程→协调器映射（供控制通道与投影判定），
//! 不复制 Agent 状态、取消或 usage。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use singularity_protocol::Thread;
use singularity_runtime::{Conversation, ProviderConfigSnapshot, TurnRunner};

use super::*;

/// 协调信任和 Thread 协调器的有状态 JSON-RPC 服务。
#[derive(Clone)]
pub struct AppServer {
    pub(super) sessions_dir: PathBuf,
    pub(super) initialized: bool,
    pub(super) initialized_acknowledged: bool,
    pub(super) shutdown_requested: bool,
    pub(super) provider_snapshot: ProviderConfigSnapshot,
    /// 共享 turn 执行核心：无状态、可共享。
    pub(super) turn_runner: Arc<TurnRunner>,
    /// thread_id → 长驻协调器；首次接触时按 JSONL 重开并修复。
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// 由 stdio 传输层共享的可克隆停止句柄。
#[derive(Clone)]
pub struct AppServerCancellationHandle {
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
    pub(super) execution_stopped: Arc<AtomicBool>,
}

/// 活动 turn 取消与输入的可克隆窄控制接缝。
///
/// 只携带存活 turn 注册表与 Thread 协调器映射：steer/followUp/interrupt
/// 全部路由到协调器，不复制执行状态。
#[derive(Clone)]
pub struct AppServerControlHandle {
    pub(super) conversations: Arc<Mutex<HashMap<String, Arc<Conversation>>>>,
}

impl AppServerControlHandle {
    /// 对活动 turn 句柄分发一条控制 lane JSON-RPC 消息。
    ///
    /// 通知在协议层刻意无副作用：被接受但不产生响应；请求消息则由调用方
    /// 收到类型化控制结果或错误响应。
    pub fn handle(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        // 以通知到达的纯请求控制方法是真 no-op：不允许取消、入队或响应。
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
        if !self.interrupt_turn(&params.turn_id)? {
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
        let text = crate::dispatch::input_items_to_text(&params.input)?;
        let conversation = self
            .conversations
            .lock()
            .map_err(|error| {
                AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
            })?
            .values()
            .find(|conversation| {
                conversation.active_turn_id().as_deref() == Some(params.turn_id.as_str())
            })
            .cloned();
        let Some(conversation) = conversation else {
            return super::dispatch::not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        let thread_id = conversation
            .thread()
            .map_err(|error| {
                AppServerError::Workspace(format!("conversation thread unavailable: {error}"))
            })?
            .thread_id;
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

    fn interrupt_turn(&self, turn_id: &str) -> AppServerResult<bool> {
        let conversation = self
            .conversations
            .lock()
            .map_err(|error| {
                AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
            })?
            .values()
            .find(|conversation| conversation.active_turn_id().as_deref() == Some(turn_id))
            .cloned();
        let Some(conversation) = conversation else {
            return Ok(false);
        };
        conversation.interrupt();
        Ok(true)
    }
}

impl AppServerCancellationHandle {
    /// 停止后续执行，并把取消传播到每个已登记的 Thread 协调器。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        let conversations: Vec<Arc<Conversation>> = self
            .conversations
            .lock()
            .map_err(|error| {
                AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
            })?
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

impl AppServer {
    pub fn new(provider_snapshot: ProviderConfigSnapshot, sessions_dir: impl AsRef<Path>) -> Self {
        let sessions_dir = sessions_dir.as_ref().to_path_buf();
        let turn_runner = Arc::new(TurnRunner::new(
            sessions_dir.clone(),
            provider_snapshot.clone(),
        ));
        Self {
            sessions_dir,
            initialized: false,
            initialized_acknowledged: false,
            shutdown_requested: false,
            provider_snapshot,
            turn_runner,
            conversations: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 仅测试：以固定 provider 取代快照解析结果。
    #[cfg(test)]
    pub(crate) fn with_test_provider(
        mut self,
        provider: Arc<dyn singularity_runtime::Provider + Send + Sync>,
    ) -> Self {
        self.turn_runner = Arc::new(
            TurnRunner::new(self.sessions_dir.clone(), self.provider_snapshot.clone())
                .with_provider_override(provider),
        );
        self
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
            conversations: Arc::clone(&self.conversations),
        }
    }

    /// 为单一 turn 工作线程共享同一协调器注册表。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            sessions_dir: self.sessions_dir.clone(),
            initialized: true,
            initialized_acknowledged: true,
            shutdown_requested: false,
            provider_snapshot: self.provider_snapshot.clone(),
            turn_runner: Arc::clone(&self.turn_runner),
            conversations: Arc::clone(&self.conversations),
            execution_stopped: Arc::clone(&self.execution_stopped),
        })
    }

    /// 取得（或按 JSONL 重开并修复）Thread 的长驻协调器。
    ///
    /// 进程内首次接触的线程从会话文件重投影（`resume_thread` 同时执行崩溃
    /// 修复）。慢路径持缓存锁完成 resume+insert，避免并发首次接触产生同一
    /// Thread 的两个协调器。
    pub(crate) fn conversation_for(&self, session_id: &str) -> AppServerResult<Arc<Conversation>> {
        // 快路径：缓存命中直接返回。
        if let Some(conversation) = self
            .conversations
            .lock()
            .map_err(|error| {
                AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
            })?
            .get(session_id)
            .cloned()
        {
            return Ok(conversation);
        }
        // 慢路径：持锁完成重投影与回插；锁内二次检查挡住并发未命中。
        let mut guard = self.conversations.lock().map_err(|error| {
            AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
        })?;
        if let Some(conversation) = guard.get(session_id).cloned() {
            return Ok(conversation);
        }
        let thread =
            singularity_runtime::store::resume_thread(self.turn_runner.sessions_dir(), session_id)
                .map_err(|error| match &error {
                    singularity_runtime::store::ResumeError::NotFound(_) => {
                        AppServerError::Store(format!("thread {session_id} was not found"))
                    }
                    singularity_runtime::store::ResumeError::Store(message) => {
                        AppServerError::Workspace(format!("failed to resume thread: {message}"))
                    }
                    // resume 路径不会产生 WriterActive/AnchorNotFound；防御性兜底。
                    other => AppServerError::Workspace(format!("failed to resume thread: {other}")),
                })?;
        let conversation = Conversation::new(Arc::clone(&self.turn_runner), thread);
        guard.insert(session_id.to_string(), Arc::clone(&conversation));
        Ok(conversation)
    }

    /// 该会话当前是否存在存活 turn（执行窗口内）。
    pub(crate) fn thread_has_live_turn(&self, session_id: &str) -> bool {
        self.conversations
            .lock()
            .ok()
            .and_then(|map| map.get(session_id).cloned())
            .is_some_and(|conversation| conversation.has_active_turn())
    }

    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        self.turn_runner
            .validate_model_selector(selector)
            .map_err(|_| AppServerError::InvalidParams("invalid model selector".to_string()))
    }

    /// wire 可见的 thread 摘要：持久化 `Active` 只有在本进程存在该会话的
    /// 存活 turn 时才成立；崩溃遗留的 `Active` 投影为 `interrupted`，读取
    /// 不回写 JSONL（终态只能由 turn 的真实结束写入）。
    pub(crate) fn project_thread(&self, record: &singularity_runtime::ThreadSummary) -> Thread {
        let mut thread = thread_from_summary(record);
        if thread.last_turn_status == Some(singularity_protocol::ThreadStatus::Active)
            && !self.thread_has_live_turn(&record.thread_id)
        {
            thread.last_turn_status = Some(singularity_protocol::ThreadStatus::Interrupted);
        }
        thread
    }
}
