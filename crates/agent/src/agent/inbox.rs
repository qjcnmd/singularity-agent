use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::{Arc, Mutex},
};

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

#[derive(Debug, Clone)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<crate::image::InputImage>,
    pub files: Vec<String>,
    pub annotations: Vec<protocol::MessageAnnotation>,
    skill_instructions: String,
}

impl UserInput {
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

fn escape_annotation_tags(text: &str) -> String {
    text.replace("</quote>", "<\\/quote>")
        .replace("</comment>", "<\\/comment>")
        .replace("</annotation>", "<\\/annotation>")
        .replace("</annotations>", "<\\/annotations>")
}

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

#[derive(Debug, Clone)]
pub struct QueuedInput {
    pub(super) input: QueuedInputKind,
    pub(super) message_id: String,
}

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

    pub(crate) fn background(task_id: String) -> Self {
        Self {
            input: QueuedInputKind::Background { task_id },
            message_id: crate::session::new_entry_id(),
        }
    }

    pub fn user_input(&self) -> Option<&UserInput> {
        match &self.input {
            QueuedInputKind::User(input) => Some(input),
            QueuedInputKind::Background { .. } => None,
        }
    }

    pub fn item_id(&self) -> String {
        crate::session::text_item_id(&self.message_id, 0)
    }
}

#[derive(Debug, Default)]
pub struct SteeringInbox {
    entries: VecDeque<QueuedInput>,
}

impl SteeringInbox {
    pub fn cancel(&mut self) {
        self.entries.retain(|entry| entry.user_input().is_none());
    }

    pub fn enqueue(&mut self, request: QueuedInput) {
        self.entries.push_back(request);
    }

    pub fn pending(&self) -> &VecDeque<QueuedInput> {
        &self.entries
    }

    pub(super) fn next(&self) -> Option<QueuedInput> {
        self.entries.front().cloned()
    }

    pub(super) fn confirm_saved(&mut self, message_id: &str) {
        if self.entries.front().is_some_and(|entry| entry.message_id == message_id) {
            self.entries.pop_front();
        }
    }
}

pub type SteeringInboxHandle = Arc<Mutex<SteeringInbox>>;

pub(super) fn lock_inbox(queue: &Mutex<SteeringInbox>) -> std::sync::MutexGuard<'_, SteeringInbox> {
    queue.lock().expect("turn inbox lock poisoned")
}
