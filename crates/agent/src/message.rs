//! Pi 式 Agent 消息类型（语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的
//! `dist/core/messages.js` 与 `dist/core/session-manager.js`）。
//!
//! 这是新 headless core 的第一步，与旧 `AgentLoop`/`context.rs` 并存，不切换。

/// 可直接交给模型提供方的消息形态，复用 `singularity_model::ModelMessage`，
/// 避免与旧 AgentLoop 形成第二套 LLM 消息表示。
pub type LlmMessage = singularity_model::ModelMessage;

/// Pi AgentMessage 的 role 枚举。
///
/// 序列化为 Pi JSON 使用的 camelCase 小写 role（`user`/`assistant`/`toolResult`/
/// `bashExecution`/`custom`/`branchSummary`/`compactionSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMessageRole {
    User,
    Assistant,
    ToolResult,
    BashExecution,
    Custom,
    BranchSummary,
    CompactionSummary,
}

/// Pi 式会话消息 payload。
///
/// JSON 字段名对齐 Pi：`role`/`content`/`toolCallId`/`toolName`/`args`/`timestamp`。
/// 与 Pi 的差异（Phase 2a 简化）：
/// - `content` 为普通字符串；Pi 的 assistant/toolResult 消息 content 是 content block
///   数组，读取真实 Pi 文件中此类消息时会原样保留但不进入 LLM 上下文。
/// - `timestamp` 用 u64 毫秒时间戳（整数）。Pi 的消息 timestamp 本就是 unix ms
///   数字（`docs/session-format.md`），ISO8601 是 Pi *条目*（entry）级时间戳，见
///   `session.rs` 的 `SessionEntry::timestamp`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub role: AgentMessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// unix 毫秒时间戳，对齐 Pi 消息 timestamp。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// Pi `COMPACTION_SUMMARY_PREFIX`（messages.js）：compaction 摘要进入 LLM 上下文时的固定前缀。
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Pi `COMPACTION_SUMMARY_SUFFIX`（messages.js）。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Pi `BRANCH_SUMMARY_PREFIX`（messages.js）。
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// Pi `BRANCH_SUMMARY_SUFFIX`（messages.js）。
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";
