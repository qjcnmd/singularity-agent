//! 上下文视图：从会话 ledger 派生出模型请求的输入，并统一计量。
//!
//! 只从持久账本派生对话历史；当前文件指令由 Agent 在请求装配时加入。
//! 请求装配、压缩判定与溢出恢复共用这一个视图。原始会话始终由会话 ledger 持有。

mod resolution;

use self::resolution::{apply_context_entry, resolve_context_entries};

use singularity_model::{ModelMessage, ModelRole};

use crate::message::{AgentMessage, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock};

use super::format::{LedgerRecord, SessionEntry};
use super::manager::SessionData;

/// 按 UTF-16 字符数做启发式 Token 估算（ceil(chars / 4)），全仓只此一份实现。
pub(crate) fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

fn compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

const UNKNOWN_TOOL_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";

/// 只在模型输入里闭合工具单元；缺失结果不作为真实执行事实写回会话。
fn project_messages(entries: &[ContextPosition], session: &SessionData) -> Vec<ContextMessage> {
    let mut messages = Vec::new();
    let mut positions = entries.iter().peekable();
    while let Some(position) = positions.next() {
        messages.push(position.model_message(session));
        let SessionEntry::Message { message, .. } = position.entry(session) else {
            continue;
        };
        for call in message.tool_calls() {
            let result = positions.next_if(|candidate| {
                matches!(candidate.entry(session), SessionEntry::Message { message, .. }
                    if message.tool_call_id() == Some(call.tool_call_id.as_str()))
            });
            messages.push(match result {
                Some(position) => position.model_message(session),
                None => {
                    let mut result = ModelMessage::text(ModelRole::Tool, UNKNOWN_TOOL_OUTCOME);
                    result.tool_call_id = Some(call.tool_call_id.clone());
                    ContextMessage { message: result, images: Vec::new() }
                }
            });
        }
    }
    messages
}

fn context_token_estimate(entries: &[ContextPosition], session: &SessionData) -> u64 {
    let mut tokens = 0;
    let mut calls = 0;
    let mut results = 0;
    for position in entries {
        tokens += position.token_estimate(session);
        if let SessionEntry::Message { message, .. } = position.entry(session) {
            calls += message.tool_calls().count();
            results += usize::from(message.tool_call_id().is_some());
        }
    }
    let unknown_result_tokens =
        content_token_estimate(&[ContentBlock::Text { text: UNKNOWN_TOOL_OUTCOME.to_string() }], true);
    tokens + (calls - results) as u64 * unknown_result_tokens
}

enum ContextEntry<'a> {
    Message(&'a AgentMessage),
    Summary(&'a str),
    SkillInstructions(&'a str),
}

/// 把 ledger 条目穷尽分类；计量、保留边界和请求投影共用这一处可见性判断。
fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::Compaction { compaction, .. } => Some(ContextEntry::Summary(&compaction.summary)),
        SessionEntry::Metadata { .. } => None,
        SessionEntry::Record { record, .. } => match record {
            LedgerRecord::SkillInstructions { text } => Some(ContextEntry::SkillInstructions(text)),
            LedgerRecord::ToolResultPruned { .. }
            | LedgerRecord::AssistantInterrupted { .. }
            | LedgerRecord::ModelRequest { .. }
            | LedgerRecord::RequestDefinitions { .. }
            | LedgerRecord::OperationStarted { .. }
            | LedgerRecord::OperationFinished { .. } => None,
        },
    }
}

/// 估算可压缩历史的模型内容；当前文件指令由 Agent 另行计量。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => message_token_estimate(message),
        Some(ContextEntry::Summary(summary)) => estimate_tokens_of(&compaction_summary(summary)) + 8,
        Some(ContextEntry::SkillInstructions(text)) => estimate_tokens_of(text) + 8,
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &crate::message::AgentMessage) -> u64 {
    content_token_estimate(message.content(), matches!(message, AgentMessage::ToolResult { .. }))
}

/// 只估算模型请求真正会带上的内容：公开思考不进入文本投影
/// （见 `ContextPosition::model_message`），所以计零。
fn content_token_estimate(content: &[ContentBlock], tool_result: bool) -> u64 {
    let mut tokens = 4u64;
    for block in content {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text } => estimate_tokens_of(text) + 4,
            ContentBlock::Thinking { .. } => 0,
            // 仅用于发送前的启发式压力判断；实际用量仍由提供方 usage 校正。
            ContentBlock::Image(image) => {
                u64::from(image.width).div_ceil(32) * u64::from(image.height).div_ceil(32) + 128
            }
            ContentBlock::ToolCall(call) => {
                estimate_tokens_of(&call.tool_name) + estimate_tokens_of(&call.arguments.to_string()) + 4
            }
        });
    }
    if tool_result {
        tokens += 4;
    }
    tokens
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ContextView {
    entries: Vec<ContextPosition>,
    /// 历史与模型投影补入结果的估算值；usage 基线缺失时用它计量。
    estimated_tokens: u64,
}

