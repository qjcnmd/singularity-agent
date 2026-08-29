//! 唯一 AppServer 运行时状态容器与 Thread 协调器注册表。
//!
//! Turn 执行全部委托给 [`singularity_runtime::Conversation`]；这里只维护
//! 线程→协调器映射（供控制通道与投影判定），
//! 不复制 Agent 状态、取消或 usage。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use singularity_protocol::Thread;
use singularity_runtime::{Conversation, ProviderConfigSnapshot, ThreadCatalog, TurnRunner};

use super::*;

/// 注册表锁中毒的统一错误投影。
pub(crate) fn registry_lock_poisoned<E: std::fmt::Display>(error: E) -> AppServerError {
    AppServerError::Workspace(format!("conversation registry lock poisoned: {error}"))
}

/// 协调信任和 Thread 协调器的有状态 JSON-RPC 服务。
#[derive(Clone)]
pub struct AppServer {
    /// 生命周期无关的共享核心；turn worker 与主服务共享同一份。
    pub(super) core: Core,
    pub(super) initialized: bool,
    pub(super) initialized_acknowledged: bool,
    pub(super) shutdown_requested: bool,
}

/// 与连接生命周期无关的共享核心：turn worker 克隆同一份，不携带
/// initialize/shutdown 门禁状态。provider/会话目录只在此处一次注入
/// [`TurnRunner`]，核心不再各自保存。
#[derive(Clone)]
pub(super) struct Core {
    pub(super) turn_runner: Arc<TurnRunner>,
    pub(super) thread_catalog: ThreadCatalog,
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

impl AppServerCancellationHandle {
    /// 停止后续执行，并把取消传播到每个已登记的 Thread 协调器。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.execution_stopped.store(true, Ordering::SeqCst);
        let conversations: Vec<Arc<Conversation>> = self
            .conversations
            .lock()
            .map_err(registry_lock_poisoned)?
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
        let turn_runner = Arc::new(TurnRunner::new(
            sessions_dir.as_ref().to_path_buf(),
            provider_snapshot,
        ));
        let thread_catalog = ThreadCatalog::new(&turn_runner);
        Self {
            core: Core {
                turn_runner,
                thread_catalog,
                conversations: Arc::new(Mutex::new(HashMap::new())),
                execution_stopped: Arc::new(AtomicBool::new(false)),
            },
            initialized: false,
            initialized_acknowledged: false,
            shutdown_requested: false,
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.cancellation_handle().request_execution_stop()
    }

    pub fn cancellation_handle(&self) -> AppServerCancellationHandle {
        AppServerCancellationHandle {
            conversations: Arc::clone(&self.core.conversations),
            execution_stopped: Arc::clone(&self.core.execution_stopped),
        }
    }

    /// 为单一 turn 工作线程共享同一协调器注册表；门禁状态按「已就绪」固定，
    /// worker 不再观察主连接的 initialize/shutdown 变化。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            core: self.core.clone(),
            initialized: true,
            initialized_acknowledged: true,
            shutdown_requested: false,
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
            .core
            .conversations
            .lock()
            .map_err(registry_lock_poisoned)?
            .get(session_id)
            .cloned()
        {
            return Ok(conversation);
        }
        // 慢路径：持锁完成重投影与回插；锁内二次检查挡住并发未命中。
        let mut guard = self
            .core
            .conversations
            .lock()
            .map_err(registry_lock_poisoned)?;
        if let Some(conversation) = guard.get(session_id).cloned() {
            return Ok(conversation);
        }
        let thread = self
            .core
            .thread_catalog
            .resume_thread(session_id)
            .map_err(|error| match &error {
                singularity_runtime::ResumeError::NotFound(_) => {
                    AppServerError::NotFound(THREAD_NOT_FOUND.to_string())
                }
                singularity_runtime::ResumeError::Store(message) => {
                    AppServerError::Workspace(format!("failed to resume thread: {message}"))
                }
                // resume 路径不会产生 WriterActive/AnchorNotFound；防御性兜底。
                other => AppServerError::Workspace(format!("failed to resume thread: {other}")),
            })?;
        let conversation = Conversation::new(Arc::clone(&self.core.turn_runner), thread);
        guard.insert(session_id.to_string(), Arc::clone(&conversation));
        Ok(conversation)
    }

    /// 该会话当前是否存在存活 turn（执行窗口内）。
    pub(crate) fn thread_has_live_turn(&self, session_id: &str) -> AppServerResult<bool> {
        let map = self
            .core
            .conversations
            .lock()
            .map_err(registry_lock_poisoned)?;
        Ok(map
            .get(session_id)
            .cloned()
            .is_some_and(|conversation| conversation.has_active_turn()))
    }

    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> AppServerResult<()> {
        self.core
            .turn_runner
            .validate_model_selector(selector)
            .map_err(|_| AppServerError::InvalidParams("invalid model selector".to_string()))
    }

    /// wire 可见的 thread 摘要：持久化 `running`（无终态的悬挂 turn_started）
    /// 只有在本进程存在该会话的存活 turn 时才成立；崩溃遗留的 `running`
    /// 投影为 `interrupted`，读取不回写 JSONL（终态只能由 turn 的真实结束
    /// 写入）。
    pub(crate) fn project_thread(
        &self,
        record: &singularity_runtime::ThreadSummary,
    ) -> AppServerResult<Thread> {
        let mut thread = thread_from_summary(record);
        if thread.last_turn_status == Some(singularity_protocol::TurnStatus::Running)
            && !self.thread_has_live_turn(&record.thread_id)?
        {
            thread.last_turn_status = Some(singularity_protocol::TurnStatus::Interrupted);
        }
        Ok(thread)
    }
}
