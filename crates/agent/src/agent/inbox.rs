//! 任务的输入队列，用户消息和后台命令完成通知共用。
//!
//! 入队、确认已保存和清除用户输入共用一把锁；保存消息时不持锁。
//! 请求失败时输入留在队列里。用户停止会清掉用户消息，完成通知保留到下一条用户消息时处理。
//! 队列只在内存中；交给 Agent 时按来源保存为用户消息或内部通知。

use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, Mutex},
};

/// 文字与图片作为一个输入一起接受和交付。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
}

impl UserInput {
    /// 检查工作台提交的输入；图片只在这里识别和解码一次。
    pub fn from_uploads(
        text: String,
        images: Vec<singularity_protocol::ImageUpload>,
    ) -> Result<Self, String> {
        if text.trim().is_empty() && images.is_empty() {
            return Err("输入不能为空。".into());
        }
        Ok(Self {
            text,
            images: images.into_iter().map(crate::image::InputImage::upload).collect::<Result<_, _>>()?,
        })
    }

    /// 按当前目录展开空白分隔的 `/技能名`；手动输入与菜单选择共用，正文由模型读取。
    pub(super) fn model_text(&self, cwd: &std::path::Path, home: &std::path::Path) -> String {
        static REFERENCE: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"(?P<prefix>^|\s)/(?P<name>\S+)").expect("skill reference pattern is valid")
        });
        if !REFERENCE.is_match(&self.text) {
            return self.text.clone();
        }
        let skills = singularity_core::skills::SkillCatalog::discover(cwd, home);
        REFERENCE
            .replace_all(&self.text, |captures: &regex::Captures<'_>| {
                match skills
                    .skills
                    .iter()
                    .find(|skill| skill.user_invocable && skill.name == captures["name"])
                {
                    Some(skill) => {
                        format!("{}read {}", &captures["prefix"], singularity_core::display_path(&skill.path))
                    }
                    None => captures[0].to_string(),
                }
            })
            .into_owned()
    }
}

/// 队列中的一项输入；消息 ID 在入队时分配，保存失败后重试仍用同一个。
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedInput {
    pub input: QueuedInputKind,
    pub(super) message_id: String,
}

/// 区分用户消息和后台命令完成通知；通知不会显示成用户输入。
#[derive(Debug, Clone, PartialEq)]
pub enum QueuedInputKind {
    User(UserInput),
    Background { task_id: String, status: String },
}

impl QueuedInput {
    /// 接受输入时分配消息 ID。
    pub fn new(input: UserInput) -> Self {
        Self {
            input: QueuedInputKind::User(input),
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 完成通知只带命令 ID 和最终状态，输出由模型按需用 task_output 读取。
    pub fn background(task_id: String, status: String) -> Self {
        Self {
            input: QueuedInputKind::Background { task_id, status },
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 只有用户消息出现在待处理列表和图片查找中。
    pub fn user_input(&self) -> Option<&UserInput> {
        match &self.input {
            QueuedInputKind::User(input) => Some(input),
            QueuedInputKind::Background { .. } => None,
        }
    }

    /// 界面条目 ID；排队时和保存后相同，界面可以原地更新。
    pub fn item_id(&self) -> String {
        crate::session::text_item_id(&self.message_id, 0)
    }
}

/// 任务的输入队列；首条和后续消息按入队顺序交给 Agent。
#[derive(Debug, Default)]
pub struct SteeringInbox {
    entries: VecDeque<QueuedInput>,
}

impl SteeringInbox {
    /// 用户停止任务时清掉还没保存的用户消息，保留后台完成通知。
    pub fn cancel(&mut self) {
        self.entries.retain(|entry| entry.user_input().is_none());
    }

    /// 调用方负责确认任务当前可以接受输入。
    pub fn enqueue(&mut self, request: QueuedInput) {
        self.entries.push_back(request);
    }

    /// 还没保存的输入，按接受顺序排列。
    pub fn pending(&self) -> &VecDeque<QueuedInput> {
        &self.entries
    }

    /// 返回最早一项输入的副本；原项留在队列里，直到确认保存成功。
    pub(super) fn next(&self) -> Option<QueuedInput> {
        self.entries.front().cloned()
    }

    /// 保存成功后移除队首；保存期间用户停止可能已经清空队列，此时什么也不做。
    pub(super) fn confirm_saved(&mut self, message_id: &str) {
        if self.entries.front().is_some_and(|entry| entry.message_id == message_id) {
            self.entries.pop_front();
        }
    }
}

/// 任务、Agent 和后台命令共用的输入队列。
pub type SteeringInboxHandle = Arc<Mutex<SteeringInbox>>;

impl SteeringInbox {
    /// 新建一个由任务长期持有的输入队列。
    pub fn default_handle() -> SteeringInboxHandle {
        Arc::new(Mutex::new(Self::default()))
    }
}

/// 给输入队列加锁；锁中毒说明之前已经 panic，直接终止。
pub(super) fn lock_inbox(queue: &Mutex<SteeringInbox>) -> std::sync::MutexGuard<'_, SteeringInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
