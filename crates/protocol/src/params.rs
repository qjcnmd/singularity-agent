//! 各 JSON-RPC method 的请求参数与响应结果类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::ClientInfo;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 初始化请求参数。
pub struct InitializeParams {
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 初始化响应及平台摘要。
pub struct InitializeResult {
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "platformFamily")]
    pub platform_family: String,
    #[serde(rename = "platformOs")]
    pub platform_os: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 创建 thread 的参数。
pub struct ThreadStartParams {
    pub model: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 更新一个 thread 的非敏感 provider/model/reasoning 选择。
pub struct ThreadSettingsParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_reasoning_patch"
    )]
    pub reasoning: Option<ReasoningPatch>,
}

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

fn deserialize_reasoning_patch<'de, D>(deserializer: D) -> Result<Option<ReasoningPatch>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ReasoningPatch::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// thread/settings 的脱敏结果；不包含 key、header 或其他认证材料。
///
/// `queued` 表示修改发生在活动轮期间：已接受但尚未持久化，
/// turn 到达可信终态后由 runtime 自动落盘并在下一 turn 生效。
pub struct ThreadSettingsResult {
    pub thread_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub updated: bool,
    #[serde(default)]
    pub queued: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 只包含 session id 的请求参数。
pub struct SessionIdParams {
    pub session_id: String,
}

fn default_session_turn_limit() -> u32 {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 查看会话历史：按 turn 为单位返回一页，默认最新 `limit` 轮；
/// 给 `beforeItem` 则返回该锚点 item 所属轮之前的 `limit` 轮（不含锚点轮），
/// 供"上滚加载更早"翻页。
pub struct ThreadReadParams {
    pub session_id: String,
    /// 每页最多返回的轮数（1..=200）。
    #[serde(default = "default_session_turn_limit")]
    pub limit: u32,
    /// 上一页最旧轮中的任意公开 item id；定位其所属轮并返回该轮之前的轮次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_item: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// `thread/read` 的公开历史 item；不暴露 SessionEntry 的 parent/tree、迁移或
/// provider-private replay 字段。
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
        usage: Value,
    },
    Compaction {
        id: String,
        summary: String,
    },
}

impl HistoryItem {
    /// 公开 history item 的稳定公开 id；`thread/read` 的 beforeItem 翻页锚点
    /// 取自上一页最旧轮内任意 item 的该 id。
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// thread/read 的响应：摘要 + 一页按 turn 组织的历史。
pub struct ThreadReadResult {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    /// 最近一次 turn 状态的投影，与 thread/list 的 `lastTurnStatus` 来自
    /// 同一投影：尚无 turn 为 None，运行中 active，
    /// 终态 completed/failed/interrupted。
    pub status: Option<ThreadStatus>,
    pub created_at: String,
    pub updated_at: String,
    pub token_usage: Value,
    /// 最近一次 compaction 摘要；无 compaction 时为 None。
    pub summary: Option<String>,
    /// 本页轮次，按会话顺序（旧→新）排列。
    pub turns: Vec<ThreadTurn>,
    /// 会话中真实 turn 的总数（不含无归属 turn 的前导组）。
    pub total_turns: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// session/delete 的响应。方法名与线格式保持既有语义；实际动作是归档
/// （rename 进 `archived/` 子目录），`deleted` 字段表示归档已发生。
pub struct SessionDeleteResult {
    pub session_id: String,
    pub deleted: bool,
}

/// 持久化 thread（session）的公开摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: String,
    /// 最近一次/当前一次 turn 的展示元数据，来自 JSONL 会话投影：
    /// 尚无 turn 时为 `None`（wire 上为 null），运行中为 active，终态为
    /// completed/failed/interrupted。`sg continue` 不受此字段限制。
    #[serde(rename = "lastTurnStatus")]
    pub last_turn_status: Option<ThreadStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// JSONL 会话状态的协议投影：最近一次/当前一次 turn 的状态。
pub enum ThreadStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// thread/start 的响应。
pub struct ThreadStartResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// thread/list 的响应。
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 启动 turn 的参数。
pub struct TurnStartParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub input: Vec<InputItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// 用户提交给 turn 的输入项。
pub enum InputItem {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 向仍在运行的 turn 注入输入；终态后的用户输入必须通过新的 turn/start 发送。
/// 未知 turn id 返回 not found；turn/steer 与 turn/followUp 共用此参数。
pub struct TurnInjectionParams {
    pub turn_id: String,
    pub input: Vec<InputItem>,
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
    /// 可选字段保持协议向后兼容：旧客户端读新响应时忽略未知字段，
    /// 新客户端读旧服务端时字段缺失回退为 None。终态 usage 同时写入
    /// JSONL metadata，app-server 重启后可从公开历史恢复。
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
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Thread 的 `lastTurnStatus` 投影：它承载的正是最近一次 turn 的终态，
/// 运行中的 turn 在 Thread 视角下记作 `active`。这张表是两枚举间唯一的
/// 派生关系，客户端投影不得另行手写映射。
impl From<TurnStatus> for ThreadStatus {
    fn from(status: TurnStatus) -> Self {
        match status {
            TurnStatus::Running => Self::Active,
            TurnStatus::Completed => Self::Completed,
            TurnStatus::Failed => Self::Failed,
            TurnStatus::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 只包含 turn id 的请求参数。
pub struct TurnIdParams {
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// turn/start 的响应。
pub struct TurnStartResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 当前 provider 配置的脱敏状态。
pub struct ProviderConfigurationStatus {
    pub source: Option<String>,
    pub snapshot_id: String,
    pub configured: bool,
    pub configuration_blocker: Option<String>,
    pub api_key_present: bool,
    pub base_url_present: bool,
    pub model_present: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// turn/steer 或 turn/followUp 的响应。
pub struct TurnInjectionResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// turn/interrupt 的响应。回执确认中断请求已受理并给出目标终态，不制造
/// 独立的中间请求状态。
pub struct TurnInterruptResult {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub status: TurnStatus,
}

/// server/shutdown 的类型化响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerShutdownResult {
    pub shutdown: bool,
}
