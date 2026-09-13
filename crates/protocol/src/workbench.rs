//! 本地 Web 工作台版本 1 合同。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{RpcMethod, ThreadTurn, TurnEvent, TurnStatus};

pub const WORKBENCH_PROTOCOL_VERSION: u16 = 1;

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
    /// The latest interrupted run has an explicit user cancellation in the ledger.
    pub manually_stopped: bool,
    pub turn_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    pub compaction_summary: Option<String>,
    pub turns: Vec<ThreadTurn>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ControlChannel {
    Steer,
    FollowUp,
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
    pub turn_id: String,
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

/// 活动事件保留原始协议类型，仅在发送时附加工作台水位与时间。
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(type = "StampedTurnEvent<TurnEvent>"))]
pub struct WorkbenchTurnEvent {
    pub event: TurnEvent,
    pub session_revision: u64,
    pub started_at: String,
}

impl Serialize for WorkbenchTurnEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut payload = crate::turn_event_envelope(&self.event);
        payload["sessionRevision"] = self.session_revision.into();
        if matches!(
            self.event,
            TurnEvent::TurnStarted { .. } | TurnEvent::ToolExecutionStart { .. }
        ) {
            payload["params"]["startedAt"] = self.started_at.clone().into();
        }
        payload.serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveTurnSnapshot {
    pub turn_id: String,
    pub events: Vec<WorkbenchTurnEvent>,
    pub started_at: String,
}

/// 普通会话变更携带的轻量活动 turn 身份；事件只经增量通道或完整恢复快照传递。
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTerminalSnapshot {
    pub status: TurnStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSnapshot {
    pub session_revision: u64,
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// 当前执行（或最近一次执行）冻结的有效上下文窗口（token）。该事实
    /// 由 turn 开始时的模型配置解析得出，不随后续配置编辑改变；进程内
    /// 尚无执行或进程重启后不可知，客户端保留未知而不是回退猜测。
    pub model_context_window: Option<u64>,
    /// 当前进程中尚未交给 Agent 的排队输入。
    pub pending_controls: Vec<ControlSnapshot>,
    pub active_turn: Option<ActiveTurnSnapshot>,
    pub active_compaction: Option<ActiveCompactionSnapshot>,
    pub terminal: Option<SessionTerminalSnapshot>,
}

/// 普通 `session_changed` / `session_settled` 的轻量运行态载荷。
///
/// 它保留活动 turn/compaction 身份、终态、队列与冻结窗口，不携带活动事件；
/// 完整事件只随 `session.read` 恢复快照传输。
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub api_protocol: String,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Vec<ReasoningVariant>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedProvider {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub credential_configured: bool,
    pub models: Vec<RedactedModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedModelCatalog {
    pub configuration: ModelConfigurationStatus,
    pub message: Option<String>,
    pub default_selector: Option<String>,
    pub providers: Vec<RedactedProvider>,
    pub presets: Vec<ProviderConfigurationInput>,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningVariant {
    pub id: String,
    pub enabled: bool,
    pub wire_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelInput {
    pub model_id: String,
    pub display_name: Option<String>,
    pub api_protocol: ProviderApiProtocol,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_variants: Vec<ReasoningVariant>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfigurationInput {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ProviderModelInput>,
}

/// A provider's advertised model, offered for explicit adoption into an editor draft.
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
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialConfigured {
    pub provider_id: String,
    pub credential_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchBootstrap {
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
    pub runtime: SessionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcRequest {
    #[serde(deserialize_with = "deserialize_protocol_version")]
    pub version: u16,
    pub request_id: String,
    pub method: RpcMethod,
    pub params: Value,
}

fn deserialize_protocol_version<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u16::deserialize(deserializer)?;
    if version == WORKBENCH_PROTOCOL_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(format!(
            "unsupported workbench protocol version {version}"
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub preserved_input: Option<String>,
}

impl RpcError {
    /// 创建包含恢复建议的工作台错误。
    pub fn new(
        code: RpcErrorCode,
        message: impl Into<String>,
        recovery: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
            preserved_input: None,
        }
    }

    /// 保留未被接受的输入，供客户端恢复草稿。
    pub fn preserve(mut self, input: impl Into<String>) -> Self {
        self.preserved_input = Some(input.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcResponse {
    pub version: u16,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<RpcError>,
}

/// Typed payloads; the outer envelope flattens this enum into the existing wire shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Ready {
        payload: crate::EmptyParams,
    },
    WorkbenchChanged {
        payload: WorkbenchBootstrap,
    },
    SessionChanged {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: SessionRuntime,
    },
    TurnEvent {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: WorkbenchTurnEvent,
    },
    SessionSettled {
        #[serde(rename = "sessionId")]
        session_id: String,
        payload: SessionSettledPayload,
    },
    ResyncRequired {
        payload: ResyncRequiredPayload,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct SessionSettledPayload {
    pub runtime: SessionRuntime,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct ResyncRequiredPayload {
    pub reason: String,
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
