#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use singularity_core::{ClientInfo, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Initialize,
    Initialized,
    ServerCapabilities,
    ThreadList,
    ThreadRead,
    ThreadStart,
    ThreadResume,
    ThreadFork,
    ThreadArchive,
    ThreadDelete,
    TurnStart,
    EvalRun,
    AgentCapability,
    TurnInterrupt,
    TurnStatus,
    ApprovalList,
    ApprovalCenter,
    ApprovalRequest,
    ApprovalDecision,
    EventSubscribe,
    ArtifactFetch,
    TraceList,
    TraceShow,
    TraceTail,
    ServerShutdown,
}

impl Method {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "initialize" => Self::Initialize,
            "initialized" => Self::Initialized,
            "server/capabilities" => Self::ServerCapabilities,
            "thread/list" => Self::ThreadList,
            "thread/read" => Self::ThreadRead,
            "thread/start" => Self::ThreadStart,
            "thread/resume" => Self::ThreadResume,
            "thread/fork" => Self::ThreadFork,
            "thread/archive" => Self::ThreadArchive,
            "thread/delete" => Self::ThreadDelete,
            "turn/start" => Self::TurnStart,
            "eval/run" => Self::EvalRun,
            "agent/capability" => Self::AgentCapability,
            "turn/interrupt" => Self::TurnInterrupt,
            "turn/status" => Self::TurnStatus,
            "approval/list" => Self::ApprovalList,
            "approval/center" => Self::ApprovalCenter,
            "approval/request" => Self::ApprovalRequest,
            "approval/decision" => Self::ApprovalDecision,
            "event/subscribe" => Self::EventSubscribe,
            "artifact/fetch" => Self::ArtifactFetch,
            "trace/list" => Self::TraceList,
            "trace/show" => Self::TraceShow,
            "trace/tail" => Self::TraceTail,
            "server/shutdown" => Self::ServerShutdown,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Initialized => "initialized",
            Self::ServerCapabilities => "server/capabilities",
            Self::ThreadList => "thread/list",
            Self::ThreadRead => "thread/read",
            Self::ThreadStart => "thread/start",
            Self::ThreadResume => "thread/resume",
            Self::ThreadFork => "thread/fork",
            Self::ThreadArchive => "thread/archive",
            Self::ThreadDelete => "thread/delete",
            Self::TurnStart => "turn/start",
            Self::EvalRun => "eval/run",
            Self::AgentCapability => "agent/capability",
            Self::TurnInterrupt => "turn/interrupt",
            Self::TurnStatus => "turn/status",
            Self::ApprovalList => "approval/list",
            Self::ApprovalCenter => "approval/center",
            Self::ApprovalRequest => "approval/request",
            Self::ApprovalDecision => "approval/decision",
            Self::EventSubscribe => "event/subscribe",
            Self::ArtifactFetch => "artifact/fetch",
            Self::TraceList => "trace/list",
            Self::TraceShow => "trace/show",
            Self::TraceTail => "trace/tail",
            Self::ServerShutdown => "server/shutdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JsonRpcMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    pub fn request(method: Method, id: Value, params: Value) -> Self {
        Self {
            jsonrpc: None,
            method: Some(method.as_str().to_string()),
            id: Some(id),
            params,
            result: None,
            error: None,
        }
    }

    pub fn response(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: None,
            method: None,
            id,
            params: Value::Null,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, error: ErrorCode) -> Self {
        Self {
            jsonrpc: None,
            method: None,
            id,
            params: Value::Null,
            result: None,
            error: Some(JsonRpcError {
                code: error.code,
                message: error.message,
            }),
        }
    }

    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: None,
            method: Some(method.into()),
            id: None,
            params,
            result: None,
            error: None,
        }
    }

    pub fn method(&self) -> Option<Method> {
        self.method.as_deref().and_then(Method::parse)
    }

    pub fn id(&self) -> Option<&Value> {
        self.id.as_ref()
    }

    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.params.clone())
    }

    pub fn to_wire_value(&self) -> Value {
        serde_json::to_value(self).expect("json-rpc message serializes")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeParams {
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "platformFamily")]
    pub platform_family: String,
    #[serde(rename = "platformOs")]
    pub platform_os: String,
}

