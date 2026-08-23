//! Thread 的长驻协调器：单活动 turn、输入注入、取消与设置生效时序。
//!
//! [`Conversation`] 是无交互入口与 TUI 共用的生命周期状态机。它不实现任何
//! 执行细节：turn 体完全委托给 [`crate::TurnRunner`]，这里只维护
//! 「同一 Thread 至多一个活动 turn」的不变量、steer/followUp 注入句柄的
//! 注册窗口、取消令牌，以及「活动 turn 期间的设置排队到 turn 完成后生效」。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::TurnInboxHandle;
use singularity_agent::session::SessionManager;
use singularity_core::CancellationToken;
use singularity_model::split_model_selector;

use crate::error::TurnRunError;
use crate::events::TurnEventSink;
use crate::objects::{Thread, TurnStatus};
use crate::runner::{TurnOutcome, TurnParams, TurnRunner};

/// TUI 设置面板可修改的当前 Thread 运行时设置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none() && self.reasoning.is_none()
    }
}

/// 一个活动 turn 的控制面：调用方在执行期间持有，用于取消与实时注入。
///
/// inbox 由 runner 在 Agent 构造完成后注册（先于 turn/started 事件发布），
/// 保证 started 后立即注入必成功；终态化前由 runner 关闭注入窗口。
#[derive(Default)]
pub struct TurnControls {
    pub cancellation: CancellationToken,
    pub(crate) inbox: Mutex<Option<TurnInboxHandle>>,
}

impl TurnControls {
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            inbox: Mutex::new(None),
        }
    }

    /// 把输入注入当前 turn；turn 已关闭注入窗口时返回 false。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        self.inject(singularity_agent::agent::TurnInputKind::Steer, text)
    }

    /// 把输入排入当前 turn 结束后的下一轮；关闭后返回 false。
    pub fn follow_up(&self, text: impl Into<String>) -> bool {
        self.inject(singularity_agent::agent::TurnInputKind::FollowUp, text)
    }

    fn inject(
        &self,
        kind: singularity_agent::agent::TurnInputKind,
        text: impl Into<String>,
    ) -> bool {
        let Ok(guard) = self.inbox.lock() else {
            return false;
        };
        guard.as_ref().is_some_and(|inbox| {
            inbox
                .lock()
                .is_ok_and(|mut inbox| inbox.enqueue(kind, text.into()))
        })
    }

    pub(crate) fn register_inbox(&self, _turn_id: &str, inbox: TurnInboxHandle) {
        if let Ok(mut guard) = self.inbox.lock() {
            *guard = Some(inbox);
        }
    }

    pub(crate) fn close_inbox(&self) {
        if let Ok(mut guard) = self.inbox.lock() {
            *guard = None;
        }
    }
}

struct ConversationState {
    thread: Thread,
    active: Option<Arc<TurnControls>>,
    queued_settings: Option<SettingsPatch>,
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    state: Mutex<ConversationState>,
}

/// 协调层错误。
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("thread already has an active turn")]
    TurnAlreadyActive,
    #[error("{0}")]
    Settings(String),
    #[error(transparent)]
    Turn(#[from] TurnRunError),
}

impl Conversation {
    pub fn new(runner: Arc<TurnRunner>, thread: Thread) -> Self {
        Self {
            runner,
            state: Mutex::new(ConversationState {
                thread,
                active: None,
                queued_settings: None,
            }),
        }
    }

    pub fn runner(&self) -> &TurnRunner {
        &self.runner
    }

    /// 当前 Thread 投影快照。
    pub fn thread(&self) -> Result<Thread, ConversationError> {
        self.state
            .lock()
            .map(|state| state.thread.clone())
            .map_err(|_| ConversationError::Settings("conversation state poisoned".to_string()))
    }

