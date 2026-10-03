//! 会话持有的 steer 输入箱，活动 turn 借用它的接受窗口。
//!
//! enqueue、drain 与 take_at_stop 都在调用方持有的同一把 Mutex 内执行。
//! 请求失败关闭当前接受窗口，尚未消费的输入留待下一次执行。用户停止取消当前 steer。
//! 输入仅随进程存在，交付时才保存为普通用户消息。

use std::sync::{Arc, Mutex};

use singularity_protocol::PendingInput;

/// 文字与图片作为一个输入一起接受、编辑和交付。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

impl UserInput {
    /// 工作台输入的信任边界，图片只在这里识别和解码一次。
    pub fn from_uploads(
        text: String,
        images: Vec<singularity_protocol::ImageUpload>,
    ) -> Result<Self, String> {
        Ok(Self {
            text,
            images: images
                .into_iter()
                .map(crate::image::InputImage::upload)
                .collect::<Result<_, _>>()?,
        })
    }
}

/// 进程内已接受的输入：序号确定顺序和身份，正文只保存一份。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub sequence: u64,
    pub input: UserInput,
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
            text: self.input.text.clone(),
            images: self
                .input
                .images
                .iter()
                .map(|image| image.attachment.clone())
                .collect(),
        }
    }
}

/// 会话的转向输入箱；条目按协调器分配的接受序号交付。
#[derive(Debug, Default)]
pub struct SteeringInbox {
    closed: bool,
    entries: Vec<ControlRequest>,
}

impl SteeringInbox {
    /// 开始新的执行时开放接受窗口，已有输入保持原位。
    pub fn open(&mut self) {
        self.closed = false;
    }

    /// 用户明确停止当前执行时取消其尚未消费的 steer。
    pub fn cancel(&mut self) {
        self.close();
        self.entries.clear();
    }

    pub fn enqueue(&mut self, request: ControlRequest) -> bool {
        if self.closed {
            return false;
        }
        self.entries.push(request);
        true
    }

    /// 取走所有未交付条目，按 sequence 升序返回（FIFO 以它为准）。
    pub fn drain(&mut self) -> Vec<ControlRequest> {
        let mut drained = std::mem::take(&mut self.entries);
        drained.sort_by_key(|request| request.sequence);
        drained
    }

    /// 新提交前先交付较早接受的 steer，保持跨执行的输入顺序。
    pub(super) fn drain_before(&mut self, sequence: u64) -> Vec<ControlRequest> {
        let (earlier, later) = self
            .drain()
            .into_iter()
            .partition(|request| request.sequence < sequence);
        self.entries = later;
        earlier
    }

    /// turn 自然停止处的原子屏障：箱内已有输入就保持开启，交给下一轮消费；箱为空则
    /// 关闭本轮接受窗口，此后的 steer 明确拒绝。
    pub(super) fn take_at_stop(&mut self) -> Option<Vec<ControlRequest>> {
        if self.entries.is_empty() {
            self.closed = true;
            None
        } else {
            Some(self.drain())
        }
    }

    /// 查找仍未消费的图片，预览不改变队列。
    pub fn image(&self, id: &str) -> Option<crate::image::InputImage> {
        self.entries
            .iter()
            .flat_map(|request| &request.input.images)
            .find(|image| image.attachment.id == id)
            .cloned()
    }

    /// 关闭注入箱：此后的输入被拒绝；已收下但未交付的条目仍留待 drain 取走。
    pub fn close(&mut self) {
        self.closed = true;
    }
}

/// 会话输入箱的线程安全句柄，各次执行借用同一个箱子。
pub type SteeringInboxHandle = Arc<Mutex<SteeringInbox>>;

impl SteeringInbox {
    /// 新建共享输入箱；会话长期持有，独立压缩使用自己的空箱子。
    pub fn default_handle() -> SteeringInboxHandle {
        Arc::new(Mutex::new(Self::default()))
    }
}

/// 给活动 turn 的 inbox 加锁。共享状态被毒化时直接 fail-stop：可能已损坏的队列
/// 不能继续用。
pub(super) fn lock_inbox(queue: &Mutex<SteeringInbox>) -> std::sync::MutexGuard<'_, SteeringInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
