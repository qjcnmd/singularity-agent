//! 前后端共用的历史、任务摘要、回合和用量类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 请求的用途：普通生成或压缩摘要；两者的用量都计入会话合计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Generation,
    Compaction,
}

/// 提供方为一次请求上报的 Token 数和缓存命中数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 提供方上报的总数；没有上报时取输入与输出之和。
    pub total_tokens: u64,
    /// None 为未上报；Some(0) 为明确上报零缓存用量。
    pub cached_input_tokens: Option<u64>,
}

/// 一次请求尝试的结果、耗时和用量；保存到日志后由后端统计。
/// 不含密钥或提供方私有数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestObservation {
    pub purpose: RequestPurpose,
    pub provider: String,
    pub model: String,
    pub status: crate::ProviderAttemptStatus,
    pub duration_ms: u64,
    /// 请求开始到首个生成增量的实测耗时；没有生成增量时保持未知。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub ttft_ms: Option<u64>,
    /// 首个生成增量到请求完成的耗时；没有生成增量时保持未知。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub decode_ms: Option<u64>,
    /// 未上报时为 None；有用量时输入、输出和总数始终齐全。
    pub usage: Option<ModelUsage>,
}

/// read 工具实际读到的行范围：起始行号和正文行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ReadSource {
    /// 第一行的行号；offset 省略或为 0 时是 1。
    pub start_line: u64,
    /// 正文行数，不含末尾的续读和超长行说明。
    pub line_count: u64,
}

/// 工具的结果，放在对应的工具条目里；工具还没结束时为空。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub output: String,
    /// bash 结果关联的后台命令，供界面跟随最新状态。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub background_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
    )]
    pub images: Vec<crate::ImageAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub read_source: Option<ReadSource>,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub duration_ms: Option<u64>,
}

/// 历史中可展示为消息的角色；工具调用单独显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum HistoryMessageRole {
    User,
    Assistant,
}

/// 工作台显示的一条历史：只含显示需要的字段，不含提供方私有的续接数据。
///
/// 回合的 ID 和状态保存在 ThreadTurn 上，条目里不重复。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryItem {
    Message {
        id: String,
        status: TurnStatus,
        role: HistoryMessageRole,
        text: String,
        /// 消息写入日志的时间；还在流式生成时为空。
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        at: Option<String>,
        /// 截至这条回复时整个会话的累计用量（含这条回复的请求），只出现在助手消息的最后一段正文上。
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        stats: Option<SessionModelUsage>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(
            feature = "typescript",
            ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
        )]
        images: Vec<crate::ImageAttachment>,
        /// 用户随消息附上的文件路径；模型按路径自己读取，文件本身不复制。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(feature = "typescript", ts(as = "Option<Vec<String>>", optional))]
        files: Vec<String>,
    },
    Thinking {
        id: String,
        status: TurnStatus,
        text: String,
    },
    Tool {
        id: String,
        status: ToolStatus,
        name: String,
        args: Value,
        result: Option<ToolResult>,
    },
    Compaction {
        id: String,
        summary: String,
    },
    /// 压缩进行中、失败、中断或无需压缩；成功的摘要用 Compaction 条目。
    CompactionResult {
        id: String,
        status: TurnStatus,
        message: Option<String>,
    },
}

impl HistoryItem {
    /// 返回条目的唯一标识。
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Thinking { id, .. }
            | Self::Tool { id, .. }
            | Self::Compaction { id, .. }
            | Self::CompactionResult { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 从回合开始记录起生成的历史；初始设置不构成回合。
pub struct ThreadTurn {
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub finished_at: Option<String>,
    /// 这一回合收到第一个生成增量的时间，由第一条有计时的助手消息的保存时间减去生成耗时得到。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub first_token_at: Option<String>,
    pub turn_id: String,
    pub status: TurnStatus,
    /// 中断是否由用户停止；没有结束记录的回合为 false。
    pub manually_stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<crate::TurnErrorDetail>,
    pub items: Vec<HistoryItem>,
}

/// 任务的 ID、工作目录、当前模型选择和创建时关闭的上下文模块。
#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    pub thread_id: String,
    pub model: crate::ModelSelection,
    pub cwd: String,
    pub disabled_modules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 回合开始和结束事件中携带的回合标识与状态。
pub struct Turn {
    pub turn_id: String,
    pub status: TurnStatus,
}

/// 整个会话的模型请求合计，供工作台显示成本和速度。
///
/// 每次请求尝试计一次，重试也计入，与日志中的请求记录一致。首 Token 耗时只统计
/// 实际测到的请求，Token 数只统计上报了用量的请求。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionModelUsage {
    /// 测到首 Token 耗时的请求的耗时合计和请求数，用于计算平均首 Token 耗时；与是否上报用量无关。
    pub ttft_ms: u64,
    pub ttft_requests: u64,
    /// 输入合计（包含命中缓存的那部分）。
    pub input_tokens: u64,
    /// 已知的缓存输入合计；仅 cache_usage_complete 为真时可计算命中率。
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// 同时有生成耗时和输出 Token 数的请求的合计，用于计算每秒输出 Token 数。
    pub decode_tokens: u64,
    pub decode_ms: u64,
    /// 计入合计的请求是否都明确报告了缓存输入用量。
    pub cache_usage_complete: bool,
    /// 计入统计的请求耗时合计（毫秒），含等待首个 token。
    pub generation_ms: u64,
    /// 是否有请求上报了用量；为 false 时 Token 数都是 0，不代表没有消耗。
    pub usage_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
/// 回合、消息或压缩条目的执行状态。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

/// 工具调用的状态；收到调用后先排队，实际开始执行时才进入 Running。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}
