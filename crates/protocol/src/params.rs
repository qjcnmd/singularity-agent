//! 公共协议对象：历史投影、会话摘要和 turn 合同。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 这次 provider 请求的发起原因；摘要请求和生成请求共用同一套计量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    #[default]
    Generation,
    Compaction,
}

/// 请求的检查信息：持久化会话、轨迹视图和实时的 provider/attempt 事件共用。
/// 请求本身与 provider 无关；鉴权信息和私有的重放数据都不在这里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestObservation {
    /// 查找请求详情并配对开始、结束观测的键；每次 provider attempt 生成一个，
    /// 与 assistant 或 compaction 的会话条目身份无关。
    pub request_id: String,
    /// 为显示做的小幅投影：只含 system/developer 消息、工具和偏好。
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
    /// 首个生成增量到请求完成的耗时；旧记录未采集时保持未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub decode_ms: Option<u64>,
    /// 供应商有效总量，缺失时由已知输入输出相加得到。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub error: Option<String>,
    /// 这次 attempt 的稳定诊断码：它和 `error` 类别一起构成可以持久回放的失败
    /// 事实，实时事件和历史读取都从这一份记录派生。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub diagnostic_code: Option<String>,
    /// 检查请求详情时失败；不影响 provider 的结果，也不影响会话能否恢复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub request_error: Option<Box<str>>,
}

/// read 工具真实读到的源文件范围：只有实际起始行和正文行数，不含正文本身。
/// 展示层直接用它，不再从本地化的说明文本或原始调用参数反推实际范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ReadSource {
    /// 实际读到的首个源文件行号；offset 省略或为 0 时规范化为 1。
    pub start_line: u64,
    /// 正文行数；分页续读和超长单行的说明都不计入。
    pub line_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
