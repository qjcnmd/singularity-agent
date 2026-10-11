//! 会话持有的 steer 输入箱，活动 turn 借用它的接受窗口。
//!
//! 接受、确认消费与停止窗口的关闭都在同一把 Mutex 内执行；保存期间不持锁。
//! 请求失败关闭当前接受窗口，尚未消费的输入留待下一次执行。用户停止取消当前 steer。
//! 输入仅随进程存在，交付时才保存为普通用户消息。

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
};

/// 文字与图片作为一个输入一起接受和交付。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
    /// 选择时确定的文件身份；交付时只展开引用，正文由模型调用 read 读取。
    pub skills: BTreeMap<String, String>,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            skills: BTreeMap::new(),
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
        skills: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        Ok(Self {
            text,
            skills,
            images: images.into_iter().map(crate::image::InputImage::upload).collect::<Result<_, _>>()?,
        })
    }

    /// 展开已选择的完整技能词，保留普通路径、URL 和其余用户文字。
    pub(super) fn model_text(&self) -> String {
        if self.skills.is_empty() {
            return self.text.clone();
        }
        static REFERENCE: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"(?P<prefix>^|[^\p{L}\p{N}_./~:])/(?P<name>\S+)")
                .expect("skill reference pattern is valid")
        });
        REFERENCE
            .replace_all(&self.text, |captures: &regex::Captures<'_>| {
                match self.skills.get(&captures["name"]) {
                    Some(path) => format!("{}{path}", &captures["prefix"]),
                    None => captures[0].to_string(),
                }
            })
            .into_owned()
    }
}

/// 进程内已接受的输入：序号确定控制顺序和身份，消息身份在重试期间保持不变。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub sequence: u64,
    pub input: UserInput,
    pub(super) message_id: String,
}

impl ControlRequest {
    /// 接受输入时分配消息身份，复制与保存失败后的重试沿用同一身份。
    pub fn new(sequence: u64, input: UserInput) -> Self {
        Self {
            sequence,
            input,
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 输入落盘后发布的用户消息条目身份，供宿主确认这一次输入已保存。
    pub fn item_id(&self) -> String {
        crate::session::text_item_id(&self.message_id, 0)
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

    /// 借出最早输入的保存副本；原输入一直归箱子持有，直到保存成功。
    pub(super) fn next(&self) -> Option<ControlRequest> {
        self.entries.first().cloned()
    }

    /// 保存期间允许停止清空箱子，因此按身份确认，而非移除队首。
    pub(super) fn acknowledge(&mut self, sequence: u64) {
        self.entries.retain(|request| request.sequence != sequence);
    }

    /// 自然停止处的原子屏障：仅在箱子为空时关闭窗口，有输入则留给下一轮消费。
    pub(super) fn close_if_empty(&mut self) -> bool {
        if !self.entries.is_empty() {
            return false;
        }
        self.closed = true;
        true
    }

    /// 关闭注入箱：此后的输入被拒绝；已收下但未保存的条目留待下次执行。
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
