//! 公共协议对象：历史投影、会话摘要和 turn 合同。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 这次 provider 请求的发起原因；摘要请求和生成请求共用同一套计量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Generation,
    Compaction,
}

/// 提供方单次请求上报的真实 Token 数与缓存计数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 供应商上报总量，缺失时由协议解析器将输入输出相加得到。
    pub total_tokens: u64,
    /// None 为未上报；Some(0) 为明确上报零缓存用量。
    pub cached_input_tokens: Option<u64>,
}

/// 请求用量与结果：持久化会话和实时 provider/attempt 事件共用。
/// 请求本身与 provider 无关；鉴权信息和私有的重放数据都不在这里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestObservation {
    /// 查找请求详情并配对开始、结束观测的键；每次 provider attempt 生成一个，
    /// 与 assistant 或 compaction 的会话条目身份无关。
    pub request_id: String,
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

/// read 工具真实读到的源文件范围：只有实际起始行和正文行数，不含正文本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ReadSource {
    /// 实际读到的首个源文件行号；offset 省略或为 0 时规范化为 1。
    pub start_line: u64,
    /// 正文行数；分页续读和超长单行的说明都不计入。
    pub line_count: u64,
}

/// 工具已经保存的结果；历史条目把它与调用放在一起，未完成时为空。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub output: String,
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

/// 历史中可展示为消息的角色；工具调用和请求各有独立条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum HistoryMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
/// 对外公开的历史 item：只带展示需要的字段，不含 provider 的私有重放材料。
///
/// 一个 turn 的状态和身份归 ThreadTurn 所有，轮内的条目不重复承载同一份事实。
pub enum HistoryItem {
    /// 请求状态与计量，身份来自 request_id。
    Request {
        observation: RequestObservation,
    },
    /// 本轮实际注入的内容。
    Context {
        id: String,
        name: String,
        content: String,
    },
    Message {
        id: String,
        running: bool,
        role: HistoryMessageRole,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(
            feature = "typescript",
            ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
        )]
        images: Vec<crate::ImageAttachment>,
    },
    Thinking {
        id: String,
        running: bool,
        text: String,
    },
    Tool {
        id: String,
        running: bool,
        name: String,
        args: Value,
        result: Option<ToolResult>,
    },
    Settings {
        id: String,
        provider: String,
        model: String,
        reasoning: Option<String>,
    },
    Compaction {
        id: String,
        summary: String,
    },
    /// 压缩的开始、失败、中断或无需压缩结果；成功摘要使用 Compaction 条目。
    CompactionResult {
        id: String,
        status: TurnStatus,
        message: Option<String>,
    },
}

impl HistoryItem {
    /// 公开 history item 的稳定 id；请求条目的身份是它所观测的 request id。
    pub fn id(&self) -> &str {
        match self {
            Self::Request { observation, .. } => &observation.request_id,
            Self::Context { id, .. }
            | Self::Message { id, .. }
            | Self::Thinking { id, .. }
            | Self::Tool { id, .. }
            | Self::Settings { id, .. }
            | Self::Compaction { id, .. }
            | Self::CompactionResult { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 公开历史中的单回合；`turn_id` 为 None 表示首个回合之前的前导条目。
pub struct ThreadTurn {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub finished_at: Option<String>,
    pub turn_id: Option<String>,
    pub status: Option<TurnStatus>,
    /// 中断是否来自用户主动停止；未完成的历史操作为 false。
    pub manually_stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<crate::TurnErrorDetail>,
    pub items: Vec<HistoryItem>,
}

/// 持久化 thread（即 session）的公开摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 持久化 turn 的公开摘要。
pub struct Turn {
    pub turn_id: String,
    pub status: TurnStatus,
}

/// 整份账本里 provider 请求观测的合计，供工作台展示成本和速度。
///
/// requestId 每次 attempt 各生成一个，按它归并同一请求的 started 与终态观测（取末次），
/// 重试计入合计，该规则与工作台历史的请求投影一致。TTFT 只统计有实测计时的请求，token
/// 与消费只统计上报了 usage 的请求，跨轮累计输入、输出和耗时。
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionModelUsage {
    /// 有首 token 计时的请求：等待耗时合计与样本数，用于计算 TTFT；不依赖 usage 上报。
    pub ttft_ms: u64,
    pub ttft_requests: u64,
    /// 输入合计（包含命中缓存的那部分）。
    pub input_tokens: u64,
    /// 已知的缓存输入合计；仅 cache_usage_complete 为真时可计算命中率。
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// 同时具有生成耗时和输出计数的样本，用于计算 TPS。
    pub decode_tokens: u64,
    pub decode_ms: u64,
    /// 计入合计的请求是否都明确报告了缓存输入用量。
    pub cache_usage_complete: bool,
    /// 计入统计的请求耗时合计（毫秒），含等待首个 token。
    pub generation_ms: u64,
    /// 是否有请求报告了 usage；为 false 时 token 计数不含任何真实消费。
    pub usage_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
/// 回合生命周期状态。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}
