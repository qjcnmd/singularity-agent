#![forbid(unsafe_code)]

//! stdio JSON-RPC 方法、生命周期事件和公共协议对象。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use singularity_core::{ClientInfo, ErrorCode, JSON_RPC_METHOD_NOT_FOUND};
pub use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionProfileName,
};

/// JSON-RPC 2.0 parse error code。
pub const JSON_RPC_PARSE_ERROR: i64 = -32700;

/// JSON-RPC method 的调用类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Request,
    Notification,
}

/// 单个 method 的参数和结果合同。
#[derive(Clone, Copy)]
pub struct MethodSpec {
    pub method: Method,
    pub name: &'static str,
    pub kind: MethodKind,
    params_schema: fn() -> Value,
    result_schema: fn() -> Value,
    validate_params: fn(Value) -> Result<(), String>,
}

impl std::fmt::Debug for MethodSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MethodSpec")
            .field("method", &self.method)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl MethodSpec {
    /// 返回 params 的 JSON Schema。
    pub fn params_schema(self) -> Value {
        (self.params_schema)()
    }

    /// 返回 result 的 JSON Schema。
    pub fn result_schema(self) -> Value {
        (self.result_schema)()
    }

    /// 按该 method 唯一登记的参数合同校验 params。
    pub fn validate_params(self, params: Value) -> Result<(), String> {
        (self.validate_params)(params)
    }
}

fn schema_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("JSON schema serializes")
}

fn validate_params<T: DeserializeOwned>(params: Value) -> Result<(), String> {
    serde_json::from_value::<T>(params)
        .map(|_| ())
        .map_err(|_| "params do not match the registered method contract".to_string())
}

/// 由 method registry 生成的 typed params/result 关联。
pub trait RpcMethod {
    const METHOD: Method;
    type Params: Serialize;
    type Result: DeserializeOwned;
}

macro_rules! method_registry {
    ($( $variant:ident => ($name:literal, $kind:ident, $params:ty, $result:ty) ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        /// JSON-RPC 方法名。
        pub enum Method {
            $( $variant, )+
        }

        /// 唯一的公共 method registry；方法查找和参数/结果合同都由此生成。
        pub const METHOD_REGISTRY: &[MethodSpec] = &[
            $( MethodSpec {
                method: Method::$variant,
                name: $name,
                kind: MethodKind::$kind,
                params_schema: schema_value::<$params>,
                result_schema: schema_value::<$result>,
                validate_params: validate_params::<$params>,
            }, )+
        ];

        impl Method {
            /// 将线上的方法字符串解析为协议枚举。
            pub fn parse(value: &str) -> Option<Self> {
                METHOD_REGISTRY
                    .iter()
                    .find(|spec| spec.name == value)
                    .map(|spec| spec.method)
            }

            /// 返回方法的 JSON-RPC 字符串。
            pub fn as_str(self) -> &'static str {
                self.spec().name
            }

            /// 返回该方法在唯一 registry 中的合同。
            pub fn spec(self) -> &'static MethodSpec {
                METHOD_REGISTRY
                    .iter()
                    .find(|spec| spec.method == self)
                    .expect("every Method variant is registered")
            }
        }

        /// 每个 marker 的 associated types 都由同一 method registry 条目生成。
        pub mod rpc_methods {
            use super::*;

            $(
                pub struct $variant;

                impl RpcMethod for $variant {
                    const METHOD: Method = Method::$variant;
                    type Params = $params;
                    type Result = $result;
                }
            )+
        }
    };
}

