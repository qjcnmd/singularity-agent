use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

const IDENTIFIER_MAX_BYTES: usize = 128;
const SHA1_HEX_LENGTH: usize = 40;
const SHA256_HEX_LENGTH: usize = 64;
const BUILTIN_TOOL_NAMES: [&str; 7] = [
    "builtin.read",
    "builtin.list",
    "builtin.grep",
    "builtin.edit",
    "builtin.patch",
    "builtin.command",
    "builtin.update_plan",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_identifier("task id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for TaskId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for TaskId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(String);

impl RunId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_identifier("run id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RunId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for RunId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_relative_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RelativePath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for RelativePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteRepository(String);

impl RemoteRepository {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_remote_repository(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for RemoteRepository {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RemoteRepository {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitCommit(String);

impl GitCommit {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if !matches!(value.len(), SHA1_HEX_LENGTH | SHA256_HEX_LENGTH)
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(
                "remote git commit must be a full 40- or 64-character hexadecimal object id"
                    .to_string(),
            );
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for GitCommit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GitCommit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolName(String);

impl ToolName {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_tool_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argv(Vec<String>);

impl Argv {
    pub fn new(argv: Vec<String>) -> Result<Self, String> {
        validate_argv(&argv)?;
        Ok(Self(argv))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<String> {
        self.0
    }
}

impl Serialize for Argv {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Argv {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Vec::<String>::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > IDENTIFIER_MAX_BYTES {
        return Err(format!(
            "{kind} must contain between 1 and {IDENTIFIER_MAX_BYTES} ASCII bytes"
        ));
    }
    if !value.is_ascii() {
        return Err(format!("{kind} must be ASCII"));
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(format!(
            "{kind} must start and end with an ASCII letter or digit"
        ));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "{kind} may contain only ASCII letters, digits, '-', '_', and '.'"
        ));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("relative path must not be empty".to_string());
    }
    if value.contains('\0') {
        return Err("relative path must not contain NUL".to_string());
    }
    if value.contains(':') {
        return Err(
            "relative path must not contain ':' or Windows alternate data streams".to_string(),
        );
    }
    if value.contains('\\') {
        return Err("relative path must use forward slashes".to_string());
    }
    if value.starts_with('/') || std::path::Path::new(value).is_absolute() {
        return Err("relative path must not be absolute".to_string());
    }
    if value
        .split('/')
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err("relative path must not contain empty, '.', or '..' segments".to_string());
    }
    Ok(())
}

fn validate_remote_repository(value: &str) -> Result<(), String> {
    if value.chars().any(char::is_whitespace) {
        return Err("remote repository URL must not contain whitespace".to_string());
    }
    let remote = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("ssh://"));
    let Some(remainder) = remote else {
        return Err("remote repository must use an https:// or ssh:// URL".to_string());
    };
    let Some((host, path)) = remainder.split_once('/') else {
        return Err("remote repository URL must include a host and repository path".to_string());
    };
    if host.is_empty() || path.is_empty() || path.starts_with('/') {
        return Err("remote repository URL must include a host and repository path".to_string());
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), String> {
    if BUILTIN_TOOL_NAMES.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "unsupported evaluation tool name {value}; expected one of {}",
            BUILTIN_TOOL_NAMES.join(", ")
        ))
    }
}

fn validate_argv(argv: &[String]) -> Result<(), String> {
    let Some(executable) = argv.first() else {
        return Err("command argv must not be empty".to_string());
    };
    if executable.trim().is_empty() {
        return Err("command argv executable must not be empty".to_string());
    }
    if argv.iter().any(|argument| argument.contains('\0')) {
        return Err("command argv must not contain NUL".to_string());
    }
    if is_shell_string_wrapper(argv) {
        return Err(
            "shell command strings are not allowed; provide direct executable argv".to_string(),
        );
    }
    Ok(())
}

fn is_shell_string_wrapper(argv: &[String]) -> bool {
    let Some(mode) = argv.get(1) else {
        return false;
    };
    let executable = argv[0]
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&argv[0])
        .to_ascii_lowercase();
    let mode = mode.to_ascii_lowercase();
    match executable.as_str() {
        "sh" | "bash" | "dash" | "ksh" | "zsh" => mode == "-c",
        "cmd" | "cmd.exe" => mode == "/c",
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            matches!(mode.as_str(), "-command" | "-c")
        }
        _ => false,
    }
}
