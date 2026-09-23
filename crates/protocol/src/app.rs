//! 本地 Web 工作台的版本 8 合同。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{RpcMethod, SessionModelUsage, ThreadTurn, TurnEvent, TurnStatus};

/// 版本 2 起，tool/execution/update 与 tool/execution/end 不再重复携带工具名称
/// 和参数，结果字段直接表达输出、失败与文件变更；版本 3 起，所有事件与请求检查
/// 载荷的 item 身份统一为 `item: {itemId}`，检查载荷字段也改用 camelCase；
/// 版本 4 起，provider/attempt 不再在外层重复携带 diagnosticCode，这个事实只由
/// observation.diagnosticCode 承载；版本 5 把应用级 RPC 与事件命名为
/// app.bootstrap 和 app_changed；版本 6 把 session_settled 的载荷直接设为
/// SessionRuntime，并移除 DiscoveredModel 的 thinking_wire_format 字段；版本 7 移除
/// HTTP RPC 请求和响应中重复的请求 ID，并公开请求定义的账本 ID；版本 8
/// 将 session.create 收敛为仅接收 workspaceId，模型选择由后续设置操作完成。
/// 工作台前端与二进制同版本分发，所以按同一个版本整体切换，不保留双版本 adapter。
pub const PROTOCOL_VERSION: u16 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<TurnStatus>,
    /// 最近一次被中断的 run，在账本里有明确的用户取消记录。
    pub manually_stopped: bool,
    pub turn_count: usize,
    /// 整份账本的累计模型用量。它只在生成快照时更新，运行中回合的增量由调用方
    /// 从活动事件里另算（读盘的冻结窗口保证两者不重叠），所以这里不是实时值。
    pub usage: SessionModelUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    pub turns: Vec<ThreadTurn>,
    pub next_cursor: Option<String>,
}

/// 输入是从哪个入口被接受的：`Steer` 注入正在执行的 turn；`FollowUp` 和
/// `Submit` 都等自己那一轮开始执行（前者是运行中的追加输入，后者是普通提交）。
/// channel 只记录来源，不表示这条输入现在是否还在等待处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ControlChannel {
    Steer,
    FollowUp,
    Submit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ControlDisposition {
    Pending,
    Injected,
    StartedAsNewTurn,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlSnapshot {
    pub control_id: String,
    /// 这条输入绑定到的 turn：注入活动 turn 的 steer 在接受时就已经有；等待自己
    /// 那一轮的排队输入在执行开始前是 None（这时还没有可以关联的 turn）。
    pub turn_id: Option<String>,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: String,
    pub disposition: ControlDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Reserved,
    Running,
    Compacting,
    Stopping,
}

/// 给原始执行事件附加工作台的数据版本号；开始时间由执行事件自己携带。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TurnEventEnvelope {
    #[serde(flatten)]
    pub event: TurnEvent,
    pub session_revision: u64,
}

/// 普通会话变更里携带的精简活动 turn 身份；事件本身只走增量通道或完整恢复快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveTurnRuntimeSnapshot {
    pub turn_id: String,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveCompactionSnapshot {
    pub started_at: String,
}

/// 产生了终态反馈的操作：普通回合，或一次独立的压缩。界面按来源决定反馈放在
/// 哪里——回合终态描述任务本身，压缩终态只描述那次压缩，不改变任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionTerminalSource {
    Turn,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTerminalSnapshot {
    pub source: SessionTerminalSource,
    pub status: TurnStatus,
    pub message: Option<String>,
}

/// 普通 `session_changed` / `session_settled` 使用的精简运行态载荷：带着活动 turn/compaction 的
/// 身份、终态、队列和冻结窗口，但不携带活动事件；完整事件只在 `session.read` 的恢复快照里传输。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRuntime {
    pub session_revision: u64,
    pub phase: SessionPhase,
    pub selector: Option<String>,
    pub model_context_window: Option<u64>,
    pub pending_controls: Vec<ControlSnapshot>,
    pub active_turn: Option<ActiveTurnRuntimeSnapshot>,
    pub active_compaction: Option<ActiveCompactionSnapshot>,
    pub terminal: Option<SessionTerminalSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ModelConfigurationStatus {
    Ready,
    Missing,
    Invalid,
}

/// 可编辑的模型取值；取值是否合法由 runtime 的配置解析负责校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfigurationInput {
    pub model_id: String,
    pub display_name: Option<String>,
    pub api_protocol: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_variants: Vec<ReasoningVariant>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
    /// Chat 输出上限使用的 wire 字段名；`None` 表示发送 `max_tokens`。表单里没有
    /// 这个开关的控件，但保存往返时会原样保留已有取值。
    #[serde(default)]
    pub chat_output_tokens_field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedProvider {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub credential_configured: bool,
    pub models: Vec<ModelConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedModelCatalog {
    pub configuration: ModelConfigurationStatus,
    pub message: Option<String>,
    pub default_selector: Option<String>,
    pub providers: Vec<RedactedProvider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    Chat,
    Responses,
}

impl ProviderApiProtocol {
    /// 请求观测里保留完整的协议名称；配置里用的是枚举的短 serde 词形。
    pub fn observation_name(self) -> &'static str {
        match self {
            Self::Chat => "open_ai_chat_completions",
            Self::Responses => "open_ai_responses",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningVariant {
    pub id: String,
    pub wire_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfigurationInput {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ModelConfigurationInput>,
}

/// provider 宣告可用的模型，供编辑草稿显式采用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoveredModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Vec<ReasoningVariant>,
    pub default_variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppBootstrap {
    /// 系统用户主目录，用于界面缩短路径；与应用数据目录无关。
    pub user_home: Option<String>,
    pub session_phases: std::collections::BTreeMap<String, SessionPhase>,
    pub generation: String,
    pub revision: u64,
    pub workspaces: Vec<Workspace>,
    pub sessions_by_workspace: BTreeMap<String, Vec<ThreadSummary>>,
    pub model_catalog: RedactedModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReadResult {
    pub history: ThreadReadPage,
    pub runtime: SessionRuntime,
    pub active_events: Vec<TurnEventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcRequest {
    #[serde(deserialize_with = "deserialize_protocol_version")]
    pub version: u16,
    pub method: RpcMethod,
    pub params: Value,
}

fn deserialize_protocol_version<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u16::deserialize(deserializer)?;
    if version == PROTOCOL_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(format!(
            "unsupported protocol version {version}"
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    InvalidRequest,
    WorkspaceNotFound,
    WorkspaceBusy,
    SessionNotFound,
    SessionBusy,
    ControlNotFound,
    ConfigurationInvalid,
    ConfigurationPartiallySaved,
    ProviderUnavailable,
    Conflict,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub recovery: String,
}

impl RpcError {
    /// 创建带恢复建议的工作台错误。未提交的草稿由浏览器自己保存，
    /// 错误载荷里不再回传用户输入。
    pub fn new(
        code: RpcErrorCode,
        message: impl Into<String>,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcResponse {
    pub version: u16,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<RpcError>,
}

/// 带类型的载荷；外层信封会把该枚举展平成既有的 wire 形状。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Ready {
        payload: crate::EmptyParams,
    },
    AppChanged {
        payload: AppBootstrap,
    },
    SessionChanged {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: SessionRuntime,
    },
    TurnEvent {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: TurnEventEnvelope,
    },
    SessionSettled {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: SessionRuntime,
    },
    ResyncRequired {
        payload: crate::EmptyParams,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct StreamEnvelope {
    pub version: u16,
    pub generation: String,
    pub revision: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}