method_registry! {
    Initialize => ("initialize", Request, InitializeParams, InitializeResult),
    Initialized => ("initialized", Notification, EmptyParams, EmptyResult),
    ServerCapabilities => ("server/capabilities", Request, EmptyParams, ServerCapabilitiesResult),
    ThreadList => ("thread/list", Request, EmptyParams, ThreadListResult),
    ThreadRead => ("thread/read", Request, ThreadReadParams, ThreadReadResult),
    ThreadStart => ("thread/start", Request, ThreadStartParams, ThreadStartResult),
    ThreadResume => ("thread/resume", Request, ThreadIdParams, ThreadResult),
    ThreadFork => ("thread/fork", Request, ThreadForkParams, ThreadForkResult),
    ThreadArchive => ("thread/archive", Request, ThreadIdParams, ThreadResult),
    ThreadDelete => ("thread/delete", Request, ThreadIdParams, ThreadDeleteResult),
    TurnStart => ("turn/start", Request, TurnStartParams, TurnStartResult),
    EvalRun => ("eval/run", Request, EvalRunParams, EvalRunResult),
    AgentCapability => ("agent/capability", Request, EmptyParams, AgentCapabilityResult),
    TurnInterrupt => ("turn/interrupt", Request, TurnIdParams, TurnInterruptResult),
    TurnStatus => ("turn/status", Request, TurnIdParams, TurnResult),
    ApprovalList => ("approval/list", Request, EmptyParams, ApprovalListResult),
    ApprovalCenter => ("approval/center", Request, EmptyParams, ApprovalCenterResult),
    ApprovalRequest => ("approval/request", Request, singularity_policy::ApprovalRequest, EmptyResult),
    ApprovalDecision => ("approval/decision", Request, singularity_policy::ApprovalDecision, ApprovalDecisionResult),
    EventSubscribe => ("event/subscribe", Request, EventSubscribeParams, EventSubscribeResult),
    ArtifactFetch => ("artifact/fetch", Request, ArtifactFetchParams, ArtifactFetchResult),
    TraceList => ("trace/list", Request, TraceListParams, TraceListResult),
    TraceShow => ("trace/show", Request, TraceShowParams, TraceShowResult),
    TraceTail => ("trace/tail", Request, TraceTailParams, TraceListResult),
    ServerShutdown => ("server/shutdown", Request, EmptyParams, ServerShutdownResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
/// JSON-RPC request id；只允许字符串或整数。
pub enum JsonRpcId {
    String(String),
    Number(i64),
}

impl From<i64> for JsonRpcId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for JsonRpcId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for JsonRpcId {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
enum JsonRpcVersion {
    #[serde(rename = "2.0")]
    V2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 带 id 的 JSON-RPC 请求。
pub struct JsonRpcRequest {
    jsonrpc: JsonRpcVersion,
    pub method: String,
    pub id: JsonRpcId,
    #[serde(default = "empty_params")]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 不带 id 的 JSON-RPC notification。
pub struct JsonRpcNotification {
    jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default = "empty_params")]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// JSON-RPC success response。
pub struct JsonRpcSuccess {
    jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// JSON-RPC error response。
pub struct JsonRpcErrorResponse {
    jsonrpc: JsonRpcVersion,
    pub id: Option<JsonRpcId>,
    pub error: JsonRpcError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
/// 互斥的 JSON-RPC request、notification、success 或 error envelope。
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Success(JsonRpcSuccess),
    Error(JsonRpcErrorResponse),
}

#[derive(Debug, Clone, PartialEq)]
/// batch 中一项已解析消息或结构无效项。
pub enum JsonRpcBatchItem {
    Message(JsonRpcMessage),
    Invalid { id: Option<JsonRpcId> },
}

#[derive(Debug, Clone, PartialEq)]
/// 一条 JSONL frame 携带的单消息或 batch payload。
pub enum JsonRpcPayload {
    Single(JsonRpcBatchItem),
    Batch(Vec<JsonRpcBatchItem>),
    EmptyBatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// JSON 文本在形成 JSON-RPC payload 前即无法解析。
pub struct JsonRpcParseError;

impl std::fmt::Display for JsonRpcParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid JSON")
    }
}

impl std::error::Error for JsonRpcParseError {}

/// 解析单个 JSONL frame，同时保留 batch 内逐项无效请求语义。
pub fn parse_json_rpc_payload(input: &str) -> Result<JsonRpcPayload, JsonRpcParseError> {
    let value: Value = serde_json::from_str(input).map_err(|_| JsonRpcParseError)?;
    match value {
        Value::Array(values) if values.is_empty() => Ok(JsonRpcPayload::EmptyBatch),
        Value::Array(values) => Ok(JsonRpcPayload::Batch(
            values.into_iter().map(parse_batch_item).collect(),
        )),
        value => Ok(JsonRpcPayload::Single(parse_batch_item(value))),
    }
}

fn parse_batch_item(value: Value) -> JsonRpcBatchItem {
    match serde_json::from_value::<JsonRpcMessage>(value.clone()) {
        Ok(message) => JsonRpcBatchItem::Message(message),
        Err(_) => JsonRpcBatchItem::Invalid {
            id: recover_typed_id(&value),
        },
    }
}

fn recover_typed_id(value: &Value) -> Option<JsonRpcId> {
    let id = value.as_object()?.get("id")?;
    serde_json::from_value(id.clone()).ok()
}

impl JsonRpcMessage {
    /// 使用已登记 method、typed id 和可序列化 params 构造请求。
    pub fn request(
        method: Method,
        id: impl Into<JsonRpcId>,
        params: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        let params = serde_json::to_value(params)?;
        Ok(Self::Request(JsonRpcRequest {
            jsonrpc: JsonRpcVersion::V2,
            method: method.as_str().to_string(),
            id: id.into(),
            params,
        }))
    }

    /// 构造 success response。
    pub fn response(id: impl Into<JsonRpcId>, result: Value) -> Self {
        Self::Success(JsonRpcSuccess {
            jsonrpc: JsonRpcVersion::V2,
            id: id.into(),
            result,
        })
    }

    /// 构造 error response；未知 request id 以标准 null 表示。
    pub fn error(id: impl Into<Option<JsonRpcId>>, error: ErrorCode) -> Self {
        Self::Error(JsonRpcErrorResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: id.into(),
            error: JsonRpcError {
                code: error.code,
                message: error.message,
                data: None,
            },
        })
    }

    /// 构造不携带解析细节的标准 parse error。
    pub fn parse_error() -> Self {
        Self::error(None, ErrorCode::new(JSON_RPC_PARSE_ERROR, "Parse error"))
    }

    /// 构造标准 invalid request error。
    pub fn invalid_request(id: Option<JsonRpcId>) -> Self {
        Self::error(id, ErrorCode::invalid_request("Invalid Request"))
    }

    /// 构造不回显不可信 method 名的标准 method-not-found error。
    pub fn method_not_found(id: Option<JsonRpcId>) -> Self {
        Self::error(
            id,
            ErrorCode::new(JSON_RPC_METHOD_NOT_FOUND, "Method not found"),
        )
    }

    /// 构造 notification。
    pub fn notification(
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::Notification(JsonRpcNotification {
            jsonrpc: JsonRpcVersion::V2,
            method: method.into(),
            params: serde_json::to_value(params)?,
        }))
    }

    /// 解析消息中的已知方法名。
    pub fn method(&self) -> Option<Method> {
        self.method_name().and_then(Method::parse)
    }

    /// 返回 request 或 notification 的 method 名。
    pub fn method_name(&self) -> Option<&str> {
        match self {
            Self::Request(message) => Some(&message.method),
            Self::Notification(message) => Some(&message.method),
            Self::Success(_) | Self::Error(_) => None,
        }
    }

    /// 返回 request 或 response 的 typed id。
    pub fn id(&self) -> Option<&JsonRpcId> {
        match self {
            Self::Request(message) => Some(&message.id),
            Self::Success(message) => Some(&message.id),
            Self::Error(message) => message.id.as_ref(),
            Self::Notification(_) => None,
        }
    }

    /// 返回调用参数。
    pub fn params(&self) -> Option<&Value> {
        match self {
            Self::Request(message) => Some(&message.params),
            Self::Notification(message) => Some(&message.params),
            Self::Success(_) | Self::Error(_) => None,
        }
    }

    /// 将 params 反序列化为调用方类型。
    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.params().cloned().unwrap_or(Value::Null))
    }

    /// 判断消息是否为 notification。
    pub fn is_notification(&self) -> bool {
        matches!(self, Self::Notification(_))
    }

    /// 返回 dispatcher 已保证存在的 request id。
    pub fn required_id(&self) -> JsonRpcId {
        self.id()
            .cloned()
            .expect("dispatcher supplies a request id")
    }

    /// 将 notification 转为只供 dispatcher 内部执行的带 id 请求。
    pub fn into_request_with_id(self, id: JsonRpcId) -> Self {
        match self {
            Self::Notification(message) => Self::Request(JsonRpcRequest {
                jsonrpc: JsonRpcVersion::V2,
                method: message.method,
                id,
                params: message.params,
            }),
            message => message,
        }
    }

    /// 生成发送到 stdio 的 JSON 值。
    pub fn to_wire_value(&self) -> Value {
        serde_json::to_value(self).expect("json-rpc message serializes")
    }
}

