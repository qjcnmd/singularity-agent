//! 公共协议对象：历史投影、会话摘要与 turn 合同。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 该 provider 请求的发起原因；摘要与生成共用同一套计量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    #[default]
    Generation,
    Compaction,
}

/// 请求检查信息：持久化会话、轨迹视图与实时 provider/attempt 事件共用。
/// 请求是 provider 无关的输入；鉴权信息与私有重放数据不在此列。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestObservation {
    /// 不可变请求详情的查找键；每次 provider attempt 一个。
    pub request_id: String,
    /// 小幅显示投影：只含 system/developer 消息、工具与偏好。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_head: Option<Box<crate::ModelRequestSnapshot>>,
    #[serde(default)]
    pub purpose: RequestPurpose,
    pub ordinal: u32,
    pub attempt: u32,
    pub provider: String,
    pub model: String,
    pub status: crate::ProviderAttemptStatus,
    pub duration_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub error: Option<String>,
    /// 检查失败；不改变 provider 结果与会话可恢复性。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_error: Option<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
/// 公开历史 item：只携带展示所需字段，不含 provider 私有重放材料。
///
/// 一个 turn 的状态与身份归属 ThreadTurn，轮内条目不重复承载同一事实。
pub enum HistoryItem {
    Request {
        id: String,
        timestamp: String,
        observation: RequestObservation,
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        diff: Option<String>,
        #[serde(rename = "isError")]
        is_error: bool,
        #[serde(
            rename = "durationMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "typescript", ts(optional))]
        duration_ms: Option<u64>,
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
}

impl HistoryItem {
    /// 公开 history item 的稳定公开 id；历史翻页锚点取自上一页最旧轮内
    /// 任意 item 的该 id。
    pub fn id(&self) -> &str {
        match self {
            Self::Request { id, .. }
            | Self::Message { id, .. }
            | Self::Thinking { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ToolResult { id, .. }
            | Self::Settings { id, .. }
            | Self::Compaction { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 按 turn 组织的一轮公开历史。turn 边界由 JSONL 中的 run operation_started
/// 记录划定；首个开始标记之前落盘的前导条目（settings 等）没有归属 turn，
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
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 持久化 turn 的公开摘要。
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    /// provider usage 投影（评估工具数据源）。
    ///
    /// provider 可能不报告 usage；缺失时本字段为 None，不把未知伪装成零。
    /// 终态 usage 同时写入 JSONL metadata，重启后可从公开历史恢复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub usage: Option<TurnModelUsage>,
}

/// 模型 usage 的协议线格式；为免 protocol 依赖 model crate 而独立声明，但两者
/// 语义不同：singularity_model::ModelUsage 是逐请求的观测（因此另有
/// cached_input_tokens_present 这类“是否上报”标志），本类型是一轮 turn 的累计
/// 结果，用 usage_complete 表达累计覆盖了哪些请求，两者不合并。
/// 同时是 JSONL 会话 operation_finished 的 usage 存储形状：七个键全部必填、
/// 只认 camelCase，写出的形状与读入要求的形状完全相同。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
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

/// 会话累计模型用量：整份账本的 provider 请求观测合计，供工作台展示成本与速度。
///
/// 与 TurnModelUsage 的分工在范围与字段，不是两套重试口径：后者是一轮 turn 的
/// 计费累计（`--json` 与评估口径，含该轮内的全部重试），本类型跨轮次累计输入、
/// 输出与耗时。requestId 标识一次具体 provider 请求，每次 attempt 各自生成一个，
/// 因此按 requestId 归并折叠的是同一请求的 started 与终态观测（取末次），重试与
/// 后续轮次全部计入；该身份规则与工作台历史的请求投影一致，两处显示互相吻合。
/// 未报告 usage 的请求把计数降为下界，由 usage_complete 表达，不伪装成零消费。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionModelUsage {
    /// 输入合计（含缓存命中部分）。
    pub input_tokens: u64,
    /// input_tokens 中命中缓存的部分；请求未报告缓存时按 0 计入本项。
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// 计入请求的耗时合计（毫秒），含等待首 token；平均速度的分母。
    pub generation_ms: u64,
    /// 是否有请求报告了 usage；为 false 时以上计数不含任何真实消费。
    pub usage_present: bool,
    /// 账本中的每个请求都报告了 usage；为 false 时以上计数是下界而非全量。
    pub usage_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
/// turn 的生命周期状态：运行中（running）、已完成（completed）、已失败（failed）或已中断（interrupted）。
/// wire 词形由 serde snake_case 单源提供，不存在手写词表。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

/// --json 终态 summary 的 thread 事实。thread 未解析时整个 summary 省略
/// 本对象，不写入伪造的哨兵 id。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SummaryThread {
    pub thread_id: String,
}

/// --json 终态 summary 的 turn 事实：状态、已知时的 threadId、观测 usage
/// 与仅在截断终态出现的 truncated 标志。usage 为 None 时以 null 出现，
/// 不把未知用量伪装成零。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SummaryTurn {
    pub status: TurnStatus,
    #[serde(rename = "threadId", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub thread_id: Option<String>,
    pub usage: Option<TurnModelUsage>,
    /// 仅截断终态出现；非截断终态省略本键（加法兼容）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// --json 唯一终态 summary 对象：{"summary":{"thread":…,"turn":…}} 的
/// 内层形状。它是事件投影的输出契约，不取代 Session ledger 的执行事实源。
/// 序列化经 Self::to_line 单点完成，客户端不再各自手搭 wire 形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub thread: Option<SummaryThread>,
    pub turn: SummaryTurn,
}

impl TerminalSummary {
    /// 构造终态 summary：thread 已知时同时填充 thread 与 turn.threadId，
    /// 未知时两处一并省略（同一事实源，不存在只填其一的形状）。
    pub fn new(
        thread_id: Option<&str>,
        status: TurnStatus,
        usage: Option<TurnModelUsage>,
        truncated: bool,
    ) -> Self {
        Self {
            thread: thread_id.map(|id| SummaryThread {
                thread_id: id.to_string(),
            }),
            turn: SummaryTurn {
                status,
                thread_id: thread_id.map(str::to_string),
                usage,
                truncated,
            },
        }
    }

    /// summary 行的唯一 wire 投影：外层 {"summary": …} 键只在此出现一次。
    pub fn to_line(&self) -> serde_json::Value {
        serde_json::json!({ "summary": self })
    }
}
