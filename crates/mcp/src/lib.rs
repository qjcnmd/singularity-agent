//! 用户级 MCP 配置、按工作目录复用的连接，以及设置页查看的最近发现结果。

mod client;
mod config;
mod result;

pub use result::McpToolResult;

use client::Connection;
use config::{Configuration, ServerConfig};
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
    // 设置页只需最近发现的名称和说明，完整工具定义交给本轮执行。
    catalog: Result<Vec<McpToolInfo>, String>,
    // 新建任务页显示的估算：这台服务器的工具定义和说明在请求里占多少 Token。
    tokens: u64,
}

type Connections = BTreeMap<(String, String), (ServerConfig, ConnectionCell)>;

struct PreparedEntries {
    active: Vec<(String, ServerConfig, ConnectionCell)>,
    evicted: Vec<ConnectionCell>,
}

/// MCP 配置和连接的管理者；设置页和 Agent 共用同一个实例。
pub struct McpManager {
    home: PathBuf,
    config_lock: Mutex<()>,
    connections: Mutex<Connections>,
}

/// 本轮发现的一个 MCP 工具；调用时使用发现时的连接，本轮内不变。
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    connection: Arc<Connection>,
}

impl McpTool {
    /// 判断该工具是否无需传入任何参数，供延迟加载摘要标注 `(no args)`。
    pub fn has_no_args(&self) -> bool {
        let Some(object) = self.parameters_schema.as_object() else {
            return false;
        };
        let required_empty =
            object.get("required").and_then(serde_json::Value::as_array).is_none_or(Vec::is_empty);
        let props_empty = object
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .is_none_or(serde_json::Map::is_empty);
        required_empty && props_empty
    }

    /// 调用服务器原始工具；连接负责总超时和有界取消通知，调用不会自动重试。
    pub async fn call(
        &self,
        arguments: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<McpToolResult, String> {
        let result = self.connection.call(&self.name, arguments, signal).await?;
        Ok(result::convert(result))
    }
}

/// 一台服务器本轮的发现结果；连不上时没有工具，失败原因在 `error`。
pub struct McpServerDiscovery {
    pub id: String,
    pub tools: Vec<McpTool>,
    /// 要写进 Developer 指令的内容：服务器自带的说明，或连接失败的原因。
    pub prompt: Option<String>,
    /// 这台服务器的工具定义和 `prompt` 在请求里的估算 Token 数。
    pub tokens: u64,
    pub error: Option<String>,
}

/// 一次工具发现的结果；`error` 是 mcp.json 读不出来的原因，此时没有服务器。
#[derive(Default)]
pub struct McpDiscovery {
    pub servers: Vec<McpServerDiscovery>,
    pub error: Option<String>,
}

impl McpManager {
    /// 绑定用户数据目录；每次设置操作和工具发现读取 mcp.json，无配置时目录为空。
    pub fn open(home: PathBuf) -> Self {
        Self {
            config_lock: Mutex::new(()),
            connections: Mutex::new(BTreeMap::new()),
            home,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connections> {
        self.connections.lock().expect("MCP connection lock poisoned")
    }

    /// 新建任务页预览用：读取已有连接留下的状态，不启动服务器或查询工具。
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
        let (evicted, states) = {
            // 仅串行化 mcp.json 的读写与替换，避免在盘 IO 期间阻塞异步线程上的连接表读取。
            let _config_guard = self.config_lock.lock().expect("MCP config lock poisoned");
            let mut config = config::read(&self.home)?;
            change(&mut config)?;
            config::write(&self.home, &config)?;
            let mut connections = self.lock();
            let mut evicted = Vec::new();
            connections.retain(|(_, server), (_, cell)| {
                if server == id {
                    evicted.push(Arc::clone(cell));
                    false
                } else {
                    true
                }
            });
            let states = Self::states(config, cwd, &connections);
            (evicted, states)
        };
        if !evicted.is_empty()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(close_connections(evicted));
        }
        Ok(states)
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
        config: Configuration,
        cwd: &Path,
        only: Option<&str>,
        skip: &[String],
        reconnect: bool,
    ) -> Result<PreparedEntries, String> {
        let mut connections = self.lock();
        if only.is_some_and(|id| !config.servers.contains_key(id)) {
            return Err("MCP 服务器不存在。".into());
        }
        // 以 mcp.json 为准：配置已改的连接从表里移除，由调用方异步关闭旧连接。
        let mut evicted = Vec::new();
        connections.retain(|(_, id), (previous, cell)| {
            if config.servers.get(id) == Some(previous) {
                true
            } else {
                evicted.push(Arc::clone(cell));
                false
            }
        });
        let mut entries = Vec::new();
        let directory = directory_key(cwd);
        for (id, config) in config.servers {
            if only.is_some_and(|only| only != id) || skip.contains(&id) {
                continue;
            }
            let key = (directory.clone(), id.clone());
            if reconnect && let Some((_, cell)) = connections.remove(&key) {
                evicted.push(cell);
            }
            let (_, cell) = connections.entry(key).or_insert_with(|| {
                (
                    config.clone(),
                    Arc::new(tokio::sync::Mutex::new(ConnectionState {
                        connection: None,
                        catalog: Ok(Vec::new()),
                        tokens: 0,
                    })),
                )
            });
            let cell = Arc::clone(cell);
            entries.push((id, config, cell));
        }
        Ok(PreparedEntries { active: entries, evicted })
    }