fn empty_params() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// JSON-RPC 错误对象；data 仅允许调用方显式提供已脱敏内容。
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 无参数 method 的严格空对象。
pub struct EmptyParams {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 无结果 notification 或永不成功 method 的占位合同。
pub struct EmptyResult {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 初始化请求参数。
pub struct InitializeParams {
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 初始化响应及平台摘要。
pub struct InitializeResult {
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "platformFamily")]
    pub platform_family: String,
    #[serde(rename = "platformOs")]
    pub platform_os: String,
}

impl InitializeResult {
    /// 构造本地 app-server 的初始化结果。
    pub fn local() -> Self {
        Self {
            user_agent: concat!("singularity-app-server/", env!("CARGO_PKG_VERSION")).to_string(),
            platform_family: "local".to_string(),
            platform_os: std::env::consts::OS.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 服务端支持的传输能力集合。
pub struct ServerCapabilitiesResult {
    pub transports: Vec<TransportCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// 单项传输能力及认证要求。
pub struct TransportCapability {
    pub transport: String,
    pub available: bool,
    #[serde(rename = "authTokenRequired")]
    pub auth_token_required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 创建 thread 的参数。
pub struct ThreadStartParams {
    pub model: Option<String>,
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<PermissionProfileName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 只包含 thread id 的请求参数。
pub struct ThreadIdParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 读取 thread 历史的参数。
pub struct ThreadReadParams {
    pub thread_id: String,
    pub before_turn_sequence: Option<u64>,
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// fork thread 的参数。
pub struct ThreadForkParams {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<PermissionProfileName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 持久化 thread 的公开摘要。
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub status: ThreadStatus,
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: PermissionProfileName,
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: ApprovalPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// thread 的生命周期状态。
pub enum ThreadStatus {
    Active,
    Archived,
}

impl ThreadStatus {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    /// 从 SQLite 的稳定文本值恢复状态；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// thread/start 的响应。
pub struct ThreadStartResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// thread/list 的响应。
pub struct ThreadListResult {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 返回单个 thread 的响应。
pub struct ThreadResult {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// 对话消息角色。
pub enum ConversationRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
/// thread 历史中的一条对话消息。
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
/// thread/read 的响应。
pub struct ThreadReadResult {
    pub thread: Thread,
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_turn_sequence: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// thread/fork 的响应。
pub struct ThreadForkResult {
    #[serde(rename = "sourceThreadId")]
    pub source_thread_id: String,
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// thread/delete 的响应。
pub struct ThreadDeleteResult {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// 启动 turn 的参数。
pub struct TurnStartParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub input: Vec<InputItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
/// 用户提交给 turn 的输入项。
pub enum InputItem {
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 持久化 turn 的公开摘要。
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub agent_loop_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// turn 的生命周期状态。
pub enum TurnStatus {
    Running,
    Completed,
    Blocked,
    Failed,
    Interrupted,
}

impl TurnStatus {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// 从 SQLite 的稳定文本值恢复状态；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 只包含 turn id 的请求参数。
pub struct TurnIdParams {
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
/// turn 输出 item 的类型。
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    Plan,
    CommandExecution,
    FileChange,
}

impl ItemKind {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::UserMessage => "userMessage",
            Self::AgentMessage => "agentMessage",
            Self::Reasoning => "reasoning",
            Self::Plan => "plan",
            Self::CommandExecution => "commandExecution",
            Self::FileChange => "fileChange",
        }
    }

    /// 从 SQLite 的稳定文本值恢复 item 类型；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "userMessage" => Some(Self::UserMessage),
            "agentMessage" => Some(Self::AgentMessage),
            "reasoning" => Some(Self::Reasoning),
            "plan" => Some(Self::Plan),
            "commandExecution" => Some(Self::CommandExecution),
            "fileChange" => Some(Self::FileChange),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// turn 输出中的一条 item。
pub struct Item {
    pub item_id: String,
    pub turn_id: String,
    pub kind: ItemKind,
    pub payload: Value,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// item 的生命周期状态。
pub enum ItemStatus {
    Started,
    Completed,
}

impl ItemStatus {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
        }
    }

    /// 从 SQLite 的稳定文本值恢复 item 状态；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "started" => Some(Self::Started),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// turn/start 的响应。
pub struct TurnStartResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// agent capability 查询的响应。
pub struct AgentCapabilityResult {
    #[serde(rename = "agentLoop")]
    pub agent_loop: AgentLoopCapabilityStatus,
    #[serde(rename = "providerConfiguration")]
    pub provider_configuration: ProviderConfigurationStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 启动 Evaluation 的参数。
pub struct EvalRunParams {
    pub manifest: String,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "outputRoot", skip_serializing_if = "Option::is_none")]
    pub output_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// Evaluation 启动结果。
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
    pub evaluation_passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// turn/status 的响应。
pub struct TurnResult {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// turn/interrupt 的响应。
pub struct TurnInterruptResult {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_loop_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// approval/list 的响应。
pub struct ApprovalListResult {
    pub approvals: Vec<singularity_policy::ApprovalRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// approval 中心的待处理请求和决策。
pub struct ApprovalCenterResult {
    #[serde(rename = "pendingApprovals")]
    pub pending_approvals: Vec<singularity_policy::ApprovalRequest>,
    pub decisions: Vec<singularity_policy::ApprovalDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// CLI 所需的脱敏 AgentLoop capability 投影。
pub struct AgentLoopCapabilityStatus {
    pub available: bool,
    pub status: String,
    pub reason: String,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// approval/decision 的类型化响应。
pub struct ApprovalDecisionResult {
    pub decision: singularity_policy::ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 事件订阅请求参数。
pub struct EventSubscribeParams {
    #[serde(rename = "eventTypes")]
    pub event_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// 事件订阅建立后的摘要。
pub struct EventSubscribeResult {
    #[serde(rename = "subscriptionId")]
    pub subscription_id: String,
    #[serde(rename = "eventTypes")]
    pub event_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// 获取 artifact 的参数。
pub struct ArtifactFetchParams {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// artifact/fetch 的响应。
pub struct ArtifactFetchResult {
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// trace/list 的筛选参数。
pub struct TraceListParams {
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// trace/show 的参数。
pub struct TraceShowParams {
    #[serde(rename = "eventId")]
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// trace/tail 的分页参数。
pub struct TraceTailParams {
    #[serde(rename = "runId")]
    pub run_id: String,
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// trace/list 的响应。
pub struct TraceListResult {
    pub events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// trace/show 的类型化响应。
pub struct TraceShowResult {
    pub event: TraceEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// server/shutdown 的类型化响应。
pub struct ServerShutdownResult {
    pub shutdown: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 公共 artifact 引用。
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
/// 脱敏后的 trace 事件。
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

/// Trace 与一个 turn 绑定时必须满足的身份不变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceBindingError {
    /// trace 的 run id 没有指向所属 thread。
    RunIdMismatch { expected: String, actual: String },
    /// trace 的 session id 没有指向所属 turn。
    SessionIdMismatch { expected: String, actual: String },
    /// trace 的 task id 没有指向所属 turn。
    TaskIdMismatch {
        expected: String,
        actual: Option<String>,
    },
}

impl std::fmt::Display for TraceBindingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RunIdMismatch { expected, actual } => write!(
                formatter,
                "turn trace run_id must match thread_id (expected {expected}, got {actual})"
            ),
            Self::SessionIdMismatch { expected, actual } => write!(
                formatter,
                "turn trace session_id must match turn_id (expected {expected}, got {actual})"
            ),
            Self::TaskIdMismatch { expected, actual } => write!(
                formatter,
                "turn trace task_id must match turn_id (expected {expected}, got {actual:?})"
            ),
        }
    }
}

impl std::error::Error for TraceBindingError {}

impl TraceEvent {
    /// 构造带默认严重级别和空 payload 的 trace 事件。
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

    /// 构造绑定到指定 thread/turn 的 trace 事件。
    pub fn for_turn(
        event_id: impl Into<String>,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        component: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        let thread_id = thread_id.into();
        let turn_id = turn_id.into();
        let mut event = Self::new(
            event_id,
            thread_id.clone(),
            turn_id.clone(),
            component,
            summary,
        );
        event.task_id = Some(turn_id);
        event
    }

    /// 校验 trace 是否绑定到指定 thread/turn。
    pub fn validate_turn_binding(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), TraceBindingError> {
        if self.run_id != thread_id {
            return Err(TraceBindingError::RunIdMismatch {
                expected: thread_id.to_string(),
                actual: self.run_id.clone(),
            });
        }
        if self.session_id != turn_id {
            return Err(TraceBindingError::SessionIdMismatch {
                expected: turn_id.to_string(),
                actual: self.session_id.clone(),
            });
        }
        if self.task_id.as_deref() != Some(turn_id) {
            return Err(TraceBindingError::TaskIdMismatch {
                expected: turn_id.to_string(),
                actual: self.task_id.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 对外广播的应用事件。
pub struct AppEvent {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// thread 生命周期 notification 的类型化参数。
pub struct ThreadEventParams {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// turn 生命周期 notification 的类型化参数。
pub struct TurnEventParams {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// item notification 中的最小 item 引用。
pub struct ItemReference {
    pub item_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// item notification 的公共可渲染参数。
pub struct ItemEventParams {
    pub item: ItemReference,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

impl AppEvent {
    /// 构造 thread started 事件。
    pub fn thread_started(thread: &Thread) -> Self {
        Self {
            method: "thread/started".to_string(),
            params: serde_json::json!({"thread": thread}),
        }
    }

    /// 构造 turn started 事件。
    pub fn turn_started(turn: &Turn) -> Self {
        Self {
            method: "turn/started".to_string(),
            params: serde_json::json!({"turn": turn}),
        }
    }

    /// 构造 turn completed 事件。
    pub fn turn_completed(turn: &Turn) -> Self {
        Self {
            method: "turn/completed".to_string(),
            params: serde_json::json!({"turn": turn}),
        }
    }

    /// 构造 plan 更新事件。
    pub fn turn_plan_updated(turn_id: impl Into<String>, plan: Value) -> Self {
        Self {
            method: "turn/plan/updated".to_string(),
            params: serde_json::json!({"turnId": turn_id.into(), "plan": plan}),
        }
    }

    /// 构造 diff 更新事件。
    pub fn turn_diff_updated(turn_id: impl Into<String>, diff: Value) -> Self {
        Self {
            method: "turn/diff/updated".to_string(),
            params: serde_json::json!({"turnId": turn_id.into(), "diff": diff}),
        }
    }

    /// 构造 item started 事件。
    pub fn item_started(item_id: impl Into<String>) -> Self {
        Self::item_event("item/started", item_id)
    }

    /// 构造 agent message 增量事件。
    pub fn item_agent_message_delta(item_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self {
            method: "item/agentMessage/delta".to_string(),
            params: serde_json::json!({
                "item": {"item_id": item_id.into()},
                "delta": delta.into(),
            }),
        }
    }

    /// 构造 command 输出增量事件。
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

    /// 构造 item completed 事件。
    pub fn item_completed(item_id: impl Into<String>) -> Self {
        Self::item_event("item/completed", item_id)
    }

    /// 构造 item failed 事件。
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

    /// 返回事件方法名。
    pub fn method(&self) -> &str {
        &self.method
    }

    /// 将应用事件包装为 JSON-RPC 通知。
    pub fn to_notification(&self) -> JsonRpcMessage {
        JsonRpcMessage::notification(self.method.clone(), &self.params)
            .expect("application event params serialize")
    }
}
