//! 桌面工作台 RPC 与事件合同。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{SessionModelUsage, ThreadTurn, TurnEvent, TurnStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub status: Option<TurnStatus>,
    /// 最近一次被中断的 run，在账本里有明确的用户取消记录。
    pub manually_stopped: bool,
    pub turn_count: usize,
    /// 整份账本的累计模型用量。它只在生成快照时更新，运行中回合的增量由调用方
    /// 从活动事件里另算（读盘的冻结窗口保证两者不重叠），所以这里不是实时值。
    pub usage: SessionModelUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    pub turns: Vec<ThreadTurn>,
    pub next_cursor: Option<String>,
}

/// 待处理输入按数组顺序展示；身份用于编辑、撤回和立即发送。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingInput {
    pub control_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
    )]
    pub images: Vec<crate::ImageAttachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[serde(rename_all = "camelCase")]
pub struct ActiveTurnRuntimeSnapshot {
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ActiveCompactionSnapshot {
    pub started_at: String,
}

/// 产生了终态反馈的操作：普通回合，或一次独立的压缩。界面按来源决定反馈放在
/// 哪里——回合终态描述任务本身，压缩终态只描述那次压缩，不改变任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionTerminalSource {
    Turn,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionTerminalSnapshot {
    pub source: SessionTerminalSource,
    pub status: TurnStatus,
    /// 该回合的中断来自已接受的用户停止，与持久化摘要使用同一事实。
    pub manually_stopped: bool,
    pub message: Option<String>,
}

/// 普通 `session_changed` / `session_settled` 使用的精简运行态载荷：带着活动 turn/compaction 的
/// 身份、终态、队列和冻结窗口，但不携带活动事件；完整事件只在 `session.read` 的恢复快照里传输。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntime {
    pub session_revision: u64,
    pub phase: SessionPhase,
    pub selector: Option<String>,
    pub model_context_window: Option<u64>,
    pub pending_controls: Vec<PendingInput>,
    pub active_turn: Option<ActiveTurnRuntimeSnapshot>,
    pub active_compaction: Option<ActiveCompactionSnapshot>,
    pub terminal: Option<SessionTerminalSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[serde(rename_all = "camelCase")]
pub struct ModelConfigurationInput {
    pub model_id: String,
    /// 智能配置管理的字段；None 表示未记录归属，编辑器仅补齐空字段。空列表为全手动。
    pub automatic_fields: Option<Vec<ModelConfigurationField>>,
    pub display_name: Option<String>,
    pub api_protocol: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Option<Vec<ReasoningVariant>>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
    /// Chat 输出上限使用的 wire 字段名；`None` 表示发送 `max_tokens`。
    pub chat_output_tokens_field: Option<String>,
    pub requires_reasoning_content_for_tool_calls: Option<bool>,
}

/// 思考选项与默认选项共同作为一个覆盖单元。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ModelConfigurationField {
    DisplayName,
    MaxContextTokens,
    MaxOutputTokens,
    ReasoningVariants,
    ThinkingWireFormat,
    ChatOutputTokensField,
    RequiresReasoningContentForToolCalls,
}

impl ModelConfigurationField {
    pub const ALL: [Self; 7] = [
        Self::DisplayName,
        Self::MaxContextTokens,
        Self::MaxOutputTokens,
        Self::ReasoningVariants,
        Self::ThinkingWireFormat,
        Self::ChatOutputTokensField,
        Self::RequiresReasoningContentForToolCalls,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RedactedProvider {
    pub api_protocol: Option<String>,
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub credential_configured: bool,
    pub models: Vec<ModelConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ReasoningVariant {
    pub id: String,
    pub wire_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfigurationInput {
    pub api_protocol: Option<String>,
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ModelConfigurationInput>,
}

/// provider 宣告可用的模型，供编辑草稿显式采用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Vec<ReasoningVariant>,
    pub default_variant: Option<String>,
    pub requires_reasoning_content_for_tool_calls: Option<bool>,
    /// 元数据的补充来源；缺省表示仅使用提供方模型目录。
    pub metadata_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AppBootstrap {
    /// 系统用户主目录，用于界面缩短路径；与应用数据目录无关。
    pub user_home: Option<String>,
    pub session_phases: std::collections::BTreeMap<String, SessionPhase>,
    pub revision: u64,
    pub workspaces: Vec<Workspace>,
    pub sessions_by_workspace: BTreeMap<String, Vec<ThreadSummary>>,
    pub model_catalog: RedactedModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionReadResult {
    pub history: ThreadReadPage,
    pub runtime: SessionRuntime,
    pub active_events: Vec<TurnEventEnvelope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub recovery: String,
}

impl RpcError {
    /// 创建带恢复建议的工作台错误。未提交的草稿由前端自己保存，
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

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcResponse {
    Success { result: Value },
    Error { error: RpcError },
}

/// 带类型的载荷；外层信封会把该枚举展平成既有的 wire 形状。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Ready,
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
        payload: Box<TurnEventEnvelope>,
    },
    SessionSettled {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: SessionRuntime,
    },
    ResyncRequired,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct StreamEnvelope {
    pub revision: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}