    /// 当前 Thread 是否有正在执行的 turn。
    pub fn has_active_turn(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.active.is_some())
    }

    /// 向活动 turn 注入立即引导输入；无活动 turn 或注入窗口已关闭时为 false。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.active.clone())
            .is_some_and(|controls| controls.steer(text))
    }

    /// 向活动 turn 排队 follow-up 输入；无活动 turn 时为 false。
    pub fn follow_up(&self, text: impl Into<String>) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.active.clone())
            .is_some_and(|controls| controls.follow_up(text))
    }

    /// 中断当前活动 turn；无活动 turn 时为 no-op。
    pub fn interrupt(&self) {
        if let Some(controls) = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.active.clone())
        {
            controls.cancellation.cancel();
        }
    }

    /// 修改当前 Thread 的 provider/model/reasoning。
    ///
    /// 活动 turn 期间仅记录意图并返回；turn 终态收敛后由
    /// [`Self::apply_queued_settings`] 持久化并在下一轮生效。空闲时立即校验
    /// 并持久化为 `thread_settings` metadata（不改写全局配置）。
    pub fn queue_settings(&self, patch: SettingsPatch) -> Result<bool, ConversationError> {
        if patch.is_empty() {
            return Ok(false);
        }
        let selector = self.compose_selector(&patch)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::Settings("conversation state poisoned".to_string()))?;
        if state.active.is_some() {
            state.queued_settings = Some(patch);
            return Ok(true);
        }
        self.persist_settings_locked(&mut state, selector)?;
        Ok(true)
    }

    /// 取出 turn 完成后排队的设置意图。由调用方在 turn 成功返回后调用，
    /// 持久化失败不吞没、也不影响已完成的 turn 结果。
    pub fn take_queued_settings(&self) -> Option<SettingsPatch> {
        let mut state = self.state.lock().ok()?;
        state.queued_settings.take()
    }

    /// 校验并持久化设置，更新内存中的 Thread 投影。
    pub fn apply_queued_settings(&self, patch: SettingsPatch) -> Result<(), ConversationError> {
        if patch.is_empty() {
            return Ok(());
        }
        let selector = self.compose_selector(&patch)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::Settings("conversation state poisoned".to_string()))?;
        self.persist_settings_locked(&mut state, selector)
    }

    fn compose_selector(&self, patch: &SettingsPatch) -> Result<String, ConversationError> {
        let current = self
            .state
            .lock()
            .map_err(|_| ConversationError::Settings("conversation state poisoned".to_string()))?
            .thread
            .model
            .clone();
        let parts = split_model_selector(current.as_deref().unwrap_or(""));
        let current_provider = parts.provider.map(str::to_string);
        let current_model = parts.model.map(str::to_string);
        let current_reasoning = parts.effort.map(str::to_string);
        let provider = patch
            .provider
            .clone()
            .or(current_provider)
            .unwrap_or_else(|| "openai_compatible".to_string());
        let model = patch.model.clone().or(current_model);
        let reasoning = patch.reasoning.clone().or(current_reasoning);
        let Some(model) = model.filter(|model| !model.trim().is_empty()) else {
            return Err(ConversationError::Settings(
                "thread settings require a model".to_string(),
            ));
        };
        if provider.trim().is_empty()
            || provider.chars().any(char::is_whitespace)
            || model.chars().any(char::is_whitespace)
            || reasoning.as_deref().is_some_and(|value| {
                value.trim().is_empty() || value.chars().any(char::is_whitespace)
            })
        {
            return Err(ConversationError::Settings(
                "invalid provider/model/reasoning value".to_string(),
            ));
        }
        let selector = compose_model_selector(&provider, &model, reasoning.as_deref());
        self.runner
            .validate_model_selector(Some(&selector))
            .map_err(ConversationError::Settings)?;
        Ok(selector)
    }

    fn persist_settings_locked(
        &self,
        state: &mut ConversationState,
        selector: String,
    ) -> Result<(), ConversationError> {
        let path =
            crate::store::thread_session_path(self.runner.sessions_dir(), &state.thread.thread_id);
        let mut session = SessionManager::open_existing(&path)
            .map_err(|error| ConversationError::Settings(error.to_string()))?;
        let parts = split_model_selector(&selector);
        let metadata = singularity_agent::session::SessionMetadata::thread_settings(
            parts.provider.unwrap_or("openai_compatible"),
            parts.model.unwrap_or_default(),
            parts.effort.map(str::to_string),
        )
        .map_err(|error| ConversationError::Settings(error.to_string()))?;
        session
            .append_metadata(metadata)
            .map_err(|error| ConversationError::Settings(error.to_string()))?;
        drop(session);
        state.thread.model = Some(selector);
        Ok(())
    }

    /// 执行一轮 turn 直到终态。
    ///
    /// 同一时刻只允许一个活动 turn；执行期间通过共享的
    /// [`TurnControls`]（TUI 从其他线程）进行 steer/followUp 与取消。
    pub fn run_turn(
        &self,
        input: &str,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, ConversationError> {
        let (thread_snapshot, controls) = {
            let mut state = self.state.lock().map_err(|_| {
                ConversationError::Settings("conversation state poisoned".to_string())
            })?;
            if state.active.is_some() {
                return Err(ConversationError::TurnAlreadyActive);
            }
            let controls = Arc::new(TurnControls::new());
            state.active = Some(Arc::clone(&controls));
            (state.thread.clone(), controls)
        };
        let params = TurnParams {
            thread: thread_snapshot.clone(),
            input: input.to_string(),
        };
        let result = self.runner.run(params, &controls, sink);
        let outcome_status = match &result {
            Ok(outcome) => Some(outcome.turn_status),
            // Terminalization 表示没有可信终态；保持上一投影不变。
            Err(TurnRunError::Terminalization(_)) => None,
            Err(_) => Some(TurnStatus::Failed),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::Settings("conversation state poisoned".to_string()))?;
        state.active = None;
        if let Some(status) = outcome_status {
            state.thread.last_turn_status = Some(match status {
                TurnStatus::Running => ThreadStatusSnapshot::Active,
                TurnStatus::Completed => ThreadStatusSnapshot::Completed,
                TurnStatus::Failed => ThreadStatusSnapshot::Failed,
                TurnStatus::Interrupted => ThreadStatusSnapshot::Interrupted,
            });
        }
        result.map_err(ConversationError::from)
    }
}

// ThreadStatus 快照别名，避免在闭包匹配中重复路径。
use crate::objects::ThreadStatus as ThreadStatusSnapshot;

/// 组合 `provider/model[#reasoning]` selector（与配置层拆分规则互逆）。
fn compose_model_selector(provider: &str, model: &str, reasoning: Option<&str>) -> String {
    let mut selector = format!("{provider}/{model}");
    if let Some(reasoning) = reasoning.filter(|value| !value.is_empty()) {
        selector.push('#');
        selector.push_str(reasoning);
    }
    selector
}

/// 便捷构造：home 路径下的会话目录。
pub fn sessions_dir_of(home: &std::path::Path) -> PathBuf {
    home.join(crate::store::SESSIONS_DIR_NAME)
}
