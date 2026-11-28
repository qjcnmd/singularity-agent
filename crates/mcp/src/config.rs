use crate::McpError;
use protocol::{McpServerInput, McpTransportConfig};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, path::Path};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    #[serde(rename = "mcpServers", default)]
    pub(crate) servers: BTreeMap<String, ServerConfig>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerConfig {
    #[serde(default = "startup_timeout")]
    pub(crate) startup_timeout_sec: u64,
    #[serde(default = "tool_timeout")]
    pub(crate) tool_timeout_sec: u64,
    #[serde(flatten)]
    pub(crate) transport: McpTransportConfig,
}

fn startup_timeout() -> u64 {
    30
}
fn tool_timeout() -> u64 {
    120
}

impl ServerConfig {
    pub(crate) fn from_input(input: McpServerInput) -> Self {
        Self {
            startup_timeout_sec: input.startup_timeout_sec,
            tool_timeout_sec: input.tool_timeout_sec,
            transport: input.transport,
        }
    }

    pub(crate) fn into_input(self, server_id: String) -> McpServerInput {
        McpServerInput {
            server_id,
            startup_timeout_sec: self.startup_timeout_sec,
            tool_timeout_sec: self.tool_timeout_sec,
            transport: self.transport,
        }
    }

    pub(crate) fn validate(&self, id: &str) -> Result<(), McpError> {
        if id.is_empty()
            || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(McpError::new("MCP 名称只能包含英文字母、数字、下划线和连字符。"));
        }
        if self.startup_timeout_sec == 0 || self.tool_timeout_sec == 0 {
            return Err(McpError::new("MCP 超时必须大于零秒。"));
        }
        match &self.transport {
            McpTransportConfig::Stdio { command, args, cwd, env } => {
                if command.trim().is_empty()
                    || command.contains('\0')
                    || args.iter().any(|arg| arg.contains('\0'))
                {
                    return Err(McpError::new("MCP 启动命令或参数无效。"));
                }
                if cwd.as_ref().is_some_and(|cwd| cwd.trim().is_empty() || cwd.contains('\0')) {
                    return Err(McpError::new("MCP 工作目录无效。"));
                }
                if env
                    .iter()
                    .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
                {
                    return Err(McpError::new("MCP 环境变量无效。"));
                }
            }
            McpTransportConfig::Http { url, headers } => {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|error| McpError::new(format!("MCP 服务器 {id} 地址无效：{error}")))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err(McpError::new(format!(
                        "MCP 服务器 {id} 地址必须是有效的 HTTP 或 HTTPS URL。"
                    )));
                }
                let mut seen_headers = std::collections::HashSet::with_capacity(headers.len());
                for (key, value) in headers {
                    let name = reqwest::header::HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
                        McpError::new(format!("MCP 服务器 {id} 请求头名称无效（{key}）：{error}"))
                    })?;
                    if !seen_headers.insert(name) {
                        return Err(McpError::new(format!("MCP 服务器 {id} 请求头名称重复（{key}）。")));
                    }
                    reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                        McpError::new(format!("MCP 服务器 {id} 请求头值无效（{key}）：{error}"))
                    })?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn redact(&self, error: String) -> String {
        let mut values: Vec<Cow<'_, str>> = Vec::new();
        match &self.transport {
            McpTransportConfig::Stdio { args, env, .. } => {
                for (name, value) in env {
                    if is_secret_name(name) {
                        collect_secret_value(value, &mut values);
                    }
                }
                collect_arg_secrets(args, &mut values);
            }
            McpTransportConfig::Http { url, headers } => {
                if let Ok(parsed) = reqwest::Url::parse(url) {
                    collect_parsed_url_secrets(&parsed, &mut values);
                } else {
                    values.push(Cow::Borrowed(url.as_str()));
                }
                for (name, value) in headers {
                    if is_secret_name(name) {
                        collect_secret_value(value, &mut values);
                    }
                }
            }
        }
        replace_non_overlapping(error, values)
    }
}

fn collect_secret_value<'a>(value: &'a str, values: &mut Vec<Cow<'a, str>>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    values.push(Cow::Borrowed(trimmed));
    if let Some((scheme, rest)) = trimmed.split_once(' ')
        && ["bearer", "basic", "token"].iter().any(|s| scheme.eq_ignore_ascii_case(s))
        && !rest.trim().is_empty()
    {
        values.push(Cow::Borrowed(rest.trim()));
    }
}

