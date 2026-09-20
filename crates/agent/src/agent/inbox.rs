//! 活动 turn 的转向输入箱：单一输入通道与其锁纪律。
//!
//! enqueue、drain 与 take_at_stop 都在调用方持有的同一把 Mutex 内
//! 运行；turn 之间的后续输入队列由调用方的 Thread 协调器持有，不进入本箱。
//! 条目携带 ControlRequest（包含协调器分配的接受顺序 sequence 与
//! 控制 identity），drain 按 sequence 升序输出确保 FIFO 投递。控制请求本身
//! 只随进程存在：接受、排队与处置都不落盘，因此它的类型与 identity 构造
//! 归属这里，而不是会话落盘格式。

use std::sync::{Arc, Mutex};

use singularity_protocol::{ControlChannel, ControlDisposition, ControlSnapshot, wire_word};

/// 控制请求的运行时载体（不参与序列化）：接受时组装的稳定 identity、
/// payload 与接受顺序。它随所在进程的生命周期存在，控制队列与处置都不落盘。
/// control_id 使用 {channel_word}:{sequence} 格式；接受序号在一个会话内单调
/// 递增，因此 identity 唯一，且不随 turn 关联的变化而改变。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub control_id: String,
    /// 本条输入绑定到的 turn；等待自己那一轮的排队输入在开始执行前为 None。
    pub turn_id: Option<String>,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: String,
}

/// 控制 identity 的单点构造形式：{channel_word}:{sequence}。channel_word 是
/// ControlChannel 的 serde snake_case 词形；steer 与排队输入共用一条接受序号，
/// identity 因此在一个会话内唯一，且不依赖 turn 是否已经存在。
pub fn control_id(channel: ControlChannel, sequence: u64) -> String {
    let channel_word = wire_word(channel);
    format!("{channel_word}:{sequence}")
}

impl ControlRequest {
    /// 当前控制事实的公开投影（同一字段映射服务 pending/执行/取消各处置）；
    /// 它只描述当前进程内的队列状态，不承诺重启后可恢复。
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

    /// 本条输入开始执行自己的 turn：把控制身份关联到那个 turn。
    pub fn bound_to(&self, turn_id: &str) -> Self {
        Self {
            turn_id: Some(turn_id.to_string()),
            ..self.clone()
        }
    }

    /// 交回队列等待执行：它不再属于任何已开始的 turn。
    pub fn unbound(mut self) -> Self {
        self.turn_id = None;
        self
    }
}

/// 活动 turn 的单一转向输入箱。
///
/// 自然终止点调用 take_at_stop 时，箱内已有输入会被取出并继续执行；只有
/// 箱为空时才原子地转为 Closed，之后的输入明确拒绝。这保证不存在“已接受但
/// 丢失”的中间状态，也不引入持久队列或 grace period。条目携带协调器分配的
/// 接受顺序 sequence（FIFO 权威）与运行期控制 identity。
#[derive(Debug, Default)]
pub struct TurnInbox {
    closed: bool,
    entries: Vec<ControlRequest>,
}

impl TurnInbox {
    pub fn enqueue(&mut self, request: ControlRequest) -> bool {
        self.enqueue_all([request])
    }

    /// 同一次临界区内接收整批已接受输入：注入窗口已关闭时一条都不接收。批量
    /// “立即发送”用它表达“要么整批交付、要么整批留在原队列”，不存在部分交付。
    pub fn enqueue_all(&mut self, requests: impl IntoIterator<Item = ControlRequest>) -> bool {
        if self.closed {
            return false;
        }
        self.entries.extend(requests);
        true
    }

    /// 取走全部未交付条目，按 sequence 升序排序后返回（FIFO 权威）。
    pub fn drain(&mut self) -> Vec<ControlRequest> {
        let mut drained = std::mem::take(&mut self.entries);
        drained.sort_by_key(|request| request.sequence);
        drained
    }

    /// 归还投递前失败的已接受输入；关闭窗口只拒绝新的接受，
    /// 不得丢弃已有的 control。
    pub(super) fn restore(&mut self, requests: impl IntoIterator<Item = ControlRequest>) {
        self.entries.extend(requests);
    }

    /// 自然停止点原子屏障：箱内已有输入时保持开启并交给下一轮消费；
    /// 箱为空时永久关闭，之后的输入明确拒绝（不存在“已接受但丢失”）。
    pub(super) fn take_at_stop(&mut self) -> Option<Vec<ControlRequest>> {
        if self.entries.is_empty() {
            self.closed = true;
            None
        } else {
            Some(self.drain())
        }
    }

    /// 关闭注入箱：之后的输入被拒绝；已接受而未交付的条目保留在箱内，
    /// 由终态排水（drain）取走并给出归宿。
    pub fn close(&mut self) {
        self.closed = true;
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
