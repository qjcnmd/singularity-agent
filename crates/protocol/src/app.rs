//! 工作台状态、设置与事件推送的数据类型。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{SessionModelUsage, ThreadTurn, TurnEvent, TurnStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
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
    /// 最近一个回合是否由用户停止而中断。
    pub manually_stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadHistory {
    pub summary: ThreadSummary,
    pub turns: Vec<ThreadTurn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Running,
    Compacting,
    Stopping,
}

/// 给执行事件附加真实运行阶段，客户端不从事件类型推断阶段。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TurnEventEnvelope {
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub event: TurnEvent,
    pub phase: SessionPhase,
}

/// 未保存错误来自普通回合或独立压缩；压缩失败不改变任务的回合状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionOperation {
    Turn,
    Compaction,
}

/// 启动失败或未能写入日志的操作错误；已写入日志的回合失败直接从历史读取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionFailure {
    pub source: SessionOperation,
    pub error: crate::TurnErrorDetail,
}

/// 项目列表恢复任务状态所需的当前阶段和未保存错误。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub phase: SessionPhase,
    pub failure: Option<SessionFailure>,
}

/// 已接受、还没保存的用户消息；保存后条目 ID 不变。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingUserMessage {
    pub item_id: String,
    pub text: String,
    pub images: Vec<crate::ImageAttachment>,
    pub files: Vec<String>,
}

/// 提供方、模型和可选推理档位；配置、会话和桌面请求共用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
}

/// 会话当前运行阶段、待处理输入与用量。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntime {
    pub phase: SessionPhase,
    pub selection: ModelSelection,
    pub context_usage: Option<ContextUsage>,
    pub pending_question: Option<crate::PendingQuestion>,
    pub pending_messages: Vec<PendingUserMessage>,
    pub failure: Option<SessionFailure>,
    /// 整份会话的累计模型用量。
    pub usage: SessionModelUsage,
}

/// 最近一次生成请求的实际输入用量，以及执行时采用的窗口大小。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used: u64,
    pub capacity: u64,
}

/// 模型设置表单读写的一项模型配置；保存时由 `singularity_model` 检查取值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigurationInput {
    pub model_id: String,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Option<Vec<String>>,
    pub thinking_wire_format: Option<String>,
    /// Chat 请求里表示最大输出 Token 的字段名；`None` 表示发送 `max_tokens`。
    pub chat_output_tokens_field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RedactedProvider {
    pub api_protocol: ProviderApiProtocol,
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
    /// 配置读取或默认模型选择校验失败；尚未配置提供方或模型时为空。
    pub error: Option<String>,
    pub default_selection: Option<ModelSelection>,
    pub providers: Vec<RedactedProvider>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    Chat,
    Responses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfigurationInput {
    pub api_protocol: ProviderApiProtocol,
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ModelConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AppBootstrap {
    /// 系统用户主目录，用于界面缩短路径；与应用数据目录无关。
    pub user_home: Option<String>,
    pub session_states: BTreeMap<String, SessionStatus>,
    pub revision: u64,
    pub workspaces: Vec<Workspace>,
    pub sessions_by_workspace: BTreeMap<String, Vec<ThreadSummary>>,
    pub model_catalog: RedactedModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionReadResult {
    /// 该任务最近一次发出事件的序号；前端用来忽略更早的旧事件。
    pub revision: u64,
    pub history: ThreadHistory,
    pub runtime: SessionRuntime,
    /// 首次读取后台命令，后续变化由独立事件更新。
    pub background_tasks: Vec<crate::BackgroundTask>,
    pub context: Option<crate::InjectedContext>,
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
    ConfigurationInvalid,
    ProviderUnavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
}

impl RpcError {
    /// 创建带错误码和实际原因的 RPC 错误。
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcResponse {
    Success { result: Value },
    Error { error: RpcError },
}

/// 推送给工作台的事件；序列化时与外层的事件序号合并成一个对象。
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
        summary: ThreadSummary,
    },
    SessionSummaryChanged {
        summary: ThreadSummary,
    },
    BackgroundTaskUpdated {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: Box<crate::BackgroundTaskUpdate>,
    },
    AppReadFailed {
        error: RpcError,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct StreamEnvelope {
    pub revision: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

/// 首次发送的结果：输入已接受，工作台按返回的任务 ID 打开任务。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStartResult {
    pub summary: ThreadSummary,
    pub status: SessionStatus,
    pub revision: u64,
}

/// 保存或删除提供方后的模型目录及最新事件序号，前端据此忽略更早的旧列表。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogResult {
    pub catalog: crate::RedactedModelCatalog,
    pub revision: u64,
}
