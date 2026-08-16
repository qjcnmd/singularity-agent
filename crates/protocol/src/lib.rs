#![forbid(unsafe_code)]

//! stdio JSON-RPC 方法、生命周期事件和公共协议对象。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use singularity_core::{ClientInfo, ErrorCode, JSON_RPC_METHOD_NOT_FOUND};

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
    ThreadStart => ("thread/start", Request, ThreadStartParams, ThreadStartResult),
    ThreadResume => ("thread/resume", Request, ThreadIdParams, ThreadResult),
    SessionRead => ("session/read", Request, SessionReadParams, SessionReadResult),
    SessionDelete => ("session/delete", Request, SessionIdParams, SessionDeleteResult),
    TurnStart => ("turn/start", Request, TurnStartParams, TurnStartResult),
    TurnSteer => ("turn/steer", Request, TurnInjectionParams, TurnResult),
    TurnFollowUp => ("turn/followUp", Request, TurnInjectionParams, TurnResult),
    AgentCapability => ("agent/capability", Request, EmptyParams, AgentCapabilityResult),
    TurnInterrupt => ("turn/interrupt", Request, TurnIdParams, TurnInterruptResult),
    ProjectTrust => ("project/trust", Request, ProjectTrustParams, ProjectTrustResult),
    ServerShutdown => ("server/shutdown", Request, EmptyParams, ServerShutdownResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
/// JSON-RPC id；请求仅允许字符串或合法整数，无法关联的错误响应使用 null。
pub enum JsonRpcId {
    String(String),
    Number(i64),
    Unsigned(u64),
    Null,
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

impl From<i32> for JsonRpcId {
    fn from(value: i32) -> Self {
        Self::Number(i64::from(value))
    }
}

impl From<u64> for JsonRpcId {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<u32> for JsonRpcId {
    fn from(value: u32) -> Self {
        Self::Unsigned(u64::from(value))
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
    #[serde(deserialize_with = "deserialize_request_id")]
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
    pub id: JsonRpcId,
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
    match serde_json::from_value(id.clone()).ok()? {
        JsonRpcId::Null => None,
        id => Some(id),
    }
}

/// 请求 envelope 只接受字符串或合法整数；Null 仅保留给 response/error 关联边界。
fn deserialize_request_id<'de, D>(deserializer: D) -> Result<JsonRpcId, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let id = JsonRpcId::deserialize(deserializer)?;
    if matches!(id, JsonRpcId::Null) {
        return Err(serde::de::Error::custom(
            "JSON-RPC request id must be a string or integer",
        ));
    }
    Ok(id)
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
            id: id.into().unwrap_or(JsonRpcId::Null),
            error: JsonRpcError {
                code: error.code,
                message: error.message,
                data: None,
            },
        })
    }

    /// 构造携带已脱敏 data 的 error response（data 仅允许调用方显式提供）。
    pub fn error_with_data(
        id: impl Into<Option<JsonRpcId>>,
        error: ErrorCode,
        data: Value,
    ) -> Self {
        Self::Error(JsonRpcErrorResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: id.into().unwrap_or(JsonRpcId::Null),
            error: JsonRpcError {
                code: error.code,
                message: error.message,
                data: Some(data),
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
            Self::Error(message) => Some(&message.id),
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
/// 只包含 session id 的请求参数。
pub struct SessionIdParams {
    pub session_id: String,
}

fn default_session_recent_limit() -> u32 {
    20
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 查看会话的参数：默认返回摘要 + 最近片段，不返回全文。
pub struct SessionReadParams {
    pub session_id: String,
    #[serde(default = "default_session_recent_limit")]
    pub recent_limit: u32,
    /// 过滤后的路径条目起始偏移（默认从 0 开始）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// 条目类型过滤；空数组 = 全部，只接受 `message` / `compaction`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
/// session/read 的响应：摘要 + 最近片段（不携带完整 rollout）。
pub struct SessionReadResult {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub token_usage: Value,
    /// 最近一次 compaction 摘要；无 compaction 时为 None。
    pub summary: Option<String>,
    /// 当前 leaf 路径上最近 `recent_limit` 条会话条目。
    pub recent_entries: Vec<Value>,
    /// 会话文件中的条目总数（不含 header）。
    pub total_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
/// session/delete 的响应。
pub struct SessionDeleteResult {
    pub session_id: String,
    pub deleted: bool,
}

/// 持久化 thread（session）的公开摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// 最近一次/当前一次 turn 的展示元数据，来自 `session_index.status`。
    /// `sg continue` 不受此字段限制。
    #[serde(rename = "lastTurnStatus")]
    pub last_turn_status: ThreadStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// `session_index.status` 的协议投影：最近一次/当前一次 turn 的状态。
pub enum ThreadStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

impl ThreadStatus {
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 向同一连接内运行中的 turn 注入用户输入（turn/steer、turn/followUp）。
pub struct TurnInjectionParams {
    pub turn_id: String,
    pub input: Vec<InputItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
/// 持久化 turn 的公开摘要。
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub agent_loop_status: String,
    /// 进程内聚合的 provider usage 投影（评估工具数据源）。
    ///
    /// 可选字段保持协议向后兼容：旧客户端读新响应时忽略未知字段，
    /// 新客户端读旧服务端时字段缺失回退为 None。usage 不持久化（裁决 6），
    /// 仅 app-server 进程内可提供；进程重启后查询历史 turn 为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_usage: Option<TurnModelUsage>,
}

/// 模型 usage 的协议线格式（与 `singularity_model::ModelUsage` 同构，
/// 避免 protocol 依赖 model crate）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct TurnModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 各轮均提供时才有成本估算；否则为 None。
    pub cost_estimate: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// turn 的生命周期状态；暂停/挂起状态机已删除。
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub const fn as_storage_text(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// CLI 所需的脱敏 AgentLoop capability 投影。
pub struct AgentLoopCapabilityStatus {
    pub available: bool,
    pub status: String,
    pub reason: String,
    pub blockers: Vec<String>,
}

/// server/shutdown 的类型化响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerShutdownResult {
    pub shutdown: bool,
}
/// 查询或设置项目信任决策的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectTrustParams {
    pub path: String,
    /// 决策操作：字段缺失=查询；`true`/`false`=设置；`null`=重置为 ask（清除记录）。
    #[serde(
        default,
        skip_serializing_if = "ProjectTrustDecision::is_query",
        deserialize_with = "deserialize_project_trust_decision"
    )]
    pub decision: ProjectTrustDecision,
}

/// project/trust 的决策操作（wire：缺失=查询、bool=设置、null=重置为 ask）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProjectTrustDecision {
    Set(bool),
    Ask,
    #[default]
    Query,
}

impl ProjectTrustDecision {
    fn is_query(&self) -> bool {
        matches!(self, Self::Query)
    }
}

fn deserialize_project_trust_decision<'de, D>(
    deserializer: D,
) -> Result<ProjectTrustDecision, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 字段存在时解析：null → Ask（重置），bool → Set；其他值类型不匹配报错。
    match Option::<bool>::deserialize(deserializer)? {
        Some(trusted) => Ok(ProjectTrustDecision::Set(trusted)),
        None => Ok(ProjectTrustDecision::Ask),
    }
}

/// project/trust 的响应：当前存储的决策（无记录时 `decision` 缺失）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTrustResult {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<bool>,
}

/// 对外广播的应用事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AppEvent {
    pub method: String,
    pub params: Value,
}

/// 事件的稳定语义分类；客户端据此选择可靠处理或可观察丢弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventClass {
    State,
    Progress,
    Gap,
}

/// 事件在 stdio 传输上的交付合同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventDelivery {
    Reliable,
    BestEffort,
    Gap,
}

/// 事件 gap 的稳定原因分类。
///
/// 单 worker 传输不再产生 gap（无背压丢弃），此枚举保留为协议合同面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventGapReason {
    CursorNotReplayed,
    ProgressDropped,
}

/// JSON-RPC notification 中附带的严格事件元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventMetadata {
    pub class: EventClass,
    pub delivery: EventDelivery,
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

    /// 将带有严格传输元数据的应用事件包装为 JSON-RPC 通知。
    pub fn to_notification_with_metadata(&self, metadata: EventMetadata) -> JsonRpcMessage {
        let mut params = match self.params.clone() {
            Value::Object(params) => params,
            _ => serde_json::Map::new(),
        };
        params.insert(
            "event".to_string(),
            serde_json::to_value(metadata).expect("event metadata serializes"),
        );
        JsonRpcMessage::notification(self.method.clone(), Value::Object(params))
            .expect("application event params serialize")
    }
}