fn collect_parsed_url_secrets(parsed: &reqwest::Url, values: &mut Vec<Cow<'_, str>>) {
    if !parsed.username().is_empty() {
        values.push(Cow::Owned(parsed.username().to_owned()));
    }
    if let Some(password) = parsed.password().filter(|value| !value.is_empty()) {
        values.push(Cow::Owned(password.to_owned()));
    }
    if let Some(query) = parsed.query().filter(|value| !value.is_empty()) {
        values.push(Cow::Owned(query.to_owned()));
    }
    for (key, value) in parsed.query_pairs() {
        if is_secret_name(&key) && !value.is_empty() {
            values.push(Cow::Owned(value.into_owned()));
        }
    }
}

fn collect_arg_secrets<'a>(args: &'a [String], values: &mut Vec<Cow<'a, str>>) {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if let Some(parsed) = parse_url_candidate(arg) {
            collect_parsed_url_secrets(&parsed, values);
            index += 1;
            continue;
        }
        let url_candidate = if let Some((key, value)) = arg.split_once('=') {
            let name = key.trim_start_matches('-');
            if !name.is_empty() && is_secret_name(name) {
                collect_secret_value(value, values);
            }
            Some(value)
        } else if arg.starts_with('-') && is_secret_name(arg.trim_start_matches('-')) {
            if let Some(next) = args.get(index + 1).filter(|v| !v.is_empty() && !v.starts_with('-')) {
                collect_secret_value(next.as_str(), values);
                index += 1;
                Some(next.as_str())
            } else {
                None
            }
        } else {
            None
        };
        if let Some(parsed) = url_candidate.and_then(parse_url_candidate) {
            collect_parsed_url_secrets(&parsed, values);
        }
        index += 1;
    }
}

fn parse_url_candidate(candidate: &str) -> Option<reqwest::Url> {
    candidate.contains("://").then(|| reqwest::Url::parse(candidate).ok()).flatten()
}

fn replace_non_overlapping(error: String, mut values: Vec<Cow<'_, str>>) -> String {
    values.retain(|value| value.len() >= 2);
    if values.is_empty() {
        return error;
    }
    values.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    values.dedup();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for value in &values {
        let needle = value.as_ref();
        for (start, _) in error.match_indices(needle) {
            let end = start + needle.len();
            if !spans
                .iter()
                .any(|&(existing_start, existing_end)| start < existing_end && end > existing_start)
            {
                spans.push((start, end));
            }
        }
    }
    if spans.is_empty() {
        return error;
    }
    spans.sort_unstable_by_key(|&(start, _)| start);
    let mut redacted = String::with_capacity(error.len());
    let mut cursor = 0;
    for (start, end) in spans {
        redacted.push_str(&error[cursor..start]);
        redacted.push_str("[redacted]");
        cursor = end;
    }
    redacted.push_str(&error[cursor..]);
    redacted
}

const SECRET_SUBSTRINGS: &[&[u8]] = &[
    b"token",
    b"secret",
    b"password",
    b"passwd",
    b"credential",
    b"cookie",
    b"bearer",
    b"authorization",
    b"apikey",
];

const SECRET_WORDS: &[&[u8]] = &[b"auth", b"key", b"keys", b"pass", b"pat", b"jwt"];

fn is_secret_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if SECRET_SUBSTRINGS
        .iter()
        .any(|needle| bytes.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle)))
    {
        return true;
    }
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            let split_camel = bytes[i].is_ascii_uppercase()
                && (bytes[i - 1].is_ascii_lowercase()
                    || bytes[i - 1].is_ascii_digit()
                    || (bytes[i - 1].is_ascii_uppercase()
                        && bytes.get(i + 1).is_some_and(u8::is_ascii_lowercase)));
            if split_camel {
                break;
            }
            i += 1;
        }
        let word = &bytes[start..i];
        if SECRET_WORDS.iter().any(|secret| word.eq_ignore_ascii_case(secret)) {
            return true;
        }
    }
    false
}

pub(crate) fn read(home: &Path) -> Result<Configuration, McpError> {
    let path = home.join("mcp.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Configuration::default());
        }
        Err(source) => {
            return Err(McpError::io(&format!("无法读取 {}", path.display()), &source));
        }
    };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    let config: Configuration = serde_json::from_slice(bytes).map_err(|error| {
        McpError::new(format!("mcp.json 格式无效（第 {} 行，第 {} 列）。", error.line(), error.column()))
    })?;
    for (id, server) in &config.servers {
        server.validate(id)?;
    }
    Ok(config)
}

pub(crate) fn write(home: &Path, config: &Configuration) -> Result<(), McpError> {
    app_core::write_json_atomic(&home.join("mcp.json"), config)
        .map_err(|source| McpError::io("无法保存 mcp.json", &source))
}
