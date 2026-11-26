use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
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

impl fmt::Debug for McpTransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args, cwd, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args_count", &args.len())
                .field("cwd", cwd)
                .field("env_keys", &env.keys().collect::<Vec<_>>())
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &redact_url_for_debug(url))
                .field("header_keys", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

fn redact_url_for_debug(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or("");
    let Some((scheme, rest)) = without_query.split_once("://") else {
        return "[redacted]".to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpServerInput {
    pub server_id: String,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
    pub transport: McpTransportConfig,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpListParams {
    pub workspace_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpSaveParams {
    pub server: McpServerInput,
    pub workspace_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpServerParams {
    pub server_id: String,
    pub workspace_root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionStatus {
    NotConnected,
    Connecting,
    Connected,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpServerState {
    pub server: McpServerInput,
    pub status: McpConnectionStatus,
    pub error: Option<String>,
    pub tools: Vec<McpToolInfo>,
    pub tokens: u64,
}
