#![forbid(unsafe_code)]

//! 用户级 MCP 配置、按工作目录复用的连接，以及每回合冻结的工具目录。

mod client;
mod config;
mod result;

pub use result::McpToolResult;

use client::Connection;
use config::{Configuration, ServerConfig};
use rmcp::model::Tool;
use singularity_protocol::{McpInspection, McpServerInput, McpToolInfo};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

type ConnectionCell = Arc<OnceCell<Result<Arc<Connection>, String>>>;

type Connections = BTreeMap<(PathBuf, String), (ServerConfig, ConnectionCell)>;

/// 进程内的 MCP 所有者；设置和 Agent 共享同一实例，不持有模型或会话状态。
pub struct McpManager {
    home: PathBuf,
    connections: Mutex<Connections>,
}

/// 已发现的 MCP 工具；名字与调用目标来自同一份冻结目录。
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
        out.debug_struct("McpTool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    /// 调用服务器原始工具；取消通知与超时由 SDK 处理，调用不会自动重试。
    pub async fn call(
        &self,
        arguments: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<McpToolResult, String> {
        let result = self
            .connection
            .call(&self.original_name, arguments, signal)
            .await?;
        Ok(result::convert(result))
    }
}

/// 工具发现保留每个失败原因，由宿主向用户报告；不可用服务器不广告工具。
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
        self.connections
            .lock()
            .expect("MCP connection lock poisoned")
    }

    /// 返回设置编辑所需的配置；只走私有桌面 RPC，不进入会话与模型请求。
    pub fn list(&self) -> Result<Vec<McpServerInput>, String> {
        let _connections = self.lock();
        Ok(config::read(&self.home)?
            .servers
            .iter()
            .map(|(id, config)| config.input(id.clone()))
            .collect())
    }

    /// 先验证并原子落盘，再发布新配置；已有回合继续持有原来的连接与目录。
    pub fn save(&self, input: McpServerInput) -> Result<(), String> {
        let id = input.server_id.clone();
        let config = ServerConfig::from_input(input);
        config.validate(&id)?;
        self.update(&id, |all| {
            all.servers.insert(id.clone(), config);
            Ok(())
        })
    }

    /// 修改服务器开关，不删除配置。
    pub fn toggle(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.update(id, |all| {
            all.servers.get_mut(id).ok_or("MCP 服务器不存在。")?.enabled = enabled;
            Ok(())
        })
    }

    /// 删除设置并释放管理器持有的连接，运行中的回合通过自己的快照继续收尾。
    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.update(id, |all| {
            all.servers.remove(id).ok_or("MCP 服务器不存在。")?;
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut Configuration) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut connections = self.lock();
        let mut config = config::read(&self.home)?;
        change(&mut config)?;
        config::write(&self.home, &config)?;
        connections.retain(|(_, server), _| server != id);
        Ok(())
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
        // 磁盘是配置事实来源；已更改或停用的连接只从后续操作中移除，回合快照仍可收尾。
        connections.retain(|(_, id), (previous, _)| {
            config
                .servers
                .get(id)
                .is_some_and(|current| current.enabled && current == previous)
        });
        let mut entries = Vec::new();
        for (id, config) in config.servers {
            if !config.enabled || only.is_some_and(|only| only != id) {
                continue;
            }
            let key = (cwd.to_path_buf(), id.clone());
            if reconnect {
                connections.remove(&key);
            }
            let (_, cell) = connections
                .entry(key)
                .or_insert_with(|| (config.clone(), Arc::new(OnceCell::new())));
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
        let connection = cell
            .get_or_init(|| async { Connection::connect(cwd, config).await.map(Arc::new) })
            .await
            .clone()?;
        let tools = connection.tools().await?;
        Ok((connection, tools))
    }

    /// 在首次模型请求前发现全部已启用工具；BTreeMap 与名称排序保持请求定义顺序稳定。
    pub async fn discover(&self, cwd: &Path, signal: &CancellationToken) -> McpDiscovery {
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
                Ok((connection, mut tools)) => {
                    if let Some(instructions) = connection.instructions() {
                        result
                            .instructions
                            .push(format!("MCP server {id}:\n{instructions}"));
                    }
                    tools.sort_by(|a, b| a.name.cmp(&b.name));
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

    /// 设置中的连接测试使用同一连接和发现路径；重连显式丢弃该项目中的旧连接。
    pub async fn inspect(
        &self,
        id: &str,
        cwd: &Path,
        reconnect: bool,
    ) -> Result<McpInspection, String> {
        let mut entries = self.entries(cwd, Some(id), reconnect)?;
        let Some((_, config, cell)) = entries.pop() else {
            return Ok(McpInspection {
                server_id: id.into(),
                connected: false,
                error: Some("服务器已关闭。".into()),
                tools: vec![],
            });
        };
        Ok(match Self::tools(cwd, config, &cell).await {
            Ok((_, mut tools)) => {
                tools.sort_by(|a, b| a.name.cmp(&b.name));
                McpInspection {
                    server_id: id.into(),
                    connected: true,
                    error: None,
                    tools: tools
                        .into_iter()
                        .map(|tool| McpToolInfo {
                            name: tool.name.into_owned(),
                            description: tool.description.unwrap_or_default().into_owned(),
                        })
                        .collect(),
                }
            }
            Err(error) => McpInspection {
                server_id: id.into(),
                connected: false,
                error: Some(error),
                tools: vec![],
            },
        })
    }

    /// 任务结算后关闭所有传输，等待 SDK 回收本地进程。
    pub async fn shutdown(&self) {
        let cells = std::mem::take(&mut *self.lock());
        for (_, cell) in cells.into_values() {
            if let Some(Ok(connection)) = cell.get() {
                connection.close().await;
            }
        }
    }
}
