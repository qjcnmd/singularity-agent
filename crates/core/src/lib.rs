#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const JSON_RPC_INVALID_REQUEST: i64 = -32600;
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
pub const APP_ERROR_NOT_INITIALIZED: i64 = -32002;
pub const APP_ERROR_ALREADY_INITIALIZED: i64 = -32003;
pub const APP_ERROR_NOT_FOUND: i64 = -32004;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

impl ClientInfo {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    pub fn now_utc() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    pub fn parse(value: &str) -> Result<Self, time::error::Parse> {
        OffsetDateTime::parse(value, &Rfc3339).map(Self)
    }
}

impl Display for Timestamp {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let formatted = self.0.format(&Rfc3339).map_err(|_| std::fmt::Error)?;
        formatter.write_str(&formatted)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn schema_name() -> String {
        "Timestamp".to_string()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        String::json_schema(generator)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorCode {
    pub code: i64,
    pub message: String,
}

impl ErrorCode {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn not_initialized() -> Self {
        Self::new(APP_ERROR_NOT_INITIALIZED, "Not initialized")
    }

    pub fn already_initialized() -> Self {
        Self::new(APP_ERROR_ALREADY_INITIALIZED, "Already initialized")
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(JSON_RPC_INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: impl AsRef<str>) -> Self {
        Self::new(
            JSON_RPC_METHOD_NOT_FOUND,
            format!("Method not found: {}", method.as_ref()),
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(APP_ERROR_NOT_FOUND, message)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}
