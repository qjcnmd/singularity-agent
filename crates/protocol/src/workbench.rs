//! 本地 Web 工作台版本 1 合同。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{ThreadTurn, TurnEvent, TurnStatus};

pub const WORKBENCH_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    pub compaction_summary: Option<String>,
    pub turns: Vec<ThreadTurn>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlChannel {
    Steer,
    FollowUp,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlDisposition {
    Pending,
    Injected,
    StartedAsNewTurn,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlSnapshot {
    pub control_id: String,
    pub turn_id: String,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: Option<String>,
    pub disposition: ControlDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveTurnSnapshot {
    pub turn_id: String,
    pub events: Vec<WorkbenchTurnEvent>,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveCompactionSnapshot {
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTerminalSnapshot {
    pub status: TurnStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSnapshot {
    pub session_revision: u64,
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// durable control ledger 的完整归约投影，按接受 sequence 排序。
    pub controls: Vec<ControlSnapshot>,
    /// 当前仍位于 follow-up 队列中的控制；立即提升后会从这里消失，但其
    /// lifecycle 仍保留在 controls 中直至终态 disposition 落盘。
    pub pending_controls: Vec<ControlSnapshot>,
    pub active_turn: Option<ActiveTurnSnapshot>,
    pub active_compaction: Option<ActiveCompactionSnapshot>,
    pub terminal: Option<SessionTerminalSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelConfigurationStatus {
    Ready,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedReasoningVariant {
    pub id: String,
    pub enabled: bool,
    pub wire_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub api_protocol: String,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Vec<RedactedReasoningVariant>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedProvider {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub credential_configured: bool,
    pub models: Vec<RedactedModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedModelCatalog {
    pub configuration: ModelConfigurationStatus,
    pub message: Option<String>,
    pub default_selector: Option<String>,
    pub providers: Vec<RedactedProvider>,
    pub presets: Vec<ProviderConfigurationInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    Chat,
    Responses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningVariantInput {
    pub id: String,
    pub enabled: bool,
    pub wire_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelInput {
    pub model_id: String,
    pub display_name: Option<String>,
    pub api_protocol: ProviderApiProtocol,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_variants: Vec<ReasoningVariantInput>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConfigurationInput {
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ProviderModelInput>,
    pub make_default: bool,
}

/// A provider's advertised model, offered for explicit adoption into an editor draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoveredModel {
    pub model_id: String,
    pub display_name: Option<String>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Vec<ReasoningVariantInput>,
    pub default_variant: Option<String>,
    pub thinking_wire_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialConfigured {
    pub provider_id: String,
    pub credential_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryEntryKind {
    Root,
    Parent,
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub kind: DirectoryEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointSnapshot {
    pub authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    FullLocalAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionSnapshot {
    pub file_access: FileAccess,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchBootstrap {
    pub session_phases: std::collections::BTreeMap<String, SessionPhase>,
    pub generation: String,
    pub revision: u64,
    pub endpoint: EndpointSnapshot,
    pub workspaces: Vec<Workspace>,
    pub sessions_by_workspace: BTreeMap<String, Vec<ThreadSummary>>,
    pub model_catalog: RedactedModelCatalog,
    pub execution: ExecutionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReadResult {
    pub summary: ThreadSummary,
    pub history: ThreadReadPage,
    pub runtime: SessionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionReceipt {
    pub request_id: String,
    pub accepted: bool,
    pub generation: String,
    pub revision: u64,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub control: Option<ControlSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcMethod {
    #[serde(rename = "workbench.bootstrap")]
    WorkbenchBootstrap,
    #[serde(rename = "directory.list")]
    DirectoryList,
    #[serde(rename = "directory.pick")]
    DirectoryPick,
    #[serde(rename = "file.search")]
    FileSearch,
    #[serde(rename = "skills.list")]
    SkillsList,
    #[serde(rename = "workspace.add")]
    WorkspaceAdd,
    #[serde(rename = "workspace.remove")]
    WorkspaceRemove,
    #[serde(rename = "workspace.rename")]
    WorkspaceRename,
    #[serde(rename = "model.saveProvider")]
    ModelSaveProvider,
    #[serde(rename = "model.setApiKey")]
    ModelSetApiKey,
    #[serde(rename = "model.discover")]
    ModelDiscover,
    #[serde(rename = "model.removeProvider")]
    ModelRemoveProvider,
    #[serde(rename = "session.create")]
    SessionCreate,
    #[serde(rename = "session.read")]
    SessionRead,
    #[serde(rename = "session.request")]
    SessionRequest,
    #[serde(rename = "session.rename")]
    SessionRename,
    #[serde(rename = "session.archive")]
    SessionArchive,
    #[serde(rename = "session.submit")]
    SessionSubmit,
    #[serde(rename = "session.steer")]
    SessionSteer,
    #[serde(rename = "session.followUp")]
    SessionFollowUp,
    #[serde(rename = "session.queueWithdraw")]
    SessionQueueWithdraw,
    #[serde(rename = "session.queueReplace")]
    SessionQueueReplace,
    #[serde(rename = "session.queueSendNow")]
    SessionQueueSendNow,
    #[serde(rename = "session.abort")]
    SessionAbort,
    #[serde(rename = "session.compact")]
    SessionCompact,
    #[serde(rename = "session.updateSettings")]
    SessionUpdateSettings,
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
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    Unauthorized,
    ForbiddenOrigin,
    InvalidRequest,
    WorkspaceNotFound,
    WorkspaceBusy,
    SessionNotFound,
    SessionBusy,
    ControlNotFound,
    ConfigurationInvalid,
    Conflict,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub recovery: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcResponse {
    pub version: u16,
    pub request_id: String,
    pub ok: bool,
    pub generation: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Ready,
    WorkbenchChanged,
    SessionChanged,
    TurnEvent,
    SessionSettled,
    ResyncRequired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamEnvelope {
    pub version: u16,
    pub generation: String,
    pub revision: u64,
    #[serde(rename = "type")]
    pub event_type: StreamType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub payload: Value,
}