/// 已按工具配对边界选好的摘要前缀。
pub(crate) struct CompactionPrefix {
    pub(crate) messages: Vec<ContextMessage>,
    pub(crate) image_directory: std::path::PathBuf,
    /// 选中历史的已有估价；上一份摘要由摘要请求的更新指令另行计量。
    pub(crate) estimated_tokens: u64,
    pub(crate) previous_summary: Option<String>,
    pub(crate) first_kept_entry_id: String,
}

impl ContextView {
    /// 按已保存的压缩与剪枝边界还原模型历史；边界在生成记录时确定。
    pub(crate) fn derive(session: &SessionData) -> Self {
        let entries = resolve_context_entries(session);
        let estimated_tokens = context_token_estimate(&entries, session);
        Self { entries, estimated_tokens }
    }

    pub(crate) fn messages(&self, session: &SessionData) -> Vec<ContextMessage> {
        project_messages(&self.entries, session)
    }

    pub(crate) fn compaction_prefix(
        &self,
        session: &SessionData,
        keep_recent_tokens: u64,
    ) -> Option<CompactionPrefix> {
        let entries = &self.entries;
        let cut = find_cut_point(entries, session, keep_recent_tokens);
        if cut == 0 {
            return None;
        }
        let previous_summary = match entries[0].entry(session) {
            SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
            _ => None,
        };
        let selected = &entries[usize::from(previous_summary.is_some())..cut];
        let messages = project_messages(selected, session);
        if messages.is_empty() {
            return None;
        }
        Some(CompactionPrefix {
            messages,
            image_directory: session.image_directory(),
            estimated_tokens: context_token_estimate(selected, session),
            previous_summary,
            first_kept_entry_id: entries[cut].entry(session).id().to_string(),
        })
    }

