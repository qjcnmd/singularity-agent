//! MCP 设置与连接检查的桌面合同。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 文件与设置接口共用的 MCP 连接配置；省略的参数、环境变量和请求头按空值处理。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpServerInput {
    pub server_id: String,
    pub enabled: bool,
    pub startup_timeout_sec: u64,
    pub tool_timeout_sec: u64,
    pub transport: McpTransportConfig,
}

/// 按项目目录读取配置和已有连接状态。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpListParams {
    pub workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpSaveParams {
    pub server: McpServerInput,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpServerParams {
    pub server_id: String,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpToggleParams {
    pub server_id: String,
    pub enabled: bool,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// 区分未启动、正在连接与失败；查看状态不会启动服务器。
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionStatus {
    NotConnected,
    Connecting,
    Connected,
    Failed,
    Disabled,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
/// 一台服务器的配置、当前连接状态和最近发现的工具。
pub struct McpServerState {
    pub server: McpServerInput,
    pub status: McpConnectionStatus,
    pub error: Option<String>,
    pub tools: Vec<McpToolInfo>,
}
