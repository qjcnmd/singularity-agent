//! 活动 turn 的转向输入箱：单一输入通道与其锁纪律。
//!
//! `enqueue`、`drain` 与 `take_at_stop` 都在调用方持有的同一把 Mutex 内
//! 运行；turn 之间的后续输入队列由调用方的 Thread 协调器持有，不进入本箱。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TurnInboxState {
    #[default]
    Open,
    Closed,
}

/// 活动 turn 的单一转向输入箱。
///
/// 自然终止点调用 `take_at_stop` 时，箱内已有输入会被取出并继续执行；只有
/// 箱为空时才原子地转为 Closed，之后的输入明确拒绝。这保证不存在“已接受但
/// 丢失”的中间状态，也不引入持久队列或 grace period。
#[derive(Debug, Default)]
pub struct TurnInbox {
    state: TurnInboxState,
    entries: VecDeque<String>,
}

impl TurnInbox {
    pub fn enqueue(&mut self, text: impl Into<String>) -> bool {
        if self.state == TurnInboxState::Closed {
            return false;
        }
        self.entries.push_back(text.into());
        true
    }

    pub(super) fn drain(&mut self) -> Vec<String> {
        self.entries.drain(..).collect()
    }

    /// 自然停止点原子屏障：箱内已有输入时保持开启并交给下一轮消费；
    /// 箱为空时永久关闭，之后的输入明确拒绝（不存在“已接受但丢失”）。
    pub(super) fn take_at_stop(&mut self) -> Option<Vec<String>> {
        if self.entries.is_empty() {
            self.state = TurnInboxState::Closed;
            None
        } else {
            Some(self.drain())
        }
    }

    pub(super) fn close(&mut self) {
        self.state = TurnInboxState::Closed;
    }
}

/// 活动 turn 转向输入箱的线程安全句柄。
pub type TurnInboxHandle = Arc<Mutex<TurnInbox>>;

impl TurnInbox {
    /// 新建共享注入箱句柄：由生命周期所有者（TurnControls）构造时创建，
    /// 同一处句柄传给执行体（Agent），使注入窗口在构造时即已绑定。
    pub fn default_handle() -> TurnInboxHandle {
        Arc::new(Mutex::new(Self::default()))
    }
}

/// 加锁活动 turn inbox；共享协调状态中毒时 fail-stop，不能继续使用可能损坏的队列。
#[allow(clippy::expect_used)]
pub(super) fn lock_inbox(queue: &Mutex<TurnInbox>) -> std::sync::MutexGuard<'_, TurnInbox> {
    // 决策：Mutex 中毒 = 共享状态损坏 → fail-stop，不静默恢复。
    queue.lock().expect("turn inbox lock poisoned")
}
