//! `mcp.json` 的读写、配置校验和敏感信息脱敏。

use serde::{Deserialize, Serialize};
use singularity_protocol::{McpServerInput, McpTransportConfig};
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
            McpTransportConfig::Stdio { command, args, cwd, env } => {
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
            McpTransportConfig::Http { url, headers } => {
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

    /// 网络错误、服务器 stderr 和服务端报错可能带有密钥，返回或打印前把配置中的敏感值替换掉。
    pub fn redact(&self, mut error: String) -> String {
        let mut values: Vec<&str> = match &self.transport {
            McpTransportConfig::Stdio { env, .. } => env
                .iter()
                .filter(|(name, _)| is_secret_name(name))
                .map(|(_, value)| value.as_str())
                .collect(),
            McpTransportConfig::Http { url, headers } => {
                let mut values = vec![url.as_str()];
                values.extend(
                    headers.iter().filter(|(name, _)| is_secret_name(name)).map(|(_, value)| value.as_str()),
                );
                for (name, value) in headers {
                    // 服务端错误可能只回显认证值，没有 Bearer / Basic 前缀。
                    if name.eq_ignore_ascii_case("authorization")
                        && let Some((_, credential)) = value.split_once(' ')
                    {
                        values.push(credential.trim());
                    }
                }
                values
            }
        };
        // 先替换完整长值：短值先匹配会留下另一个凭据的后缀。
        values.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
        for value in values {
            if !value.is_empty() {
                error = error.replace(value, "[redacted]");
            }
        }
        error
    }
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    name == "authorization"
        || name == "proxy_authorization"
        || name == "cookie"
        || name
            .split('_')
            .any(|part| matches!(part, "key" | "token" | "secret" | "password" | "credential"))
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
    singularity_core::write_json_atomic(&home.join("mcp.json"), config)
        .map_err(|error| format!("无法保存 mcp.json：{error}"))
}
