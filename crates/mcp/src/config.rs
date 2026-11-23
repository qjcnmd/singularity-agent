use crate::McpError;
use protocol::{McpServerInput, McpTransportConfig};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerConfig {
    #[serde(default = "startup_timeout")]
    pub startup_timeout_sec: u64,
    #[serde(default = "tool_timeout")]
    pub tool_timeout_sec: u64,
    #[serde(flatten)]
    pub transport: McpTransportConfig,
}

fn startup_timeout() -> u64 {
    30
}
fn tool_timeout() -> u64 {
    120
}

impl ServerConfig {
    pub fn from_input(input: McpServerInput) -> Self {
        Self {
            startup_timeout_sec: input.startup_timeout_sec,
            tool_timeout_sec: input.tool_timeout_sec,
            transport: input.transport,
        }
    }

    pub fn into_input(self, server_id: String) -> McpServerInput {
        McpServerInput {
            server_id,
            startup_timeout_sec: self.startup_timeout_sec,
            tool_timeout_sec: self.tool_timeout_sec,
            transport: self.transport,
        }
    }

    pub fn validate(&self, id: &str) -> Result<(), McpError> {
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
                let parsed = reqwest::Url::parse(url).map_err(|error| {
                    McpError::new(self.redact(format!("MCP 服务器 {id} 地址无效：{error}")))
                })?;
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

    pub fn redact(&self, error: String) -> String {
        use std::borrow::Cow;

        let values: Vec<Cow<'_, str>> = match &self.transport {
            McpTransportConfig::Stdio { env, .. } => env
                .iter()
                .filter(|(name, _)| is_secret_name(name))
                .map(|(_, value)| Cow::Borrowed(value.as_str()))
                .collect(),
            McpTransportConfig::Http { url, headers } => {
                let mut values: Vec<Cow<'_, str>> = Vec::new();
                if let Ok(parsed) = reqwest::Url::parse(url) {
                    if !parsed.username().is_empty() {
                        values.push(Cow::Owned(parsed.username().to_owned()));
                        if let Some(decoded) = percent_decode(parsed.username()) {
                            values.push(Cow::Owned(decoded));
                        }
                    }
                    if let Some(password) = parsed.password().filter(|value| !value.is_empty()) {
                        values.push(Cow::Owned(password.to_owned()));
                        if let Some(decoded) = percent_decode(password) {
                            values.push(Cow::Owned(decoded));
                        }
                    }
                    if let Some(query) = parsed.query() {
                        values.push(Cow::Owned(query.to_owned()));
                        for pair in query.split('&') {
                            if let Some((key, value)) = pair.split_once('=')
                                && is_secret_name(key)
                                && !value.is_empty()
                            {
                                values.push(Cow::Owned(value.to_owned()));
                            }
                        }
                    }
                    for (key, value) in parsed.query_pairs() {
                        if is_secret_name(&key) && !value.is_empty() {
                            values.push(Cow::Owned(value.into_owned()));
                        }
                    }
                } else {
                    values.push(Cow::Borrowed(url.as_str()));
                }
                values.extend(
                    headers
                        .iter()
                        .filter(|(name, _)| is_secret_name(name))
                        .map(|(_, value)| Cow::Borrowed(value.as_str())),
                );
                for (name, value) in headers {
                    if (name.eq_ignore_ascii_case("authorization")
                        || name.eq_ignore_ascii_case("proxy-authorization"))
                        && let Some((_, credential)) = value.split_once(' ')
                    {
                        values.push(Cow::Borrowed(credential.trim()));
                    }
                }
                values
            }
        };
        replace_non_overlapping(error, values)
    }
}

fn replace_non_overlapping(error: String, mut values: Vec<std::borrow::Cow<'_, str>>) -> String {
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

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_nibble(bytes[index + 1]), hex_nibble(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
            changed = true;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    changed.then(|| String::from_utf8_lossy(&decoded).into_owned())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_secret_name(name: &str) -> bool {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = name.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(ch) = chars.next() {
        if matches!(ch, '_' | '-') {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev = None;
            continue;
        }
        let next = chars.peek().copied();
        let split_camel = ch.is_ascii_uppercase()
            && (prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || (prev.as_ref().is_some_and(char::is_ascii_uppercase)
                    && next.as_ref().is_some_and(char::is_ascii_lowercase)));
        if split_camel && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch.to_ascii_lowercase());
        prev = Some(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.iter().any(|word| {
        matches!(
            word.as_str(),
            "authorization"
                | "cookie"
                | "key"
                | "keys"
                | "apikey"
                | "apikeys"
                | "token"
                | "tokens"
                | "accesstoken"
                | "authtoken"
                | "apitoken"
                | "secret"
                | "secrets"
                | "clientsecret"
                | "password"
                | "passwords"
                | "passwd"
                | "credential"
                | "credentials"
        )
    })
}

pub(crate) fn read(home: &Path) -> Result<Configuration, McpError> {
    let path = home.join("mcp.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Configuration::default());
        }
        Err(source) => {
            return Err(McpError::io(&format!("无法读取 {}", path.display()), source));
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
        .map_err(|source| McpError::io("无法保存 mcp.json", source))
}
