#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use singularity_core::{ClientInfo, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Initialize,
    Initialized,
    ThreadStart,
    TurnStart,
    ApprovalRequest,
    ApprovalDecision,
    TraceList,
    TraceShow,
}

impl Method {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "initialize" => Self::Initialize,
            "initialized" => Self::Initialized,
            "thread/start" => Self::ThreadStart,
            "turn/start" => Self::TurnStart,
            "approval/request" => Self::ApprovalRequest,
            "approval/decision" => Self::ApprovalDecision,
            "trace/list" => Self::TraceList,
            "trace/show" => Self::TraceShow,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Initialized => "initialized",
            Self::ThreadStart => "thread/start",
            Self::TurnStart => "turn/start",
            Self::ApprovalRequest => "approval/request",
            Self::ApprovalDecision => "approval/decision",
            Self::TraceList => "trace/list",
            Self::TraceShow => "trace/show",
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
            user_agent: "singularity-rust-app-server/0.1.0".to_string(),
            platform_family: "local".to_string(),
            platform_os: std::env::consts::OS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadStartParams {
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
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    InputMessage,
    AgentMessage,
    ToolCall,
    CommandRun,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceListParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TraceShowParams {
    pub event_id: String,
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
            redaction_applied: true,
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
    pub fn item_started(item_id: impl Into<String>) -> Self {
        Self::item_event("item/started", item_id)
    }

    pub fn item_delta(item_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self {
            method: "item/delta".to_string(),
            params: serde_json::json!({
                "item": {"item_id": item_id.into()},
                "delta": delta.into(),
            }),
        }
    }

    pub fn item_completed(item_id: impl Into<String>) -> Self {
        Self::item_event("item/completed", item_id)
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
