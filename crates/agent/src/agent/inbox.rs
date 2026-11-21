//! 任务的输入队列，用户消息和后台命令完成通知共用。
//!
//! 入队、确认已保存和清除用户输入共用一把锁；保存消息时不持锁。
//! 请求失败时输入留在队列里。用户停止会清掉用户消息，完成通知保留到下一条用户消息时处理。
//! 队列只在内存中；交给 Agent 时按来源保存为用户消息或内部通知。

use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::{Arc, Mutex},
};

/// 控制接受用户输入时是否展开 `/skill` 引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillExpansion {
    Enabled,
    Disabled,
}

impl SkillExpansion {
    pub fn from_disabled_modules(disabled_modules: &[String]) -> Self {
        if disabled_modules.iter().any(|id| id == super::modules::SKILLS_MODULE_ID) {
            Self::Disabled
        } else {
            Self::Enabled
        }
    }
}

/// 文字、图片、附件路径和批注作为一个输入一起接受和交付。
#[derive(Debug, Clone)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
    /// 附件文件的绝对路径；只交给模型路径，由模型用读取工具查看内容。
    pub files: Vec<String>,
    /// 用户对历史助手回复片段添加的文字批注。
    pub annotations: Vec<protocol::MessageAnnotation>,
    /// 接受输入时读取的技能全文；原始任务文字单独保留供界面显示。
    skill_instructions: String,
}

impl UserInput {
    /// 接受输入前完成校验、图片解码和技能读取；失败时不进入队列。
    pub fn from_uploads(
        text: String,
        images: Vec<protocol::ImageUpload>,
        files: Vec<String>,
        annotations: Vec<protocol::MessageAnnotation>,
        cwd: &std::path::Path,
        home: &std::path::Path,
        skill_expansion: SkillExpansion,
    ) -> Result<Self, String> {
        let mut clean_annotations = Vec::with_capacity(annotations.len());
        for annotation in annotations {
            let message_id = annotation.message_id.trim().to_owned();
            if message_id.is_empty() {
                return Err("批注缺少目标消息标识。".into());
            }
            let quote = annotation.quote.trim().to_owned();
            if quote.is_empty() {
                return Err("批注引用内容不能为空。".into());
            }
            clean_annotations.push(protocol::MessageAnnotation {
                message_id,
                quote,
                comment: annotation.comment.trim().to_owned(),
            });
        }
        if text.trim().is_empty() && images.is_empty() && files.is_empty() && clean_annotations.is_empty() {
            return Err("输入不能为空。".into());
        }
        let mut seen_files = std::collections::HashSet::new();
        let mut unique_files = Vec::with_capacity(files.len());
        for path in files {
            let file = std::path::Path::new(&path);
            if !file.is_absolute() {
                return Err(format!("附件路径必须是绝对路径：{path}"));
            }
            if !file.is_file() {
                return Err(format!("找不到附件：{path}"));
            }
            if seen_files.insert(path.clone()) {
                unique_files.push(path);
            }
        }
        let skill_instructions = match skill_expansion {
            SkillExpansion::Enabled => load_selected_skills(
                std::iter::once(text.as_str())
                    .chain(clean_annotations.iter().map(|item| item.comment.as_str())),
                cwd,
                home,
            )?,
            SkillExpansion::Disabled => String::new(),
        };
        Ok(Self {
            skill_instructions,
            text,
            images: images.into_iter().map(crate::image::InputImage::upload).collect::<Result<_, _>>()?,
            files: unique_files,
            annotations: clean_annotations,
        })
    }

    /// 模型看到的文字：批注区块（如有）在最前，接原始任务、所选技能，附件路径列在最后。
    pub(super) fn model_text(&self) -> String {
        let mut text = String::new();
        if !self.annotations.is_empty() {
            text.push_str("<annotations>");
            for (index, annotation) in self.annotations.iter().enumerate() {
                let _ = write!(
                    text,
                    "\n<annotation index=\"{}\">\n<quote>\n{}\n</quote>",
                    index + 1,
                    escape_annotation_tags(&annotation.quote)
                );
                if !annotation.comment.is_empty() {
                    let _ = write!(
                        text,
                        "\n<comment>\n{}\n</comment>",
                        escape_annotation_tags(&annotation.comment)
                    );
                }
                text.push_str("\n</annotation>");
            }
            text.push_str("\n</annotations>");
        }
        if !self.text.trim().is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&self.text);
        } else if text.is_empty() {
            text.push_str(&self.text);
        }
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

/// 转义批注正文中可能提前闭合结构区块的结束标签。
fn escape_annotation_tags(text: &str) -> String {
    text.replace("</quote>", "<\\/quote>")
        .replace("</comment>", "<\\/comment>")
        .replace("</annotation>", "<\\/annotation>")
        .replace("</annotations>", "<\\/annotations>")
}

/// 按空白分隔的引用顺序读取技能，同一技能只加载一次，未知名称不影响原文。
fn load_selected_skills<'a>(
    texts: impl IntoIterator<Item = &'a str>,
    cwd: &std::path::Path,
    home: &std::path::Path,
) -> Result<String, String> {
    let mut references = texts
        .into_iter()
        .flat_map(str::split_whitespace)
        .filter_map(|word| word.strip_prefix('/'))
        .filter(|name| !name.is_empty())
        .peekable();
    if references.peek().is_none() {
        return Ok(String::new());
    }
    let catalog = app_core::skills::SkillCatalog::discover(cwd, home);
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
    pub(super) input: QueuedInputKind,
    pub(super) message_id: String,
}

/// 区分用户消息和后台命令完成通知；通知不会显示成用户输入。
#[derive(Debug, Clone)]
pub(super) enum QueuedInputKind {
    User(UserInput),
    Background { task_id: String },
}

impl QueuedInput {
    pub fn new(input: UserInput) -> Self {
        Self {
            input: QueuedInputKind::User(input),
            message_id: crate::session::new_entry_id(),
        }
    }

    /// 队列只保存命令 ID；交给模型时从后台管理器取未读输出与终态。
    pub(crate) fn background(task_id: String) -> Self {
        Self {
            input: QueuedInputKind::Background { task_id },
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
