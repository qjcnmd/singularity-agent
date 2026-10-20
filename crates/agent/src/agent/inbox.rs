//! 用户消息和后台完成通知共用的内存输入箱，活动 turn 借用它的接受窗口。
//!
//! 接受、确认消费与停止窗口的关闭都在同一把 Mutex 内执行；保存期间不持锁。
//! 请求失败保留未消费输入。用户停止清掉用户输入，完成通知留到下一条用户消息后处理。
//! 输入仅随进程存在，交付时按来源保存为用户消息或内部通知。

use std::{
    collections::{BTreeMap, VecDeque},
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

impl UserInput {
    /// 工作台输入的信任边界，图片只在这里识别和解码一次。
    pub fn from_uploads(
        text: String,
        images: Vec<singularity_protocol::ImageUpload>,
        skills: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        if text.trim().is_empty() && images.is_empty() {
            return Err("输入不能为空。".into());
        }
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

/// 队列中的完整输入；消息身份在保存失败后的重试期间保持不变。
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedInput {
    pub input: QueuedInputKind,
    pub(super) message_id: String,
}

/// 区分用户意图与进程完成事实，内部通知不会显示为用户输入。
#[derive(Debug, Clone, PartialEq)]
pub enum QueuedInputKind {
    User(UserInput),
    Background { task_id: String, status: String },
}

impl QueuedInput {
    /// 接受输入时分配消息身份，复制与保存失败后的重试沿用同一身份。
    pub fn new(input: UserInput) -> Self {
        Self {
            input: QueuedInputKind::User(input),
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 后台完成事实只携带身份和终态，完整日志由 task_output 按需读取。
    pub fn background(task_id: String, status: String) -> Self {
        Self {
            input: QueuedInputKind::Background { task_id, status },
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 用户输入才出现在待处理消息和图片中。
    pub fn user_input(&self) -> Option<&UserInput> {
        match &self.input {
            QueuedInputKind::User(input) => Some(input),
            QueuedInputKind::Background { .. } => None,
        }
    }

    /// 待处理状态与正式用户消息共用的界面条目身份。
    pub fn item_id(&self) -> String {
        crate::session::text_item_id(&self.message_id, 0)
    }
}

/// 会话的输入箱；首条和追加消息按入队顺序交付。
#[derive(Debug, Default)]
pub struct SteeringInbox {
    entries: VecDeque<QueuedInput>,
}

impl SteeringInbox {
    /// 用户明确停止当前执行时取消尚未消费的输入。
    pub fn cancel(&mut self) {
        self.entries.retain(|entry| entry.user_input().is_none());
    }

    /// 调用方已确认当前会话状态允许接收输入。
    pub fn enqueue(&mut self, request: QueuedInput) {
        self.entries.push_back(request);
    }

    /// 按接受顺序借出尚未交付的输入，供会话快照和压缩后的执行读取。
    pub fn pending(&self) -> &VecDeque<QueuedInput> {
        &self.entries
    }

    /// 借出最早输入的保存副本；原输入一直归箱子持有，直到保存成功。
    pub(super) fn next(&self) -> Option<QueuedInput> {
        self.entries.front().cloned()
    }

    /// 只有当前 Agent 消费队首；保存期间停止可能已清空队列，下一轮会等当前回合结束。
    pub(super) fn confirm_saved(&mut self, message_id: &str) {
        if self.entries.front().is_some_and(|entry| entry.message_id == message_id) {
            self.entries.pop_front();
        }
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

/// 给活动 turn 的 inbox 加锁；锁被毒化时直接 fail-stop。
pub(super) fn lock_inbox(queue: &Mutex<SteeringInbox>) -> std::sync::MutexGuard<'_, SteeringInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
