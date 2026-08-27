//! JSON-RPC 消息信封：帧结构、解析、错误码与公共对象。

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::method::Method;

/// JSON-RPC 2.0 parse error code。
pub const JSON_RPC_PARSE_ERROR: i64 = -32700;
/// JSON-RPC 请求结构无效。
pub const JSON_RPC_INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 方法不存在。
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 参数无效。
pub const JSON_RPC_INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 内部错误。
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
/// AppServer 尚未初始化。
pub const APP_ERROR_NOT_INITIALIZED: i64 = -32002;
/// AppServer 已经初始化。
pub const APP_ERROR_ALREADY_INITIALIZED: i64 = -32003;
/// 请求的持久化对象不存在。
pub const APP_ERROR_NOT_FOUND: i64 = -32004;

/// 连接 AppServer 的客户端身份信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

impl ClientInfo {
    /// 创建客户端身份信息。
    pub fn new(
        name: impl Into<String>,
        title: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            version: version.into(),
        }
    }
}

/// JSON-RPC 错误码和错误消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCode {
    pub code: i64,
    pub message: String,
}

impl ErrorCode {
    /// 创建 JSON-RPC 错误码。
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 构造未初始化错误。
    pub fn not_initialized() -> Self {
        Self::new(APP_ERROR_NOT_INITIALIZED, "Not initialized")
    }

    /// 构造重复初始化错误。
    pub fn already_initialized() -> Self {
        Self::new(APP_ERROR_ALREADY_INITIALIZED, "Already initialized")
    }

    /// 构造无效请求错误。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(JSON_RPC_INVALID_REQUEST, message)
    }

    /// 构造无效参数错误。
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(JSON_RPC_INVALID_PARAMS, message)
    }

    /// 构造资源不存在错误。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(APP_ERROR_NOT_FOUND, message)
    }

    /// 返回错误消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// JSON-RPC id：单一数字（i64）。请求、响应与错误响应只接受数字 id；
/// 无法关联的响应（parse error 等）以 `id: null` 表示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcId(pub i64);

impl From<i64> for JsonRpcId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<i32> for JsonRpcId {
    fn from(value: i32) -> Self {
        Self(i64::from(value))
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
/// JSON-RPC error response；无法关联到请求 id 时以 `null` 表示。
pub struct JsonRpcErrorResponse {
    jsonrpc: JsonRpcVersion,
    pub id: Option<JsonRpcId>,
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

    /// 构造 error response；无法关联的请求 id 以标准 null 表示。
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

    /// 返回 request 或 response 的 typed id；error response 无法关联时为
    /// `None`（wire 上为 null）。
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
