#![forbid(unsafe_code)]

//! stdio JSON-RPC 方法、生命周期事件和公共协议对象。

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
    /// 按该 method 唯一登记的参数合同校验 params。
    pub fn validate_params(self, params: Value) -> Result<(), String> {
        (self.validate_params)(params)
    }
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
    ThreadList => ("thread/list", Request, EmptyParams, ThreadListResult),
    ThreadStart => ("thread/start", Request, ThreadStartParams, ThreadStartResult),
    ThreadSettings => ("thread/settings", Request, ThreadSettingsParams, ThreadSettingsResult),
    ThreadRead => ("thread/read", Request, ThreadReadParams, ThreadReadResult),
    SessionDelete => ("session/delete", Request, SessionIdParams, SessionDeleteResult),
    TurnStart => ("turn/start", Request, TurnStartParams, TurnStartResult),
    TurnSteer => ("turn/steer", Request, TurnInjectionParams, TurnInjectionResult),
    TurnFollowUp => ("turn/followUp", Request, TurnInjectionParams, TurnInjectionResult),
    ProviderStatus => ("provider/status", Request, EmptyParams, ProviderConfigurationStatus),
    TurnInterrupt => ("turn/interrupt", Request, TurnIdParams, TurnInterruptResult),
    ServerShutdown => ("server/shutdown", Request, EmptyParams, ServerShutdownResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum JsonRpcVersion {
    #[serde(rename = "2.0")]
    V2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 不带 id 的 JSON-RPC notification。
pub struct JsonRpcNotification {
    jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default = "empty_params")]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// JSON-RPC success response。
pub struct JsonRpcSuccess {
    jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// JSON-RPC error response。
pub struct JsonRpcErrorResponse {
    jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub error: JsonRpcError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
/// 互斥的 JSON-RPC request、notification、success 或 error envelope。
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Success(JsonRpcSuccess),
    Error(JsonRpcErrorResponse),
}

#[derive(Debug, Clone, PartialEq)]
/// 一条 JSONL frame 解析得到的入站消息，或结构无效但可恢复 id 的帧。
pub enum JsonRpcInbound {
    Message(JsonRpcMessage),
    Invalid { id: Option<JsonRpcId> },
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

/// 解析单个 JSONL frame；有效消息作为 `Message`，无法形成消息的对象保留可恢复 id。
/// 顶层数组（JSON-RPC batch）没有 stdio 消费者，按结构无效拒绝，且数组无法携带 id。
pub fn parse_json_rpc_payload(input: &str) -> Result<JsonRpcInbound, JsonRpcParseError> {
    let value: Value = serde_json::from_str(input).map_err(|_| JsonRpcParseError)?;
    match serde_json::from_value::<JsonRpcMessage>(value.clone()) {
        Ok(message) => Ok(JsonRpcInbound::Message(message)),
        Err(_) => Ok(JsonRpcInbound::Invalid {
            id: recover_typed_id(&value),
        }),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// JSON-RPC 错误对象；data 仅允许调用方显式提供已脱敏内容。
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 无参数 method 的严格空对象。
pub struct EmptyParams {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// 无结果 notification 或永不成功 method 的占位合同。
pub struct EmptyResult {}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
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
/// session/delete 的响应。
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
    pub cwd: Option<String>,
    /// 最近一次/当前一次 turn 的展示元数据，来自 `session_index.status`：
    /// 尚无 turn 时为 `None`（wire 上为 null），运行中为 active，终态为
    /// completed/failed/interrupted。`sg continue` 不受此字段限制。
    #[serde(rename = "lastTurnStatus")]
    pub last_turn_status: Option<ThreadStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(rename_all = "camelCase")]
/// 返回单个 thread 的响应。
pub struct ThreadResult {
    pub thread: Thread,
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
    pub model_usage: Option<TurnModelUsage>,
}

/// 模型 usage 的协议线格式（与 `singularity_model::ModelUsage` 同构，
/// 避免 protocol 依赖 model crate）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// 原始 usage 对象是否存在；缺失时各计数保持既有 unknown 表示。
    /// 旧服务端数据无此字段时按存在解释。
    #[serde(default = "default_usage_present_protocol")]
    pub usage_present: bool,
    /// Whether every provider request represented by this aggregate reported
    /// exact usage. Missing/unknown final-request usage remains partial rather
    /// than being represented as zero.
    #[serde(default = "default_usage_complete_protocol")]
    pub usage_complete: bool,
}

fn default_usage_present_protocol() -> bool {
    true
}

fn default_usage_complete_protocol() -> bool {
    true
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

/// 对外广播的应用事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEvent {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// thread 生命周期 notification 的类型化参数。
pub struct ThreadEventParams {
    pub thread: Thread,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// turn 生命周期 notification 的类型化参数。
pub struct TurnEventParams {
    pub turn: Turn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// item notification 中的最小 item 引用。
pub struct ItemReference {
    pub item_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// item notification 的公共可渲染参数。
pub struct ItemEventParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item: ItemReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 工具执行开始事件的类型化公共参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStartParams {
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// 工具调用参数是工具生命周期合同的一部分；CLI 仅将其作为 JSON 值投影，
    /// 不把它重新解释为协议字段。
    pub args: Value,
}

/// 工具执行增量事件的类型化公共参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdateParams {
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub partial_result: String,
}

/// 工具执行终态结果的类型化参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEndParams {
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolExecutionResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionResult {
    pub content: Vec<ToolExecutionContent>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// turn/error 事件的严格类型化参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnErrorParams {
    pub thread_id: String,
    pub turn_id: String,
    pub error: TurnErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnErrorDetail {
    pub stage: String,
    pub cause: String,
    pub message: String,
}

/// 非致命 Agent 诊断事件；不进入 Session JSONL。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDiagnosticParams {
    pub thread_id: String,
    pub turn_id: String,
    pub severity: String,
    pub code: String,
    pub message: String,
}

/// Provider 单次 HTTP attempt 的非敏感进度/终态观测。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAttemptEventParams {
    pub thread_id: String,
    pub turn_id: String,
    pub model_turn_ordinal: u32,
    pub operation_phase: String,
    pub provider: String,
    pub model: String,
    pub protocol: String,
    pub attempt_index: u32,
    pub status: String,
    pub attempt_duration_ms: Option<u64>,
    pub retry_scheduled: Option<bool>,
    pub retry_backoff_ms: Option<u64>,
    pub error_category: Option<String>,
    pub diagnostic_code: Option<String>,
}

/// Provider attempt aggregate，作为终态可靠投影使用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAttemptSummaryParams {
    pub thread_id: String,
    pub turn_id: String,
    pub model_turn_ordinal: u32,
    pub attempt_count: u32,
    pub retry_count: u32,
    pub latency_ms: u64,
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

    /// 构造 Turn 执行错误终态事件。
    pub fn turn_error(
        turn_id: &str,
        thread_id: &str,
        stage: &str,
        cause: &str,
        message: &str,
    ) -> Self {
        Self {
            method: "turn/error".to_string(),
            params: serde_json::json!({
                "turnId": turn_id,
                "threadId": thread_id,
                "error": {
                    "stage": stage,
                    "cause": cause,
                    "message": message,
                },
            }),
        }
    }

    /// 构造非致命、脱敏的 Agent 诊断事件。
    pub fn agent_diagnostic(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        severity: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            method: "agent/diagnostic".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "severity": severity.into(),
                "code": code.into(),
                "message": message.into(),
            }),
        }
    }

    /// 构造单次 Provider attempt 的脱敏进度/终态事件。
    #[allow(clippy::too_many_arguments)]
    pub fn provider_attempt(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        model_turn_ordinal: u32,
        operation_phase: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        protocol: impl Into<String>,
        attempt_index: u32,
        status: impl Into<String>,
        attempt_duration_ms: Option<u64>,
        retry_scheduled: Option<bool>,
        retry_backoff_ms: Option<u64>,
        error_category: Option<String>,
        diagnostic_code: Option<String>,
    ) -> Self {
        Self {
            method: "provider/attempt".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "modelTurnOrdinal": model_turn_ordinal,
                "operationPhase": operation_phase.into(),
                "provider": provider.into(),
                "model": model.into(),
                "protocol": protocol.into(),
                "attemptIndex": attempt_index,
                "status": status.into(),
                "attemptDurationMs": attempt_duration_ms,
                "retryScheduled": retry_scheduled,
                "retryBackoffMs": retry_backoff_ms,
                "errorCategory": error_category,
                "diagnosticCode": diagnostic_code,
            }),
        }
    }

    /// 构造一个 model turn 的 Provider attempt aggregate 终态事件。
    pub fn provider_attempt_summary(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        model_turn_ordinal: u32,
        attempt_count: u32,
        retry_count: u32,
        latency_ms: u64,
    ) -> Self {
        Self {
            method: "provider/attempt/summary".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "modelTurnOrdinal": model_turn_ordinal,
                "attemptCount": attempt_count,
                "retryCount": retry_count,
                "latencyMs": latency_ms,
            }),
        }
    }

    /// 构造 item started 事件。
    pub fn item_started(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
    ) -> Self {
        Self::item_event("item/started", thread_id, turn_id, item_id)
    }

    /// 构造 agent message 增量事件。
    pub fn item_agent_message_delta(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
    ) -> Self {
        Self {
            method: "item/agentMessage/delta".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "item": {"itemId": item_id.into()},
                "delta": delta.into(),
            }),
        }
    }

    /// 构造 item completed 事件。
    pub fn item_completed(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
    ) -> Self {
        Self::item_event("item/completed", thread_id, turn_id, item_id)
    }

    /// 构造 item failed 事件。
    pub fn item_failed(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            method: "item/failed".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "item": {"itemId": item_id.into()},
                "error": error.into(),
            }),
        }
    }

    /// 构造工具开始执行事件通知。
    pub fn tool_execution_start(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: Value,
    ) -> Self {
        Self {
            method: "tool/execution/start".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "toolCallId": tool_call_id.into(),
                "toolName": tool_name.into(),
                "args": args,
            }),
        }
    }

    /// 构造工具执行流式输出增量更新通知。
    pub fn tool_execution_update(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: Value,
        partial_result: impl Into<String>,
    ) -> Self {
        Self {
            method: "tool/execution/update".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "toolCallId": tool_call_id.into(),
                "toolName": tool_name.into(),
                "args": args,
                "partialResult": partial_result.into(),
            }),
        }
    }

    /// 构造工具执行完成事件通知。
    pub fn tool_execution_end(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        result: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let result = result.into();
        Self {
            method: "tool/execution/end".to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "toolCallId": tool_call_id.into(),
                "toolName": tool_name.into(),
                "result": {
                    "content": [{"type": "text", "text": result}],
                    "isError": is_error,
                },
            }),
        }
    }

    fn item_event(
        method: &'static str,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
    ) -> Self {
        Self {
            method: method.to_string(),
            params: serde_json::json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "item": {"itemId": item_id.into()},
            }),
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
