#![forbid(unsafe_code)]

//! 跨 crate 共享的 JSON-RPC 基础类型、敏感信息检测和 workspace 规则。

mod cancellation;
mod project_instructions;

pub use cancellation::CancellationToken;
pub use project_instructions::{
    PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
    PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES, PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME,
    ProjectInstructionError, ProjectInstructionErrorCode, ProjectInstructionSource,
    ProjectInstructions, load_project_instructions, load_project_instructions_from_cwd,
};

use std::fmt::{Display, Formatter};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

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
const TOKEN_VALUE_MIN_BODY_CHARS: usize = 8;
const SECRET_ASSIGNMENT_MIN_VALUE_CHARS: usize = 1;
const AWS_ACCESS_KEY_ID_BODY_CHARS: usize = 16;
const GOOGLE_API_KEY_BODY_MIN_CHARS: usize = 30;
const GOOGLE_API_KEY_BODY_MAX_CHARS: usize = 45;
const JWT_MIN_PARTS: usize = 3;
const JWT_MIN_PART_CHARS: usize = 8;
/// Protected metadata directories whose names are reserved by the workspace/runtime contract.
///
/// Keep this list in `core` so tools, sandbox policy resolution, and the Windows enforcement
/// adapter cannot silently diverge. Windows may materialize a missing `.git` sentinel only when
/// no ancestor repository marker exists; nested markers are rejected by the adapter so Git uses
/// its existing ancestor discovery semantics.
pub const PROTECTED_GIT_DIR_NAME: &str = ".git";
pub const PROTECTED_AGENTS_DIR_NAME: &str = ".agents";
pub const PROTECTED_METADATA_DIR_NAME: &str = ".singularity";
pub const PROTECTED_METADATA_PATH_NAMES: [&str; 3] = [
    PROTECTED_GIT_DIR_NAME,
    PROTECTED_AGENTS_DIR_NAME,
    PROTECTED_METADATA_DIR_NAME,
];

/// Protected path components that must be denied wherever they occur in a workspace.
pub const PROTECTED_PATH_EXACT_MARKERS: [&str; 12] = [
    ".aws",
    ".azure",
    ".gnupg",
    ".ssh",
    "credentials",
    "credentials.json",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
    "secret",
    "secrets",
];
/// Protected component prefixes; only the prefix itself or a dotted variant matches.
pub const PROTECTED_PATH_PREFIXES: [&str; 3] = [".env", "credential", "private-key"];
/// Protected component suffixes that can carry private keys or credential containers.
///
/// PEM also carries public certificates, but a filename alone cannot prove that the content is
/// public. Keep the default fail-closed; any future certificate exception must validate content
/// at the trust boundary instead of globally allowing the ambiguous suffix.
pub const PROTECTED_PATH_SUFFIXES: [&str; 4] = [".key", ".pem", ".p12", ".pfx"];
/// Substrings that identify protected components.
pub const PROTECTED_PATH_CONTAINS_MARKERS: [&str; 1] = ["secret"];
const SENSITIVE_TEXT_MARKERS: [&str; 27] = [
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
    "private key-----",
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

/// 连接 AppServer 的客户端身份信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// JSON-RPC 请求或持久化对象使用的字符串 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// 返回 ID 字符串视图。
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
/// RFC 3339 时间戳。
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// 创建当前 UTC 时间戳。
    pub fn now_utc() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    /// 解析 RFC 3339 时间戳。
    pub fn parse(value: &str) -> Result<Self, time::error::Parse> {
        OffsetDateTime::parse(value, &Rfc3339).map(Self)
    }

    /// Construct an RFC 3339 timestamp from non-negative Unix milliseconds.
    pub fn from_unix_ms(value: u64) -> Option<Self> {
        let nanos = i128::from(value).checked_mul(1_000_000)?;
        OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .ok()
            .map(Self)
    }

    /// Return a non-negative Unix timestamp in milliseconds, saturating at `u64::MAX`.
    pub fn unix_ms(self) -> u64 {
        let nanos = self.0.unix_timestamp_nanos();
        if nanos <= 0 {
            return 0;
        }
        u64::try_from(nanos / 1_000_000).unwrap_or(u64::MAX)
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

/// JSON-RPC 错误码和脱敏错误消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

    /// 构造方法不存在错误。
    pub fn method_not_found(method: impl AsRef<str>) -> Self {
        Self::new(
            JSON_RPC_METHOD_NOT_FOUND,
            format!("Method not found: {}", method.as_ref()),
        )
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

/// 判断文本是否包含密钥、provider payload 或其他敏感信息标记。
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

/// 判断工作区相对路径是否命中统一的 protected path 规则。
pub fn is_protected_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .map(str::to_ascii_lowercase)
        .any(|component| {
            PROTECTED_METADATA_PATH_NAMES.contains(&component.as_str())
                || PROTECTED_PATH_EXACT_MARKERS.contains(&component.as_str())
                || PROTECTED_PATH_PREFIXES.iter().any(|prefix| {
                    component == *prefix
                        || component
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('.'))
                })
                || PROTECTED_PATH_SUFFIXES
                    .iter()
                    .any(|suffix| component.ends_with(suffix))
                || PROTECTED_PATH_CONTAINS_MARKERS
                    .iter()
                    .any(|marker| component.contains(marker))
        })
}

/// 判断 protected `.pem` 路径是否只有文件后缀本身需要内容分类。
///
/// 任何受保护祖先，或同时命中 `.env`、credential、private-key、secret 等规则的文件名
/// 仍保持拒绝；只有普通名称的 `.pem` 叶对象可以继续接受证书内容验证。
pub fn is_public_certificate_pem_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let components = normalized.split('/').collect::<Vec<_>>();
    let Some((leaf, parents)) = components.split_last() else {
        return false;
    };
    if leaf.is_empty()
        || parents.iter().any(|component| {
            component.is_empty() || matches!(*component, "." | "..") || is_protected_path(component)
        })
    {
        return false;
    }
    let Some((stem, extension)) = leaf.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && extension.eq_ignore_ascii_case("pem") && !is_protected_path(stem)
}

