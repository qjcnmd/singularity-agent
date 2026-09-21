//! 活动 turn 的转向输入箱：唯一入口，以及配套的加锁约定。
//!
//! enqueue、drain 与 take_at_stop 都在调用方持有的同一把 Mutex 内执行；一轮结束后
//! 才到来的输入由 Thread 协调器另排队列，不进本箱。控制请求只随进程存在，接受、
//! 排队与处置都不落盘。

use std::sync::{Arc, Mutex};

use singularity_protocol::{ControlChannel, ControlDisposition, ControlSnapshot, wire_word};

/// 控制请求在运行期的载体，不参与序列化：identity、正文与接受顺序都在接受时组装好，
/// 只活在当前进程内。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub control_id: String,
    /// 这条输入所属的 turn；排队等自己那一轮的输入，开始执行前是 None。
    pub turn_id: Option<String>,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: String,
}

/// 控制 identity 唯一的构造入口，格式 {channel_word}:{sequence}。转向输入与排队输入
/// 共用同一条接受序号，因此 identity 不会重复，也不要求 turn 已经存在。
pub fn control_id(channel: ControlChannel, sequence: u64) -> String {
    let channel_word = wire_word(channel);
    format!("{channel_word}:{sequence}")
}

impl ControlRequest {
    /// 把当前控制事实投影成对外快照；只反映进程内的队列状态。
    pub fn snapshot(&self, disposition: ControlDisposition) -> ControlSnapshot {
        ControlSnapshot {
            control_id: self.control_id.clone(),
            turn_id: self.turn_id.clone(),
            channel: self.channel,
            sequence: self.sequence,
            text: self.text.clone(),
            disposition,
        }
    }

    /// 这条输入开始执行自己那一轮：把控制身份绑定到该 turn。
    pub fn bound_to(&self, turn_id: &str) -> Self {
        Self {
            turn_id: Some(turn_id.to_string()),
            ..self.clone()
        }
    }

    /// 退回队列继续等待执行：它不再属于任何已经开始的 turn。
    pub fn unbound(mut self) -> Self {
        self.turn_id = None;
        self
    }
}

/// 活动 turn 唯一的转向输入箱：条目携带协调器分配的接受顺序 sequence（FIFO 的权威
/// 依据）和运行期控制 identity。
#[derive(Debug, Default)]
pub struct TurnInbox {
    closed: bool,
    entries: Vec<ControlRequest>,
}

impl TurnInbox {
    pub fn enqueue(&mut self, request: ControlRequest) -> bool {
        self.enqueue_all([request])
    }

    /// 在同一次临界区内整批接收已接受的输入；窗口已关闭时一条都不收，因此批量
    /// 「立即发送」要么整批交付、要么整批留在原队列。
    pub fn enqueue_all(&mut self, requests: impl IntoIterator<Item = ControlRequest>) -> bool {
        if self.closed {
            return false;
        }
        self.entries.extend(requests);
        true
    }

    /// 取走所有未交付条目，按 sequence 升序返回（FIFO 以它为准）。
    pub fn drain(&mut self) -> Vec<ControlRequest> {
        let mut drained = std::mem::take(&mut self.entries);
        drained.sort_by_key(|request| request.sequence);
        drained
    }

    /// 把投递前失败的已接受输入还回箱内：关闭窗口只拒绝新的接受，已收下的不能丢。
    pub(super) fn restore(&mut self, requests: impl IntoIterator<Item = ControlRequest>) {
        self.entries.extend(requests);
    }

    /// turn 自然停止处的原子屏障：箱内已有输入就保持开启，交给下一轮消费；箱为空则
    /// 永久关闭，此后输入明确拒绝——不会出现「已经接受却丢失」的中间状态。
    pub(super) fn take_at_stop(&mut self) -> Option<Vec<ControlRequest>> {
        if self.entries.is_empty() {
            self.closed = true;
            None
        } else {
            Some(self.drain())
        }
    }

    /// 关闭注入箱：此后的输入被拒绝；已收下但未交付的条目仍留待 drain 取走。
    pub fn close(&mut self) {
        self.closed = true;
    }
}

/// 活动 turn 输入箱的线程安全句柄，可在多个执行体之间共享。
pub type TurnInboxHandle = Arc<Mutex<TurnInbox>>;

impl TurnInbox {
    /// 新建共享注入箱句柄；由生命周期所有者在构造控制面时创建。
    pub fn default_handle() -> TurnInboxHandle {
        Arc::new(Mutex::new(Self::default()))
    }
}

/// 给活动 turn 的 inbox 加锁。共享状态被毒化时直接 fail-stop：可能已损坏的队列
/// 不能继续用。
#[allow(clippy::expect_used)]
pub(super) fn lock_inbox(queue: &Mutex<TurnInbox>) -> std::sync::MutexGuard<'_, TurnInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
