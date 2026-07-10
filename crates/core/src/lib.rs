#![forbid(unsafe_code)]

mod project_instructions;

pub use project_instructions::{
    PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
    PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES, ProjectInstructionError, ProjectInstructionErrorCode,
    ProjectInstructions, load_project_instructions, load_project_instructions_from_cwd,
};

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
const TOKEN_VALUE_MIN_BODY_CHARS: usize = 8;
const SECRET_ASSIGNMENT_MIN_VALUE_CHARS: usize = 1;
const AWS_ACCESS_KEY_ID_BODY_CHARS: usize = 16;
const GOOGLE_API_KEY_BODY_MIN_CHARS: usize = 30;
const GOOGLE_API_KEY_BODY_MAX_CHARS: usize = 45;
const JWT_MIN_PARTS: usize = 3;
const JWT_MIN_PART_CHARS: usize = 8;
const SENSITIVE_TEXT_MARKERS: [&str; 26] = [
    ".aws",
    ".azure",
    ".env",
    ".gnupg",
    ".ssh",
    "api_key",
    "authorization",
    "credential",
    "cookie",
    "evaluator-only",
    "evaluator_only",
    "id_ed25519",
    "id_rsa",
    "password",
    "private-key",
    "\"provider\"",
    "provider:",
    "provider_payload",
    "provider_response",
    "raw_arguments",
    "raw_prompt",
    "raw_response",
    "secret",
    "\"env\"",
    "env:",
    "env=",
];
const SECRET_ASSIGNMENT_MARKERS: [&str; 20] = [
    "token=",
    "token:",
    "access_token=",
    "access_token:",
    "access-token=",
    "access-token:",
    "refresh_token=",
    "refresh_token:",
    "refresh-token=",
    "refresh-token:",
    "api_key=",
    "api_key:",
    "api-key=",
    "api-key:",
    "apikey=",
    "apikey:",
    "x-api-key=",
    "x-api-key:",
    "password=",
    "password:",
];
const SECRET_FLAG_MARKERS: [&str; 6] = [
    "--token",
    "--access-token",
    "--refresh-token",
    "--api-key",
    "--apikey",
    "--password",
];

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

pub fn contains_sensitive_text(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    SENSITIVE_TEXT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
        || contains_secret_like_token(text)
        || contains_bearer_value(&lowered)
        || contains_secret_assignment(&lowered)
        || contains_secret_flag_argument(&lowered)
}

fn contains_secret_like_token(text: &str) -> bool {
    text.split(secret_token_delimiter)
        .any(is_secret_like_token_fragment)
}

fn contains_bearer_value(lowered: &str) -> bool {
    lowered
        .split("bearer")
        .skip(1)
        .filter_map(|suffix| suffix.split_whitespace().next())
        .any(|candidate| candidate.len() >= TOKEN_VALUE_MIN_BODY_CHARS)
}

fn contains_secret_assignment(lowered: &str) -> bool {
    SECRET_ASSIGNMENT_MARKERS.iter().any(|marker| {
        lowered.split(marker).skip(1).any(|suffix| {
            suffix
                .trim_start()
                .split(secret_value_delimiter)
                .next()
                .is_some_and(|value| value.len() >= SECRET_ASSIGNMENT_MIN_VALUE_CHARS)
        })
    })
}

fn contains_secret_flag_argument(lowered: &str) -> bool {
    let parts = lowered.split_whitespace().collect::<Vec<_>>();
    parts.windows(2).any(|window| {
        SECRET_FLAG_MARKERS.contains(&window[0])
            && window[1].len() >= SECRET_ASSIGNMENT_MIN_VALUE_CHARS
    })
}

fn is_secret_like_token_fragment(fragment: &str) -> bool {
    let token = fragment.trim_matches(secret_token_delimiter);
    token
        .strip_prefix("sk-")
        .is_some_and(|body| body.len() >= TOKEN_VALUE_MIN_BODY_CHARS)
        || token
            .strip_prefix("ghp_")
            .or_else(|| token.strip_prefix("gho_"))
            .or_else(|| token.strip_prefix("ghu_"))
            .or_else(|| token.strip_prefix("ghs_"))
            .or_else(|| token.strip_prefix("ghr_"))
            .is_some_and(|body| body.len() >= TOKEN_VALUE_MIN_BODY_CHARS)
        || token
            .strip_prefix("npm_")
            .is_some_and(|body| body.len() >= TOKEN_VALUE_MIN_BODY_CHARS)
        || token
            .strip_prefix("AKIA")
            .is_some_and(|body| body.len() == AWS_ACCESS_KEY_ID_BODY_CHARS)
        || token.strip_prefix("AIza").is_some_and(|body| {
            (GOOGLE_API_KEY_BODY_MIN_CHARS..=GOOGLE_API_KEY_BODY_MAX_CHARS).contains(&body.len())
        })
        || ["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"]
            .iter()
            .any(|prefix| {
                token
                    .strip_prefix(prefix)
                    .is_some_and(|body| body.len() >= TOKEN_VALUE_MIN_BODY_CHARS)
            })
        || is_jwt_like_token(token)
}

fn is_jwt_like_token(token: &str) -> bool {
    let parts = token.split('.').collect::<Vec<_>>();
    parts.len() >= JWT_MIN_PARTS
        && parts
            .iter()
            .take(JWT_MIN_PARTS)
            .all(|part| part.len() >= JWT_MIN_PART_CHARS && part.chars().all(is_base64url_char))
}

fn secret_token_delimiter(ch: char) -> bool {
    !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn secret_value_delimiter(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, ',' | ']' | '}' | '"' | '\'')
}

fn is_base64url_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}