impl InitializeResult {
    pub fn local() -> Self {
        Self {
            user_agent: concat!("singularity-app-server/", env!("CARGO_PKG_VERSION")).to_string(),
            platform_family: "local".to_string(),
            platform_os: std::env::consts::OS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerCapabilitiesResult {
    pub transports: Vec<TransportCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TransportCapability {
    pub transport: String,
    pub available: bool,
    #[serde(rename = "authTokenRequired")]
    pub auth_token_required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadStartParams {
    pub model: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadIdParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadReadParams {
    pub thread_id: String,
    pub before_turn_sequence: Option<u64>,
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadForkParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub status: ThreadStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadStartResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConversationMessage {
    pub item_id: String,
    pub turn_id: String,
    pub turn_sequence: u64,
    pub item_sequence: u64,
    pub role: ConversationRole,
    pub content: String,
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResult {
    pub thread: Thread,
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_turn_sequence: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadForkResult {
    #[serde(rename = "sourceThreadId")]
    pub source_thread_id: String,
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadDeleteResult {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnStartParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub input: Vec<InputItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub agent_loop_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Blocked,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnIdParams {
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    Plan,
    CommandExecution,
    FileChange,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    pub item_id: String,
    pub turn_id: String,
    pub kind: ItemKind,
    pub payload: Value,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Started,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnStartResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderReadiness {
    pub source: Option<String>,
    pub snapshot_id: String,
    pub ready: bool,
    pub blocker: Option<String>,
    pub api_key_present: bool,
    pub base_url_present: bool,
    pub model_present: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCapabilityResult {
    #[serde(rename = "agentLoop")]
    pub agent_loop: Value,
    #[serde(rename = "providerReadiness")]
    pub provider_readiness: ProviderReadiness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvalRunParams {
    pub manifest: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "outputRoot", skip_serializing_if = "Option::is_none")]
    pub output_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvalRunResult {
    pub run_id: String,
    pub manifest: String,
    pub runner: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    pub tasks: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    pub evaluation_passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnInterruptResult {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_loop_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalListResult {
    pub approvals: Vec<singularity_policy::ApprovalRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalCenterResult {
    #[serde(rename = "pendingApprovals")]
    pub pending_approvals: Vec<singularity_policy::ApprovalRequest>,
    pub decisions: Vec<singularity_policy::ApprovalDecision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventSubscribeParams {
    #[serde(rename = "eventTypes")]
    pub event_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventSubscribeResult {
    #[serde(rename = "subscriptionId")]
    pub subscription_id: String,
    #[serde(rename = "eventTypes")]
    pub event_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactFetchParams {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactFetchResult {
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceListParams {
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceShowParams {
    #[serde(rename = "eventId")]
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceTailParams {
    #[serde(rename = "runId")]
    pub run_id: String,
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceListResult {
    pub events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactRef {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "itemId", skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub kind: String,
    pub uri: String,
    #[serde(rename = "contentDigest")]
    pub content_digest: String,
    pub summary: String,
    pub metadata: Value,
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceEvent {
    pub event_id: String,
    pub event_type: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub phase_id: Option<String>,
    pub action_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub timestamp: Option<String>,
    pub monotonic_ms: Option<u64>,
    pub component: String,
    pub severity: String,
    pub summary: String,
    pub payload: Value,
    pub artifact_refs: Vec<String>,
    pub policy_decision_id: Option<String>,
    pub approval_grant_id: Option<String>,
    pub sandbox_id: Option<String>,
    pub command_id: Option<String>,
    pub transaction_id: Option<String>,
    pub verification_id: Option<String>,
    pub span_id: Option<String>,
    pub redaction_applied: bool,
    pub payload_hash: String,
}

impl TraceEvent {
    pub fn new(
        event_id: impl Into<String>,
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        component: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            event_type: "trace.event".to_string(),
            run_id: run_id.into(),
            session_id: session_id.into(),
            task_id: None,
            phase_id: None,
            action_id: None,
            parent_event_id: None,
            timestamp: None,
            monotonic_ms: None,
            component: component.into(),
            severity: "info".to_string(),
            summary: summary.into(),
            payload: Value::Null,
            artifact_refs: Vec::new(),
            policy_decision_id: None,
            approval_grant_id: None,
            sandbox_id: None,
            command_id: None,
            transaction_id: None,
            verification_id: None,
            span_id: None,
            redaction_applied: false,
            payload_hash: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AppEvent {
    pub method: String,
    pub params: Value,
}

impl AppEvent {
    pub fn thread_started(thread: &Thread) -> Self {
        Self {
            method: "thread/started".to_string(),
            params: serde_json::json!({"thread": thread}),
        }
    }

    pub fn turn_started(turn: &Turn) -> Self {
        Self {
            method: "turn/started".to_string(),
            params: serde_json::json!({"turn": turn}),
        }
    }

    pub fn turn_completed(turn: &Turn) -> Self {
        Self {
            method: "turn/completed".to_string(),
            params: serde_json::json!({"turn": turn}),
        }
    }

    pub fn turn_plan_updated(turn_id: impl Into<String>, plan: Value) -> Self {
        Self {
            method: "turn/plan/updated".to_string(),
            params: serde_json::json!({"turnId": turn_id.into(), "plan": plan}),
        }
    }

    pub fn turn_diff_updated(turn_id: impl Into<String>, diff: Value) -> Self {
        Self {
            method: "turn/diff/updated".to_string(),
            params: serde_json::json!({"turnId": turn_id.into(), "diff": diff}),
        }
    }

    pub fn item_started(item_id: impl Into<String>) -> Self {
        Self::item_event("item/started", item_id)
    }

    pub fn item_agent_message_delta(item_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self {
            method: "item/agentMessage/delta".to_string(),
            params: serde_json::json!({
                "item": {"item_id": item_id.into()},
                "delta": delta.into(),
            }),
        }
    }

    pub fn item_command_execution_output_delta(
        item_id: impl Into<String>,
        stream: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            method: "item/commandExecution/outputDelta".to_string(),
            params: serde_json::json!({
                "item": {"item_id": item_id.into()},
                "stream": stream.into(),
                "output": output.into(),
            }),
        }
    }

    pub fn item_completed(item_id: impl Into<String>) -> Self {
        Self::item_event("item/completed", item_id)
    }

    pub fn item_failed(item_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            method: "item/failed".to_string(),
            params: serde_json::json!({
                "item": {"item_id": item_id.into()},
                "error": error.into(),
            }),
        }
    }

    fn item_event(method: &'static str, item_id: impl Into<String>) -> Self {
        Self {
            method: method.to_string(),
            params: serde_json::json!({"item": {"item_id": item_id.into()}}),
        }
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn to_notification(&self) -> JsonRpcMessage {
        JsonRpcMessage::notification(self.method.clone(), self.params.clone())
    }
}
