//! 私有 mcp.json 的持久化模型与设置边界转换。

use serde::{Deserialize, Serialize};
use singularity_protocol::{McpServerInput, McpTransportInput};
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
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default = "startup_timeout")]
    pub startup_timeout_sec: u64,
    #[serde(default = "tool_timeout")]
    pub tool_timeout_sec: u64,
    #[serde(flatten)]
    pub transport: TransportConfig,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

fn enabled() -> bool {
    true
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
            enabled: input.enabled,
            startup_timeout_sec: input.startup_timeout_sec,
            tool_timeout_sec: input.tool_timeout_sec,
            transport: match input.transport {
                McpTransportInput::Stdio { command, args, cwd, env } => {
                    TransportConfig::Stdio { command, args, cwd, env }
                }
                McpTransportInput::Http { url, headers } => TransportConfig::Http { url, headers },
            },
        }
    }

    pub fn input(&self, server_id: String) -> McpServerInput {
        McpServerInput {
            server_id,
            enabled: self.enabled,
            startup_timeout_sec: self.startup_timeout_sec,
            tool_timeout_sec: self.tool_timeout_sec,
            transport: match &self.transport {
                TransportConfig::Stdio { command, args, cwd, env } => McpTransportInput::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    cwd: cwd.clone(),
                    env: env.clone(),
                },
                TransportConfig::Http { url, headers } => McpTransportInput::Http {
                    url: url.clone(),
                    headers: headers.clone(),
                },
            },
        }
    }

    pub fn validate(&self, id: &str) -> Result<(), String> {
        if id.is_empty()
            || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("MCP 名称只能包含英文字母、数字、下划线和连字符。".into());
        }
        if self.startup_timeout_sec == 0 || self.tool_timeout_sec == 0 {
            return Err("MCP 超时必须大于零秒。".into());
        }
        match &self.transport {
            TransportConfig::Stdio { command, args, cwd, env } => {
                if command.trim().is_empty()
                    || command.contains('\0')
                    || args.iter().any(|arg| arg.contains('\0'))
                {
                    return Err("MCP 启动命令或参数无效。".into());
                }
                if cwd.as_ref().is_some_and(|cwd| cwd.trim().is_empty() || cwd.contains('\0')) {
                    return Err("MCP 工作目录无效。".into());
                }
                if env
                    .iter()
                    .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
                {
                    return Err("MCP 环境变量无效。".into());
                }
            }
            TransportConfig::Http { url, headers } => {
                let parsed = url::Url::parse(url).map_err(|_| "MCP 地址必须是有效的 HTTP 或 HTTPS URL。")?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err("MCP 地址必须是有效的 HTTP 或 HTTPS URL。".into());
                }
                for (key, value) in headers {
                    reqwest::header::HeaderName::from_bytes(key.as_bytes())
                        .map_err(|_| "MCP 请求头名称无效。")?;
                    reqwest::header::HeaderValue::from_str(value).map_err(|_| "MCP 请求头值无效。")?;
                }
            }
        }
        Ok(())
    }

    /// 网络异常、服务器 stderr 和服务端错误可能含令牌，发布前去掉配置中的敏感值。
    pub fn redact(&self, mut error: String) -> String {
        let mut values: Vec<&str> = match &self.transport {
            TransportConfig::Stdio { env, .. } => env.values().map(String::as_str).collect(),
            TransportConfig::Http { url, headers } => {
                std::iter::once(url.as_str())
                    .chain(headers.values().map(String::as_str))
                    .chain(headers.iter().filter_map(|(name, value)| {
                        // 服务端错误可能只回显认证值，没有 Bearer / Basic 前缀。
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.split_once(' ').map(|(_, value)| value.trim()))
                            .flatten()
                    }))
                    .collect()
            }
        };
        // 先替换完整长值，避免短值覆盖后留下另一个凭据的后缀。
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        for value in values {
            if !value.is_empty() {
                error = error.replace(value, "[redacted]");
            }
        }
        error
    }
}

pub(crate) fn read(home: &Path) -> Result<Configuration, String> {
    let path = home.join("mcp.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Configuration::default());
        }
        Err(error) => return Err(format!("无法读取 {}：{error}", path.display())),
    };
    let config: Configuration = serde_json::from_slice(&bytes).map_err(|error| {
        format!("mcp.json 格式无效（第 {} 行，第 {} 列）。", error.line(), error.column())
    })?;
    for (id, server) in &config.servers {
        server.validate(id)?;
    }
    Ok(config)
}

pub(crate) fn write(home: &Path, config: &Configuration) -> Result<(), String> {
    singularity_core::create_data_dir(home)?;
    let mut bytes = serde_json::to_vec_pretty(config).expect("MCP configuration is JSON serializable");
    bytes.push(b'\n');
    singularity_core::atomic_replace_bytes(&home.join("mcp.json"), &bytes)
        .map_err(|error| format!("无法保存 mcp.json：{error}"))
}
