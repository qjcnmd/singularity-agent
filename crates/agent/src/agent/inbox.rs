//! 任务的输入队列，用户消息和后台命令完成通知共用。
//!
//! 入队、确认已保存和清除用户输入共用一把锁；保存消息时不持锁。
//! 请求失败时输入留在队列里。用户停止会清掉用户消息，完成通知保留到下一条用户消息时处理。
//! 队列只在内存中；交给 Agent 时按来源保存为用户消息或内部通知。

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

/// 文字、图片和附件路径作为一个输入一起接受和交付。
#[derive(Debug, Clone)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
    /// 附件文件的绝对路径；只交给模型路径，由模型用读取工具查看内容。
    pub files: Vec<String>,
    /// 接受输入时读取的技能全文；原始任务文字单独保留供界面显示。
    skill_instructions: String,
}

impl UserInput {
    /// 接受输入前完成校验、图片解码和技能读取；失败时不进入队列。
    pub fn from_uploads(
        text: String,
        images: Vec<singularity_protocol::ImageUpload>,
        files: Vec<String>,
        cwd: &std::path::Path,
        home: &std::path::Path,
    ) -> Result<Self, String> {
        if text.trim().is_empty() && images.is_empty() && files.is_empty() {
            return Err("输入不能为空。".into());
        }
        if let Some(missing) = files.iter().find(|path| !std::path::Path::new(path).is_file()) {
            return Err(format!("找不到附件：{missing}"));
        }
        Ok(Self {
            skill_instructions: load_selected_skills(&text, cwd, home)?,
            text,
            images: images.into_iter().map(crate::image::InputImage::upload).collect::<Result<_, _>>()?,
            files,
        })
    }

    /// 模型看到的文字：原始任务后附所选技能，附件路径列在最后。
    pub(super) fn text_with_files(&self) -> String {
        let mut text = self.text.clone();
        text.push_str(&self.skill_instructions);
        if !self.files.is_empty() {
            if text.trim().is_empty() {
                text.clear();
            } else {
                text.push_str("\n\n");
            }
            text.push_str("附件：");
            for path in &self.files {
                text.push_str("\n- ");
                text.push_str(path);
            }
        }
        text
    }
}

/// 按空白分隔的引用顺序读取技能，同一技能只加载一次，未知名称不影响原文。
fn load_selected_skills(text: &str, cwd: &std::path::Path, home: &std::path::Path) -> Result<String, String> {
    let mut references = text
        .split_whitespace()
        .filter_map(|word| word.strip_prefix('/'))
        .filter(|name| !name.is_empty())
        .peekable();
    if references.peek().is_none() {
        return Ok(String::new());
    }
    let catalog = singularity_core::skills::SkillCatalog::discover(cwd, home);
    let mut selected = std::collections::HashSet::new();
    let mut instructions = String::new();
    for name in references {
        let Some(skill) = catalog.skills.iter().find(|skill| skill.user_invocable && skill.name == name)
        else {
            continue;
        };
        if selected.insert(&skill.path) {
            let content = skill.load_instructions()?;
            instructions.push_str("\n\n");
            instructions.push_str(&content);
        }
    }
    Ok(instructions)
}

/// 队列中的一项输入；消息 ID 在入队时分配，保存失败后重试仍用同一个。
#[derive(Debug, Clone)]
pub struct QueuedInput {
    pub input: QueuedInputKind,
    pub(super) message_id: String,
}

/// 区分用户消息和后台命令完成通知；通知不会显示成用户输入。
#[derive(Debug, Clone)]
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

/// 给输入队列加锁；锁中毒说明之前已经 panic，直接终止。
pub(super) fn lock_inbox(queue: &Mutex<SteeringInbox>) -> std::sync::MutexGuard<'_, SteeringInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