fn public_certificate_label<'a>(line: &'a str, boundary: &str) -> Option<&'a str> {
    line.strip_prefix(boundary)
        .and_then(|line| line.strip_suffix("-----"))
        .filter(|label| matches!(*label, "CERTIFICATE" | "TRUSTED CERTIFICATE"))
}

fn is_public_certificate_metadata_comment(line: &str) -> bool {
    [
        "# Issuer:",
        "# Subject:",
        "# Label:",
        "# Serial:",
        "# MD5 Fingerprint:",
        "# SHA1 Fingerprint:",
        "# SHA256 Fingerprint:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// 判断 PEM 内容是否只包含可解析的公开 X.509 证书。
///
/// 调用方负责通过已固定、无跟随的文件句柄读取 `bytes`，并用平台的 X.509 解析器提供
/// `is_x509_certificate_der`；未知标签、混合私钥、格式错误或无法解析的 DER 均拒绝。
pub fn is_public_certificate_only_pem(
    bytes: &[u8],
    mut is_x509_certificate_der: impl FnMut(&[u8]) -> bool,
) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut open_label = None;
    let mut encoded_block = String::new();
    let mut certificate_count = 0usize;
    for line in text.lines().map(str::trim) {
        match open_label {
            None => {
                if line.is_empty() || is_public_certificate_metadata_comment(line) {
                    continue;
                }
                let Some(label) = public_certificate_label(line, "-----BEGIN ") else {
                    return false;
                };
                open_label = Some(label);
                encoded_block.clear();
            }
            Some(expected_label) => {
                if let Some(label) = public_certificate_label(line, "-----END ") {
                    let decoded = BASE64_STANDARD.decode(encoded_block.as_bytes());
                    if label != expected_label
                        || encoded_block.is_empty()
                        || !decoded.as_deref().is_ok_and(&mut is_x509_certificate_der)
                    {
                        return false;
                    }
                    certificate_count = certificate_count.saturating_add(1);
                    open_label = None;
                    continue;
                }
                if line.is_empty()
                    || line.starts_with("-----")
                    || !line.is_ascii()
                    || !line.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
                    })
                {
                    return false;
                }
                encoded_block.push_str(line);
            }
        }
    }
    open_label.is_none() && certificate_count > 0
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
        && parts[0].starts_with("eyJ")
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

#[cfg(test)]
mod tests {
    use super::{is_protected_path, is_public_certificate_pem_path};

    #[test]
    fn protected_path_policy_covers_exact_prefix_and_suffix_markers() {
        for path in [
            ".git/config",
            ".agents/runtime.json",
            ".singularity/state.json",
            "nested/.env.local",
            "nested/private-key.pem",
            "nested/server.pem",
            "nested/tls.pem",
            "nested/backup.p12",
            "nested/client-secret.txt",
        ] {
            assert!(is_protected_path(path), "{path} should be protected");
        }
        for path in ["src/main.rs", "config/example.env.sample", "notes/key.txt"] {
            assert!(!is_protected_path(path), "{path} should be allowed");
        }
    }

    #[test]
    fn public_certificate_path_requires_an_unprotected_pem_leaf() {
        for path in ["cacert.pem", "nested/cacert.PEM"] {
            assert!(
                is_public_certificate_pem_path(path),
                "{path} should be eligible for content validation"
            );
        }
        for path in [
            ".env.pem",
            "private-key.pem",
            "secret.pem",
            "nested/.env/cacert.pem",
            "../cacert.pem",
            r"nested\secret\cacert.pem",
        ] {
            assert!(
                !is_public_certificate_pem_path(path),
                "{path} should remain protected"
            );
        }
    }
}