    pub(crate) fn pruned_tool_results(&self, session: &SessionData) -> Vec<LedgerRecord> {
        self.entries
            .iter()
            .filter_map(|position| match position.entry(session) {
                SessionEntry::Message {
                    id,
                    message: AgentMessage::ToolResult { .. },
                    ..
                } => {
                    // tool_result_message 与剪枝记录都以正文开头，后接图片。
                    let [ContentBlock::Text { text }, images @ ..] = position.content(session) else {
                        unreachable!("tool results start with a text block");
                    };
                    crate::compaction::prune_tool_text(text).map(|text| {
                        let mut content = vec![ContentBlock::Text { text }];
                        content.extend_from_slice(images);
                        LedgerRecord::ToolResultPruned { entry_id: id.clone(), content }
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// 已提交历史的缓存估价；本轮请求开销与实测校正由 Agent 添加。
    pub(crate) fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    /// 把刚提交的日志位置推进到视图里；追加与重新打开走同一套排序规则。
    pub(crate) fn append_entry(&mut self, session: &SessionData, index: usize) {
        let entry = &session.entries()[index];
        let replaced =
            apply_context_entry(&mut self.entries, ContextPosition { index, pruned_index: None }, session);
        if let Some(previous) = replaced {
            // 剪枝只替换结果内容，工具配对及其补齐估价不变。
            let current = ContextPosition { pruned_index: Some(index), ..previous };
            self.estimated_tokens =
                self.estimated_tokens - previous.token_estimate(session) + current.token_estimate(session);
        } else if matches!(entry, SessionEntry::Compaction { .. }) {
            self.estimated_tokens = context_token_estimate(&self.entries, session);
        } else if is_context_entry(entry) {
            self.estimated_tokens = self.estimated_tokens.saturating_add(entry_token_estimate(entry));
        }
    }
}

/// 日志只追加，所以位置在同一份 SessionData 内始终稳定；剪枝正文仍由原始日志持有。
#[derive(Debug, Clone, Copy)]
struct ContextPosition {
    index: usize,
    pruned_index: Option<usize>,
}

impl ContextPosition {
    fn entry<'a>(&self, session: &'a SessionData) -> &'a SessionEntry {
        &session.entries()[self.index]
    }

    fn content<'a>(&self, session: &'a SessionData) -> &'a [ContentBlock] {
        if let Some(index) = self.pruned_index {
            let SessionEntry::Record {
                record: LedgerRecord::ToolResultPruned { content, .. },
                ..
            } = &session.entries()[index]
            else {
                unreachable!("pruned position references its ledger record");
            };
            return content;
        }
        let SessionEntry::Message { message, .. } = self.entry(session) else {
            unreachable!("content is only read for messages");
        };
        message.content()
    }

    fn token_estimate(&self, session: &SessionData) -> u64 {
        match self.entry(session) {
            SessionEntry::Message { message, .. } => content_token_estimate(
                self.content(session),
                matches!(message, AgentMessage::ToolResult { .. }),
            ),
            entry => entry_token_estimate(entry),
        }
    }

    /// 对话历史、Skill 与摘要前缀共用同一套消息投影；文件指令由 Agent 加入。
    fn model_message(&self, session: &SessionData) -> ContextMessage {
        let message = match context_entry(self.entry(session))
            .expect("context position references a context entry")
        {
            ContextEntry::Message(message) => match message {
                AgentMessage::User { .. } => {
                    ModelMessage::text(ModelRole::User, crate::message::content_text(self.content(session)))
                }
                AgentMessage::Assistant { .. } => ModelMessage {
                    role: ModelRole::Assistant,
                    content: crate::message::content_text(self.content(session)),
                    images: Vec::new(),
                    tool_call_id: None,
                    tool_calls: message.tool_calls().cloned().collect(),
                    provider_reasoning_replay: message.provider_reasoning_replay().cloned(),
                },
                AgentMessage::ToolResult { tool_call_id, .. } => {
                    let mut llm = ModelMessage::text(
                        ModelRole::Tool,
                        crate::message::content_text(self.content(session)),
                    );
                    llm.tool_call_id = Some(tool_call_id.clone());
                    llm
                }
            },
            ContextEntry::Summary(summary) => {
                ModelMessage::text(ModelRole::User, compaction_summary(summary))
            }
            ContextEntry::SkillInstructions(text) => ModelMessage::text(ModelRole::User, text),
        };
        let images = match self.entry(session) {
            SessionEntry::Message { .. } => self
                .content(session)
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image(image) => Some(image.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        ContextMessage { message, images }
    }
}

pub(crate) struct ContextMessage {
    message: ModelMessage,
    images: Vec<singularity_protocol::ImageAttachment>,
}

/// 生成和摘要共用的模型投影。图片读取发生在写者锁之外。
pub(crate) fn load_messages(
    materials: Vec<ContextMessage>,
    directory: &std::path::Path,
) -> super::Result<Vec<ModelMessage>> {
    materials
        .into_iter()
        .map(|material| {
            let mut message = material.message;
            for (index, image) in material.images.iter().enumerate() {
                let path = directory.join(&image.id);
                message.content.push_str(&format!(
                    "\n[Image #{}: {}; {} × {}; saved copy: {}]",
                    index + 1,
                    image.name,
                    image.width,
                    image.height,
                    path.display()
                ));
                message.images.push(crate::image::load_image(directory, image)?);
            }
            Ok(message)
        })
        .collect()
}

/// 按日志顺序归约出唯一的活动历史：摘要替换掉前缀，剪枝记录只借用替换后的正文。
/// 会进入可压缩历史的条目；文件指令由 Agent 直接加入请求。
pub(crate) fn is_context_entry(entry: &SessionEntry) -> bool {
    context_entry(entry).is_some()
}

/// 从后往前累加到保留预算，再在候选上界之内取最后一个工具对闭合的切点；
/// 预算为零时仍保留最后一个完整单元。
fn find_cut_point(entries: &[ContextPosition], session: &SessionData, keep_recent_tokens: u64) -> usize {
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        accumulated = accumulated.saturating_add(entries[index].token_estimate(session));
        if accumulated >= keep_recent_tokens {
            return last_balanced_cut(entries, session, index);
        }
    }
    0
}

/// 工具结果已经按调用顺序紧随 assistant；切点不能拆开这个工具单元。
/// 缺失结果由模型投影补齐，Skill 与触发它的用户输入也一同保留。
fn last_balanced_cut(entries: &[ContextPosition], session: &SessionData, upper_bound: usize) -> usize {
    let mut cut = 0usize;
    for (position, candidate) in entries.iter().take(upper_bound).enumerate() {
        let next_is_tool_result = entries.get(position + 1).is_some_and(|next| {
            matches!(
                next.entry(session),
                SessionEntry::Message {
                    message: AgentMessage::ToolResult { .. },
                    ..
                }
            )
        });
        if !next_is_tool_result
            && !matches!(
                candidate.entry(session),
                SessionEntry::Record {
                    record: LedgerRecord::SkillInstructions { .. },
                    ..
                }
            )
        {
            cut = position + 1;
        }
    }
    cut
}
