//! 活动 turn 的转向输入箱：唯一入口，以及配套的加锁约定。
//!
//! enqueue、drain 与 take_at_stop 都在调用方持有的同一把 Mutex 内执行；一轮结束后
//! 才到来的输入由 Thread 协调器另排队列，不进本箱。控制请求只随进程存在，接受、
//! 排队与处置都不落盘。

use std::sync::{Arc, Mutex};

use singularity_protocol::PendingInput;

/// 进程内已接受的输入：序号确定顺序和身份，正文只保存一份。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub sequence: u64,
    pub text: String,
}

impl ControlRequest {
    /// 进程内接受序号同时确定不透明控制身份，不另存一份字符串。
    pub fn control_id(&self) -> String {
        self.sequence.to_string()
    }

    /// 待处理输入只公开身份和正文；接受序号留在协调器内部。
    pub fn pending(&self) -> PendingInput {
        PendingInput {
            control_id: self.control_id(),
            text: self.text.clone(),
        }
    }
}

/// 活动 turn 的转向输入箱；条目按协调器分配的接受序号交付。
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
pub(super) fn lock_inbox(queue: &Mutex<TurnInbox>) -> std::sync::MutexGuard<'_, TurnInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
