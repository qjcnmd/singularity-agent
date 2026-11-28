use crate::McpError;
use protocol::{McpServerInput, McpTransportConfig};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

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

    pub(crate) fn redact(&self, mut error: String) -> String {
        const SECRET_KEYS: &[&str] = &["token", "secret", "key", "pass", "auth", "cookie", "credential"];
        let mut needles: Vec<String> = Vec::new();
        match &self.transport {
            McpTransportConfig::Http { url, headers } => {
                if let Ok(parsed) = reqwest::Url::parse(url) {
                    if !parsed.username().is_empty() {
                        needles.push(parsed.username().to_owned());
                    }
                    if let Some(password) = parsed.password().filter(|v| !v.is_empty()) {
                        needles.push(password.to_owned());
                    }
                    if let Some(query) = parsed.query().filter(|v| !v.is_empty()) {
                        needles.push(query.to_owned());
                    }
                } else if !url.is_empty() {
                    needles.push(url.clone());
                }
                for value in headers.values() {
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        needles.push(trimmed.to_owned());
                    }
                }
            }
            McpTransportConfig::Stdio { env, .. } => {
                for (name, value) in env {
                    let lower = name.to_ascii_lowercase();
                    let trimmed = value.trim();
                    if !trimmed.is_empty() && SECRET_KEYS.iter().any(|s| lower.contains(s)) {
                        needles.push(trimmed.to_owned());
                    }
                }
            }
        }
        needles.retain(|needle| needle.len() >= 2);
        needles.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup();
        for needle in &needles {
            error = error.replace(needle, "[redacted]");
        }
        error
    }
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
    let bytes = app_core::strip_utf8_bom_bytes(&bytes);
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