    async fn tools(
        id: String,
        cwd: &Path,
        config: ServerConfig,
        cell: &ConnectionCell,
    ) -> McpServerDiscovery {
        // 同一项目的并发准备共用连接；失败的工具调用不会走这里重放。
        let mut state = cell.lock().await;
        let connection = match state.connection.as_ref().filter(|connection| !connection.is_closed()) {
            Some(connection) => Ok(Arc::clone(connection)),
            None => {
                state.connection = None;
                Connection::connect(cwd, config).await.map(Arc::new)
            }
        };
        let discovered = match connection {
            Ok(connection) => {
                let listed = connection.tools().await;
                state.connection = Some(Arc::clone(&connection));
                listed.map(|mut listed| {
                    listed.sort_by(|a, b| a.name.cmp(&b.name));
                    state.catalog = Ok(listed
                        .iter()
                        .map(|tool| McpToolInfo {
                            name: tool.name.to_string(),
                            description: tool.description.as_deref().unwrap_or_default().into(),
                        })
                        .collect());
                    let tools: Vec<_> = listed
                        .into_iter()
                        .map(|tool| McpTool {
                            name: tool.name.into_owned(),
                            description: tool.description.as_deref().unwrap_or("").to_string(),
                            parameters_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                            connection: Arc::clone(&connection),
                        })
                        .collect();
                    let prompt = connection.instructions().map(|text| format!("MCP server {id}:\n{text}"));
                    // 仅估算服务器自身说明文本；统一延迟加载的工具摘要由 agent 模块按实际注入口径计算。
                    let tokens = prompt.as_deref().map_or(0, singularity_core::estimate_tokens_of);
                    state.tokens = tokens;
                    McpServerDiscovery {
                        id: id.clone(),
                        tools,
                        prompt,
                        tokens,
                        error: None,
                    }
                })
            }
            Err(error) => Err(error),
        };
        discovered.unwrap_or_else(|error| {
            tracing::warn!(server_id = %id, error = %error, "MCP server discovery failed");
            let prompt = format!("Unavailable MCP server {id}:\n{error}");
            let tokens = singularity_core::estimate_tokens_of(&prompt);
            state.tokens = tokens;
            state.catalog = Err(error.clone());
            McpServerDiscovery {
                id,
                tools: Vec::new(),
                prompt: Some(prompt),
                tokens,
                error: Some(error),
            }
        })
    }

    /// 每轮获取全部服务器的工具（`skip` 里的服务器不连接）；复用健康连接，重新读取目录。
    /// BTreeMap 与名称排序保持请求定义顺序稳定。
    pub async fn discover(&self, cwd: &Path, signal: &CancellationToken, skip: &[String]) -> McpDiscovery {
        if signal.is_cancelled() {
            return McpDiscovery::default();
        }
        let home = self.home.clone();
        let config = match tokio::task::spawn_blocking(move || config::read(&home))
            .await
            .expect("MCP config read completes while the runtime is running")
        {
            Ok(config) => config,
            Err(error) => return McpDiscovery { servers: Vec::new(), error: Some(error) },
        };
        let PreparedEntries { active: entries, evicted } = match self.entries(config, cwd, None, skip, false)
        {
            Ok(prepared) => prepared,
            Err(error) => return McpDiscovery { servers: Vec::new(), error: Some(error) },
        };
        if !evicted.is_empty() {
            tokio::spawn(close_connections(evicted));
        }
        let mut pending = tokio::task::JoinSet::new();
        for (id, config, cell) in entries {
            let cwd = cwd.to_path_buf();
            pending.spawn(async move { Self::tools(id, &cwd, config, &cell).await });
        }
        let mut discovered = BTreeMap::new();
        while !pending.is_empty() {
            tokio::select! {
                () = signal.cancelled() => {
                    pending.shutdown().await;
                    break;
                }
                entry = pending.join_next() => {
                    let server = entry.expect("pending discovery task").expect("MCP discovery worker completes");
                    discovered.insert(server.id.clone(), server);
                }
            }
        }
        McpDiscovery {
            servers: discovered.into_values().collect(),
            error: None,
        }
    }

    /// 用户明确重试时重建该项目的连接并发现工具，连接失败随服务器状态返回。
    pub async fn reconnect(&self, id: &str, cwd: &Path) -> Result<McpServerState, String> {
        let home = self.home.clone();
        let config = tokio::task::spawn_blocking(move || config::read(&home))
            .await
            .expect("MCP config read completes while the runtime is running")?;
        let PreparedEntries { active: mut entries, evicted } =
            self.entries(config, cwd, Some(id), &[], true)?;
        if !evicted.is_empty() {
            close_connections(evicted).await;
        }
        if let Some((id, config, cell)) = entries.pop() {
            // 连接失败保存在状态中，与执行准备失败使用同一份原因。
            let _ = Self::tools(id, cwd, config, &cell).await;
        }
        // 重连等待期间设置可能已修改，返回时读取当前配置和连接。
        let home = self.home.clone();
        let mut config = tokio::task::spawn_blocking(move || config::read(&home))
            .await
            .expect("MCP config read completes while the runtime is running")?;
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
            tokens: 0,
        };
        let Some(cell) = cell else { return inspection };
        let Ok(state) = cell.try_lock() else {
            inspection.status = McpConnectionStatus::Connecting;
            return inspection;
        };
        inspection.tokens = state.tokens;
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
                inspection.tools = tools.clone();
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
    let mut closing = tokio::task::JoinSet::new();
    for cell in cells {
        closing.spawn(async move {
            let connection = cell.lock().await.connection.take();
            // 仅当没有活跃回合持有 McpTool 时主动异步关闭；否则交由最后一个工具释放时通过 Drop 清理。
            if let Some(connection) = connection.and_then(Arc::into_inner) {
                connection.close().await;
            }
        });
    }
    while closing.join_next().await.is_some() {}
}

// 原生路径带有 Windows 长路径前缀，界面路径没有；两者按同一目录复用和关闭连接。
fn directory_key(cwd: &Path) -> String {
    singularity_core::workspace_key(cwd)
}
