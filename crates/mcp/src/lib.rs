#![forbid(unsafe_code)]

//! 用户级 MCP 配置、按工作目录复用的连接，以及设置页查看的最近发现结果。

mod client;
mod config;
mod result;

pub use result::McpToolResult;

use client::Connection;
use config::{Configuration, ServerConfig};
use rmcp::model::Tool;
use singularity_protocol::{McpConnectionStatus, McpServerInput, McpServerState, McpToolInfo};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

type ConnectionCell = Arc<tokio::sync::Mutex<ConnectionState>>;

struct ConnectionState {
    connection: Option<Arc<Connection>>,
    // 设置页读取最近发现结果，不另发发现请求。
    catalog: Result<Vec<Tool>, String>,
}

type Connections = BTreeMap<(String, String), (ServerConfig, ConnectionCell)>;

/// MCP 配置和连接的管理者；设置页和 Agent 共用同一个实例。
pub struct McpManager {
    home: PathBuf,
    connections: Mutex<Connections>,
}

/// 本轮发现的一个 MCP 工具；调用时使用发现时的连接，本轮内不变。
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    original_name: String,
    connection: Arc<Connection>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("McpTool").field("name", &self.name).finish_non_exhaustive()
    }
}

impl McpTool {
    /// 调用服务器原始工具；取消通知与超时由 SDK 处理，调用不会自动重试。
    pub async fn call(
        &self,
        arguments: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<McpToolResult, String> {
        let result = self.connection.call(&self.original_name, arguments, signal).await?;
        Ok(result::convert(result))
    }
}

/// 一次工具发现的结果；连不上的服务器不提供工具，失败原因写进 Developer 指令告诉模型。
#[derive(Default)]
pub struct McpDiscovery {
    pub tools: Vec<McpTool>,
    pub errors: Vec<String>,
    pub instructions: Vec<String>,
}

impl McpManager {
    /// 绑定用户数据目录；每次设置操作和工具发现读取 mcp.json，无配置时目录为空。
    pub fn open(home: PathBuf) -> Self {
        Self {
            connections: Mutex::new(BTreeMap::new()),
            home,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connections> {
        self.connections.lock().expect("MCP connection lock poisoned")
    }

    /// 一次返回配置和已有连接状态，不启动服务器或查询工具。
    pub fn list(&self, cwd: &Path) -> Result<Vec<McpServerState>, String> {
        let config = config::read(&self.home)?;
        Ok(Self::states(config, cwd, &self.lock()))
    }

    /// 检查并原子写入配置，返回配置和状态；正在运行的回合继续用原来的连接和工具。
    pub fn save(&self, input: McpServerInput, cwd: &Path) -> Result<Vec<McpServerState>, String> {
        let id = input.server_id.clone();
        let config = ServerConfig::from_input(input);
        config.validate(&id)?;
        self.update(&id, cwd, |all| {
            all.servers.insert(id.clone(), config);
            Ok(())
        })
    }

    /// 修改服务器开关，不删除配置。
    pub fn toggle(&self, id: &str, enabled: bool, cwd: &Path) -> Result<Vec<McpServerState>, String> {
        self.update(id, cwd, |all| {
            all.servers.get_mut(id).ok_or("MCP 服务器不存在。")?.enabled = enabled;
            Ok(())
        })
    }

    /// 删除配置并释放连接；正在运行的回合仍可使用已发现的工具，直到这一轮结束。
    pub fn remove(&self, id: &str, cwd: &Path) -> Result<Vec<McpServerState>, String> {
        self.update(id, cwd, |all| {
            all.servers.remove(id).ok_or("MCP 服务器不存在。")?;
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        cwd: &Path,
        change: impl FnOnce(&mut Configuration) -> Result<(), String>,
    ) -> Result<Vec<McpServerState>, String> {
        let mut connections = self.lock();
        let mut config = config::read(&self.home)?;
        change(&mut config)?;
        config::write(&self.home, &config)?;
        connections.retain(|(_, server), _| server != id);
        Ok(Self::states(config, cwd, &connections))
    }

    fn states(config: Configuration, cwd: &Path, connections: &Connections) -> Vec<McpServerState> {
        let directory = directory_key(cwd);
        config
            .servers
            .into_iter()
            .map(|(id, config)| Self::server_state(id, config, &directory, connections))
            .collect()
    }

    fn entries(
        &self,
        cwd: &Path,
        only: Option<&str>,
        reconnect: bool,
    ) -> Result<Vec<(String, ServerConfig, ConnectionCell)>, String> {
        let mut connections = self.lock();
        let config = config::read(&self.home)?;
        if only.is_some_and(|id| !config.servers.contains_key(id)) {
            return Err("MCP 服务器不存在。".into());
        }
        // 以 mcp.json 为准：配置已改或已停用的连接从表里移除，正在运行的回合仍持有旧连接直到结束。
        connections.retain(|(_, id), (previous, _)| {
            config.servers.get(id).is_some_and(|current| current.enabled && current == previous)
        });
        let mut entries = Vec::new();
        for (id, config) in config.servers {
            if !config.enabled || only.is_some_and(|only| only != id) {
                continue;
            }
            let key = (directory_key(cwd), id.clone());
            if reconnect {
                connections.remove(&key);
            }
            let (_, cell) = connections.entry(key).or_insert_with(|| {
                (
                    config.clone(),
                    Arc::new(tokio::sync::Mutex::new(ConnectionState {
                        connection: None,
                        catalog: Ok(Vec::new()),
                    })),
                )
            });
            let cell = Arc::clone(cell);
            entries.push((id, config, cell));
        }
        Ok(entries)
    }

    async fn tools(
        cwd: &Path,
        config: ServerConfig,
        cell: &ConnectionCell,
    ) -> Result<(Arc<Connection>, Vec<Tool>), String> {
        // 同一项目的并发准备共用连接；失败的工具调用不会走这里重放。
        let mut state = cell.lock().await;
        let connection = match state.connection.as_ref().filter(|connection| !connection.is_closed()) {
            Some(connection) => Arc::clone(connection),
            None => {
                state.connection = None;
                match Connection::connect(cwd, config).await {
                    Ok(connection) => Arc::new(connection),
                    Err(error) => {
                        state.catalog = Err(error.clone());
                        return Err(error);
                    }
                }
            }
        };
        let tools = connection.tools().await.map(|mut tools| {
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            tools
        });
        state.connection = Some(Arc::clone(&connection));
        state.catalog = tools.clone();
        tools.map(|tools| (connection, tools))
    }

    /// 每轮获取全部已启用工具；复用健康连接，重新读取目录。
    /// BTreeMap 与名称排序保持请求定义顺序稳定。
    pub async fn discover(&self, cwd: &Path, signal: &CancellationToken) -> McpDiscovery {
        if signal.is_cancelled() {
            return McpDiscovery::default();
        }
        let entries = match self.entries(cwd, None, false) {
            Ok(entries) => entries,
            Err(error) => {
                return McpDiscovery {
                    errors: vec![error],
                    ..Default::default()
                };
            }
        };
        let mut pending = tokio::task::JoinSet::new();
        for (id, config, cell) in entries {
            let cwd = cwd.to_path_buf();
            pending.spawn(async move { (id, Self::tools(&cwd, config, &cell).await) });
        }
        let mut discovered = BTreeMap::new();
        while !pending.is_empty() {
            tokio::select! {
                () = signal.cancelled() => break,
                entry = pending.join_next() => {
                    let (id, result) = entry.expect("pending discovery task").expect("MCP discovery worker completes");
                    discovered.insert(id, result);
                }
            }
        }
        let mut result = McpDiscovery::default();
        for (id, found) in discovered {
            match found {
                Ok((connection, tools)) => {
                    if let Some(instructions) = connection.instructions() {
                        result.instructions.push(format!("MCP server {id}:\n{instructions}"));
                    }
                    for tool in tools {
                        result.tools.push(McpTool {
                            name: client::tool_alias(&id, &tool.name),
                            description: format!(
                                "MCP server {id}, tool {}. {}",
                                tool.name,
                                tool.description.as_deref().unwrap_or("")
                            ),
                            original_name: tool.name.into_owned(),
                            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                            connection: Arc::clone(&connection),
                        });
                    }
                }
                Err(error) => result.errors.push(format!("MCP {id}：{error}")),
            }
        }
        result
    }

    /// 用户明确重试时重建该项目的连接并发现工具，连接失败随服务器状态返回。
    pub async fn reconnect(&self, id: &str, cwd: &Path) -> Result<McpServerState, String> {
        if let Some((_, config, cell)) = self.entries(cwd, Some(id), true)?.pop() {
            // 连接失败保存在状态中，与执行准备失败使用同一份原因。
            let _ = Self::tools(cwd, config, &cell).await;
        }
        // 重连等待期间设置可能已修改，返回时读取当前配置和连接。
        let mut config = config::read(&self.home)?;
        let config = config.servers.remove(id).ok_or("MCP 服务器不存在。")?;
        Ok(Self::server_state(id.into(), config, &directory_key(cwd), &self.lock()))
    }

    fn server_state(
        id: String,
        config: ServerConfig,
        directory: &str,
        connections: &Connections,
    ) -> McpServerState {
        let cell = connections
            .get(&(directory.into(), id.clone()))
            .filter(|(previous, _)| previous == &config)
            .map(|(_, cell)| cell);
        let mut inspection = McpServerState {
            server: config.into_input(id),
            status: McpConnectionStatus::NotConnected,
            error: None,
            tools: vec![],
        };
        if !inspection.server.enabled {
            inspection.status = McpConnectionStatus::Disabled;
            return inspection;
        }
        let Some(cell) = cell else { return inspection };
        let Ok(state) = cell.try_lock() else {
            inspection.status = McpConnectionStatus::Connecting;
            return inspection;
        };
        match (&state.catalog, &state.connection) {
            (Err(error), _) => {
                inspection.status = McpConnectionStatus::Failed;
                inspection.error = Some(error.clone());
            }
            (Ok(_), Some(connection)) if connection.is_closed() => {
                inspection.status = McpConnectionStatus::Failed;
                inspection.error = Some("连接已断开，下次执行任务时会重新连接。".into());
            }
            (Ok(tools), Some(_)) => {
                inspection.status = McpConnectionStatus::Connected;
                inspection.tools = tools
                    .iter()
                    .map(|tool| McpToolInfo {
                        name: tool.name.to_string(),
                        description: tool.description.as_deref().unwrap_or_default().into(),
                    })
                    .collect();
            }
            (Ok(_), None) => {}
        }
        inspection
    }

    /// 立即移除项目持有的连接，再等待这些连接退出；重新添加项目会建立新连接。
    pub fn disconnect_directory(&self, cwd: &Path) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut connections = self.lock();
        let key = directory_key(cwd);
        let cells = connections
            .extract_if(.., |(directory, _), _| directory == &key)
            .map(|(_, (_, cell))| cell)
            .collect();
        close_connections(cells)
    }

    /// 所有任务停止后关闭全部连接，等待 SDK 回收本地进程。
    pub async fn shutdown(&self) {
        let cells = std::mem::take(&mut *self.lock()).into_values().map(|(_, cell)| cell).collect();
        close_connections(cells).await;
    }
}

async fn close_connections(cells: Vec<ConnectionCell>) {
    for cell in cells {
        if let Some(connection) = &cell.lock().await.connection {
            connection.close().await;
        }
    }
}

// 原生路径带有 Windows 长路径前缀，界面路径没有；两者按同一目录复用和关闭连接。
fn directory_key(cwd: &Path) -> String {
    singularity_core::display_path(cwd).to_lowercase()
}
