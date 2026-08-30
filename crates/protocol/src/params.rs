//! 公共协议对象：历史投影、会话摘要与 turn 合同。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `thread/settings` 中显式出现的 reasoning patch。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningPatch {
    /// 字符串：设置显式 reasoning effort。
    Set(String),
    /// `null`：清除显式值并恢复模型默认。
    Clear,
}

impl Serialize for ReasoningPatch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Set(value) => serializer.serialize_str(value),
            Self::Clear => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for ReasoningPatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)
            .map(|value| value.map_or(Self::Clear, Self::Set))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// 公开历史 item：只携带展示所需字段，不含 provider 私有重放材料。
pub enum HistoryItem {
    Message {
        id: String,
        role: String,
        text: String,
    },
    Thinking {
        id: String,
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        id: String,
        output: String,
        #[serde(rename = "isError")]
        is_error: bool,
    },
    Turn {
        id: String,
        status: TurnStatus,
    },
    Settings {
        id: String,
        provider: Option<String>,
        model: Option<String>,
        reasoning: Option<String>,
    },
    Usage {
        id: String,
        usage: TurnModelUsage,
    },
    Compaction {
        id: String,
        summary: String,
    },
}

impl HistoryItem {
    /// 公开 history item 的稳定公开 id；历史翻页锚点取自上一页最旧轮内
    /// 任意 item 的该 id。
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Thinking { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. }
            | Self::Turn { id, .. }
            | Self::Settings { id, .. }
            | Self::Usage { id, .. }
            | Self::Compaction { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 按 turn 组织的一轮公开历史。turn 边界由 JSONL 中的 turn 开始 metadata
/// 划定；首个开始标记之前落盘的前导条目（settings 等）没有归属 turn，
/// turnId/status 为 null。
pub struct ThreadTurn {
    pub turn_id: Option<String>,
    /// 该轮终态；仅有开始标记的未终止轮为 running（崩溃遗留会被整体状态
    /// 投影修正为 interrupted），前导组为 null。
    pub status: Option<TurnStatus>,
    /// 该轮公开条目，按会话顺序排列。
    pub items: Vec<HistoryItem>,
}

/// 持久化 thread（session）的公开摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: String,
    /// 最近一次/当前一次 turn 的展示元数据，来自 JSONL 会话投影：
    /// 尚无 turn 时为 `None`（wire 上为 null），运行中为 running，终态为
    /// completed/failed/interrupted。
    #[serde(rename = "lastTurnStatus")]
    pub last_turn_status: Option<TurnStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 持久化 turn 的公开摘要。
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    /// provider usage 投影（评估工具数据源）。
    ///
    /// provider 可能不报告 usage；缺失时本字段为 `None`，不把未知伪装成零。
    /// 终态 usage 同时写入 JSONL metadata，重启后可从公开历史恢复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnModelUsage>,
}

/// 模型 usage 的协议线格式（与 `singularity_model::ModelUsage` 同构，
/// 避免 protocol 依赖 model crate）。同时是 JSONL 会话 `turn_terminal`
/// 的 usage 存储形状：七个键全部必填、只认 camelCase，写出的形状与读入要求
/// 的形状完全相同。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 原始 usage 对象是否存在；为 false 时各计数保持 unknown 表示，不把缺失
    /// 伪装成零消费或其它可计算金额。
    pub usage_present: bool,
    /// 该聚合表示的每个 provider 请求是否都报告了精确 usage；未报告的末次
    /// 请求 usage 保持 partial 而非表示为 0。
    pub usage_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// turn 的生命周期状态：运行中（running）、已完成（completed）、已失败（failed）或已中断（interrupted）。
/// wire 词形由 serde snake_case 单源提供，不存在手写词表。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}