/// 对外公开的历史 item：只带展示需要的字段，不含 provider 的私有重放材料。
///
/// 一个 turn 的状态和身份归 ThreadTurn 所有，轮内的条目不重复承载同一份事实。
pub enum HistoryItem {
    /// 请求条目。身份只由 `observation.request_id` 承载，条目里不再单独保存
    /// 同一个值；`started_at` 是该请求开始观测时的记录时间，只有终态观测（旧
    /// 日志，或开始记录缺失）才为 None——不用结束时间去冒充开始时刻。
    Request {
        #[serde(rename = "startedAt", default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        started_at: Option<String>,
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
        /// read 工具真实读到的来源范围；其他工具和旧记录没有这个字段。
        #[serde(
            rename = "readSource",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        #[cfg_attr(feature = "typescript", ts(optional))]
        read_source: Option<ReadSource>,
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
    /// 公开 history item 的稳定公开 id。历史翻页的锚点不是 item id：分页按轮 cursor
    /// （`turn:{turnId}`，无归属的前导组是 `turn:leading`）定位，见 runtime 的分页实现。
    /// 请求条目的身份就是它所观测的 request id：生产者和消费者从同一处拿到同一个身份。
    pub fn id(&self) -> &str {
        match self {
            Self::Request { observation, .. } => &observation.request_id,
            Self::Message { id, .. }
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
/// 按 turn 组织的一轮公开历史。turn 的边界由 JSONL 里的 run operation_started 记录划定；第一个
/// 开始标记之前落盘的前导条目（settings 等）不属于任何 turn，turnId/status 为 null。
pub struct ThreadTurn {
    /// 本轮 run operation 的持久化边界时间；前导组或缺失边界为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub finished_at: Option<String>,
    pub turn_id: Option<String>,
    /// 该轮的终态；只有开始标记、还没结束的轮是 running（崩溃遗留的会被整体
    /// 状态投影修正为 interrupted），前导组是 null。
    pub status: Option<TurnStatus>,
    /// 该轮失败终态落盘下来的细节；成功、中断、前导组，以及没记录细节的旧日志都是 None。
    /// 它和实时的 `turn/error` 事件表达同一个概念，重读历史时不依赖 runtime 最近一次的错误文本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<crate::TurnErrorDetail>,
    /// 该轮的公开条目，按会话顺序排列。
    pub items: Vec<HistoryItem>,
}

/// 持久化 thread（即 session）的公开摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// provider usage 的投影（评估工具的数据来源）。provider 可能不报告 usage；缺失时本字段是
    /// None，不把未知伪装成零。请求观测写入 JSONL，重启后可聚合历史用量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub usage: Option<TurnModelUsage>,
}

/// 模型 usage 的协议线格式。为了不让 protocol 依赖 model crate，这里单独声明，但两者的语义并不
/// 相同：singularity_model::ModelUsage 是逐请求的观测（所以另有 cached_input_tokens_present 这类
/// 「有没有上报」的标志），本类型是一轮 turn 的累计结果，用 usage_complete 表达这次累计覆盖了
/// 哪些请求，两者不合并。七个键全部必填、只认 camelCase，写出的形状和读入要求一致。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 这次聚合里是否至少有一个请求完整上报了输入和输出计数（两项都齐全）。
    /// 为 false 时各个计数保持「未知」的含义，不把缺失伪装成零消费或可计算的金额。
    pub usage_present: bool,
    /// 这个聚合覆盖的每个 provider 请求是否都报告了精确 usage；没报告的请求
    /// 让结果保持「不完整」，而不是把它表示成 0。
    pub usage_complete: bool,
}

/// 会话累计的模型用量：整份账本里 provider 请求观测的合计，供工作台展示成本和速度。
///
/// 与 TurnModelUsage 的分工在范围与字段，不是两套重试口径：后者是一轮 turn 的计费累计
/// （`--json` 与评估的口径，含该轮内的全部重试），本类型跨轮次累计输入、输出和耗时。
/// requestId 标识一次具体的 provider 请求，每次 attempt 各自生成一个，所以按 requestId 归并
/// 折叠的是同一个请求的 started 与终态观测（取末次），重试和后续轮次全部计入；这个身份规则
/// 和工作台历史的请求投影一致，两处显示的数字吻合。没报告 usage 的请求会把计数降为下界，
/// 由 usage_complete 表达，不伪装成零消费。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionModelUsage {
    /// 输入合计（包含命中缓存的那部分）。
    pub input_tokens: u64,
    /// 已知的缓存输入合计；仅 cache_usage_complete 为真时可计算命中率。
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// 同时具有生成耗时和输出计数的样本，用于计算 TPS。
    pub decode_tokens: u64,
    pub decode_ms: u64,
    /// 所有请求都明确报告了缓存输入用量。
    pub cache_usage_complete: bool,
    /// 计入统计的请求耗时合计（毫秒），含等待首个 token。
    pub generation_ms: u64,
    /// 是否有请求报告了 usage；为 false 时上面的计数不含任何真实消费。
    pub usage_present: bool,
    /// 账本里每个请求都报告了 usage；为 false 时上面的计数只是下界，不是全量。
    pub usage_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
/// turn 的生命周期状态：运行中（running）、已完成（completed）、已失败（failed）
/// 或已中断（interrupted）。wire 词形由 serde 的 snake_case 单点提供，没有手写词表。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

/// --json 终态 summary 里的 thread 事实。thread 没能解析出来时，整个 summary 会
/// 省略这个对象，不写伪造的哨兵 id。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SummaryThread {
    pub thread_id: String,
}

/// --json 终态 summary 里的 turn 事实：状态、已知时的 threadId、观测到的 usage，以及只在截断
/// 终态出现的 truncated 标志。usage 为 None 时就以 null 出现，不把未知用量伪装成零。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SummaryTurn {
    pub status: TurnStatus,
    #[serde(rename = "threadId", skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub usage: Option<TurnModelUsage>,
    /// 只在截断终态出现；非截断终态会省略这个键（对老客户端是加法兼容）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// --json 唯一的终态 summary 对象：{"summary":{"thread":…,"turn":…}} 的内层形状。它是事件投影
/// 的输出契约，不取代 Session ledger 这个执行事实源；序列化统一由 Self::to_line 完成。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<SummaryThread>,
    pub turn: SummaryTurn,
}

impl TerminalSummary {
    /// 构造终态 summary：thread 已知时同时填 thread 和 turn.threadId；未知时两处
    /// 一起省略（同一个事实源，不会出现只填一处的形状）。
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

    /// summary 行唯一的 wire 投影：外层 {"summary": …} 这个键只在这里出现一次。
    pub fn to_line(&self) -> serde_json::Value {
        serde_json::json!({ "summary": self })
    }
}
