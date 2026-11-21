//! 用户级 MCP 配置、按工作目录复用的连接，以及设置页查看的最近发现结果。

mod client;
mod config;
mod error;
mod result;

pub use error::McpError;
pub use result::McpToolResult;

use client::Connection;
use config::{Configuration, ServerConfig};
use protocol::{McpConnectionStatus, McpServerInput, McpServerState, McpToolInfo};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

type ConnectionCell = Arc<tokio::sync::Mutex<ConnectionState>>;
type ConnectionGuard = tokio::sync::OwnedMutexGuard<ConnectionState>;

struct ConnectionState {
    connection: Option<Arc<Connection>>,
    // 设置页只需最近发现的名称和说明，完整工具定义交给本轮执行。
    catalog: Result<Vec<McpToolInfo>, String>,
    // 最近一次发现后，这台服务器的说明或失败提示在请求里的估算 Token 数。
    tokens: u64,
}

type Connections = BTreeMap<(String, String), (ServerConfig, ConnectionCell)>;

#[derive(Default)]
struct ConnectionPool {
    generation: u64,
    entries: Connections,
}

impl ConnectionPool {
    /// 以 mcp.json 为准：移除配置已改或已删除的连接，返回待异步关闭的旧槽位。
    fn evict_stale(&mut self, config: &Configuration) -> Vec<ConnectionCell> {
        let mut evicted = Vec::new();
        self.entries.retain(|(_, id), (previous, cell)| {
            if config.servers.get(id) == Some(previous) {
                true
            } else {
                evicted.push(Arc::clone(cell));
                false
            }
        });
        evicted
    }
}

struct DiscoverEntry {
    id: String,
    config: ServerConfig,
    cell: ConnectionCell,
}

struct PreparedDiscover {
    active: Vec<DiscoverEntry>,
    evicted: Vec<ConnectionCell>,
}

struct ReconnectEntry {
    id: String,
    config: ServerConfig,
    cell: ConnectionCell,
    guard: ConnectionGuard,
}

struct PreparedReconnect {
    entry: ReconnectEntry,
    evicted: Vec<ConnectionCell>,
}

/// MCP 配置和连接的管理者；设置页和 Agent 共用同一个实例。
pub struct McpManager {
    home: PathBuf,
    config_lock: Arc<Mutex<()>>,
    connections: Arc<Mutex<ConnectionPool>>,
    closing: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager").field("home", &self.home).finish_non_exhaustive()
    }
}

/// 本轮发现的一个 MCP 工具；调用时使用发现时的连接，本轮内不变。
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    connection: Arc<Connection>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters_schema", &self.parameters_schema)
            .finish_non_exhaustive()
    }
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
        let has_dynamic_or_composed = ["anyOf", "oneOf", "allOf", "patternProperties", "$ref"]
            .iter()
            .any(|key| object.contains_key(*key))
            || matches!(
                object.get("additionalProperties"),
                Some(value) if *value != serde_json::Value::Bool(false)
            );
        required_empty && props_empty && !has_dynamic_or_composed
    }

    /// 调用服务器原始工具；连接负责总超时和有界取消通知，调用不会自动重试。
    pub async fn call(
        &self,
        arguments: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<McpToolResult, McpError> {
        let result = self.connection.call(&self.name, arguments, signal).await?;
        Ok(result::convert(result))
    }
}

/// 一台服务器本轮的发现结果；连不上时没有工具，失败原因在 `error`。
#[derive(Debug)]
pub struct McpServerDiscovery {
    pub id: String,
    pub tools: Vec<McpTool>,
    /// 要写进 Developer 指令的内容：服务器自带的说明，或连接失败的原因。
    pub prompt: Option<String>,
    /// 这台服务器的 `prompt`（服务器说明或失败提示）在请求里的估算 Token 数。
    pub tokens: u64,
    pub error: Option<String>,
}

/// 一次工具发现的结果；`error` 是 mcp.json 读不出来的原因，此时没有服务器。
#[derive(Debug, Default)]
pub struct McpDiscovery {
    pub servers: Vec<McpServerDiscovery>,
    pub error: Option<String>,
}

impl McpManager {
    /// 绑定用户数据目录；每次设置操作和工具发现读取 mcp.json，无配置时目录为空。
    pub fn open(home: PathBuf) -> Self {
        Self {
            config_lock: Arc::new(Mutex::new(())),
            connections: Arc::new(Mutex::new(ConnectionPool::default())),
            closing: Mutex::new(Vec::new()),
            home,
        }
    }

    fn lock_pool(&self) -> std::sync::MutexGuard<'_, ConnectionPool> {
        self.connections.lock().expect("MCP connection lock poisoned")
    }

    async fn read_snapshot(&self) -> Result<(u64, Configuration), McpError> {
        let home = self.home.clone();
        let config_lock = Arc::clone(&self.config_lock);
        let connections = Arc::clone(&self.connections);
        tokio::task::spawn_blocking(move || {
            let _config_guard = config_lock.lock().expect("MCP config lock poisoned");
            let config = config::read(&home)?;
            let generation = connections.lock().expect("MCP connection lock poisoned").generation;
            Ok((generation, config))
        })
        .await
        .expect("MCP config read completes while the runtime is running")
    }

    async fn read_server_state(&self, id: String, cwd: PathBuf) -> Result<McpServerState, McpError> {
        let home = self.home.clone();
        let config_lock = Arc::clone(&self.config_lock);
        let connections = Arc::clone(&self.connections);
        tokio::task::spawn_blocking(move || {
            let _config_guard = config_lock.lock().expect("MCP config lock poisoned");
            let mut config = config::read(&home)?;
            let config = config.servers.remove(&id).ok_or(McpError::NotFound)?;
            let pool = connections.lock().expect("MCP connection lock poisoned");
            Ok(Self::server_state(id, config, &directory_key(&cwd), &pool.entries))
        })
        .await
        .expect("MCP config read completes while the runtime is running")
    }

    fn spawn_close(&self, cells: Vec<ConnectionCell>) {
        if cells.is_empty() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = handle.spawn(close_connections(cells));
        let mut closing = self.closing.lock().expect("MCP closing lock poisoned");
        closing.retain(|existing| !existing.is_finished());
        closing.push(task);
    }

    /// 新建任务页预览用：读取已有连接留下的状态，不启动服务器或查询工具。
    ///
    /// # Blocking
    /// 同步获取配置锁并读取磁盘上的 `mcp.json`，异步上下文应在阻塞线程池中调用。
    pub fn list(&self, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        let _config_guard = self.config_lock.lock().expect("MCP config lock poisoned");
        let config = config::read(&self.home)?;
        Ok(Self::states(config, cwd, &self.lock_pool().entries))
    }

    /// 检查并原子写入配置，返回配置和状态；正在运行的回合继续用原来的连接和工具。
    ///
    /// # Blocking
    /// 同步获取配置锁并执行磁盘读写与刷盘，异步上下文应在阻塞线程池中调用。
    pub fn save(&self, input: McpServerInput, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        let id = input.server_id.clone();
        let config = ServerConfig::from_input(input);
        config.validate(&id)?;
        self.update(&id, cwd, |all| {
            all.servers.insert(id.clone(), config);
            Ok(())
        })
    }

    /// 删除配置并释放连接；正在运行的回合仍可使用已发现的工具，直到这一轮结束。
    ///
    /// # Blocking
    /// 同步获取配置锁并执行磁盘读写与刷盘，异步上下文应在阻塞线程池中调用。
    pub fn remove(&self, id: &str, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        self.update(id, cwd, |all| {
            all.servers.remove(id).ok_or(McpError::NotFound)?;
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        cwd: &Path,
        change: impl FnOnce(&mut Configuration) -> Result<(), McpError>,
    ) -> Result<Vec<McpServerState>, McpError> {
        let (evicted, states) = {
            // 仅串行化 mcp.json 的读写与替换，避免在盘 IO 期间阻塞异步线程上的连接表读取。
            let _config_guard = self.config_lock.lock().expect("MCP config lock poisoned");
            let mut config = config::read(&self.home)?;
            change(&mut config)?;
            config::write(&self.home, &config)?;
            let mut pool = self.lock_pool();
            pool.generation = pool.generation.wrapping_add(1);
            let mut evicted = Vec::new();
            pool.entries.retain(|(_, server), (_, cell)| {
                if server == id {
                    evicted.push(Arc::clone(cell));
                    false
                } else {
                    true
                }
            });
            let states = Self::states(config, cwd, &pool.entries);
            (evicted, states)
        };
        self.spawn_close(evicted);
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

    fn prepare_discover(
        &self,
        snapshot_generation: u64,
        config: Configuration,
        cwd: &Path,
        skip: &[String],
    ) -> Option<PreparedDiscover> {
        let mut pool = self.lock_pool();
        if pool.generation != snapshot_generation {
            return None;
        }
        let evicted = pool.evict_stale(&config);
        let mut active = Vec::new();
        let directory = directory_key(cwd);
        for (id, config) in config.servers {
            if skip.contains(&id) {
                continue;
            }
            let key = (directory.clone(), id.clone());
            let (_, cell) = pool.entries.entry(key).or_insert_with(|| {
                (
                    config.clone(),
                    Arc::new(tokio::sync::Mutex::new(ConnectionState {
                        connection: None,
                        catalog: Ok(Vec::new()),
                        tokens: 0,
                    })),
                )
            });
            active.push(DiscoverEntry { id, config, cell: Arc::clone(cell) });
        }
        Some(PreparedDiscover { active, evicted })
    }

    fn prepare_reconnect(
        &self,
        snapshot_generation: u64,
        config: &Configuration,
        cwd: &Path,
        id: &str,
    ) -> Result<Option<PreparedReconnect>, McpError> {
        let mut pool = self.lock_pool();
        if pool.generation != snapshot_generation {
            return Ok(None);
        }
        let Some(server_config) = config.servers.get(id).cloned() else {
            return Err(McpError::NotFound);
        };
        let mut evicted = pool.evict_stale(config);
        let key = (directory_key(cwd), id.to_owned());
        if let Some((_, old_cell)) = pool.entries.remove(&key) {
            evicted.push(old_cell);
        }
        let cell = Arc::new(tokio::sync::Mutex::new(ConnectionState {
            connection: None,
            catalog: Ok(Vec::new()),
            tokens: 0,
        }));
        // 先占住新 cell 再放进连接表：关闭旧连接期间状态显示 Connecting，并发 discover 不会抢先启动。
        let guard = Arc::clone(&cell).try_lock_owned().expect("fresh MCP connection cell is unlocked");
        pool.entries.insert(key, (server_config.clone(), Arc::clone(&cell)));
        Ok(Some(PreparedReconnect {
            entry: ReconnectEntry {
                id: id.to_owned(),
                config: server_config,
                cell,
                guard,
            },
            evicted,
        }))
    }

    async fn tools(
        id: String,
        cwd: &Path,
        config: ServerConfig,
        mut state: ConnectionGuard,
    ) -> McpServerDiscovery {
        let connection = if let Some(connection) =
            state.connection.as_ref().filter(|connection| !connection.is_closed())
        {
            Ok(Arc::clone(connection))
        } else {
            state.connection = None;
            Connection::connect(cwd, config).await.map(Arc::new)
        };
        let discovered = match connection {
            Ok(connection) => match connection.tools().await {
                Ok(mut listed) => {
                    state.connection = Some(Arc::clone(&connection));
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
                            description: tool.description.map(String::from).unwrap_or_default(),
                            parameters_schema: serde_json::Value::Object(Arc::unwrap_or_clone(
                                tool.input_schema,
                            )),
                            connection: Arc::clone(&connection),
                        })
                        .collect();
                    let prompt = connection.instructions().map(|text| format!("MCP server {id}:\n{text}"));
                    // 仅估算服务器自身说明文本；统一延迟加载的工具摘要由 agent 模块按实际注入口径计算。
                    let tokens = prompt.as_deref().map_or(0, app_core::estimate_tokens_of);
                    state.tokens = tokens;
                    Ok(McpServerDiscovery {
                        id: id.clone(),
                        tools,
                        prompt,
                        tokens,
                        error: None,
                    })
                }
                Err(error) => {
                    state.connection = None;
                    if let Some(connection) = Arc::into_inner(connection) {
                        connection.close().await;
                    }
                    Err(error)
                }
            },
            Err(error) => Err(error),
        };
        discovered.unwrap_or_else(|error| {
            let error = error.to_string();
            tracing::warn!(server_id = %id, error = %error, "MCP server discovery failed");
            let prompt = format!("Unavailable MCP server {id}:\n{error}");
            let tokens = app_core::estimate_tokens_of(&prompt);
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
        let PreparedDiscover { active: entries, evicted } = loop {
            if signal.is_cancelled() {
                return McpDiscovery::default();
            }
            let (generation, config) = match self.read_snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return McpDiscovery {
                        servers: Vec::new(),
                        error: Some(error.to_string()),
                    };
                }
            };
            if signal.is_cancelled() {
                return McpDiscovery::default();
            }
            if let Some(prepared) = self.prepare_discover(generation, config, cwd, skip) {
                break prepared;
            }
        };
        self.spawn_close(evicted);
        let mut pending = tokio::task::JoinSet::new();
        for entry in entries {
            let cwd = cwd.to_path_buf();
            pending.spawn(async move {
                // 同一项目的并发准备共用连接；失败的工具调用不会走这里重放。
                let state = entry.cell.lock_owned().await;
                Self::tools(entry.id, &cwd, entry.config, state).await
            });
        }
        let mut discovered = BTreeMap::new();
        while !pending.is_empty() {
            tokio::select! {
                biased;
                () = signal.cancelled() => {
                    pending.shutdown().await;
                    return McpDiscovery::default();
                }
                entry = pending.join_next() => match entry {
                    Some(Ok(server)) => {
                        discovered.insert(server.id.clone(), server);
                    }
                    Some(Err(error)) => {
                        tracing::warn!(error = %error, "MCP discovery worker failed");
                    }
                    None => break,
                },
            }
        }
        McpDiscovery {
            servers: discovered.into_values().collect(),
            error: None,
        }
    }

    /// 用户明确重试时重建该项目的连接并发现工具，连接失败随服务器状态返回。
    pub async fn reconnect(&self, id: &str, cwd: &Path) -> Result<McpServerState, McpError> {
        let PreparedReconnect { entry, evicted } = loop {
            let (generation, config) = self.read_snapshot().await?;
            if let Some(prepared) = self.prepare_reconnect(generation, &config, cwd, id)? {
                break prepared;
            }
        };
        if !evicted.is_empty() {
            close_connections(evicted).await;
        }
        let still_current = self
            .lock_pool()
            .entries
            .get(&(directory_key(cwd), entry.id.clone()))
            .is_some_and(|(_, current)| Arc::ptr_eq(current, &entry.cell));
        if still_current {
            // 连接失败保存在状态中，与执行准备失败使用同一份原因。
            let _ = Self::tools(entry.id, cwd, entry.config, entry.guard).await;
        }
        // 重连等待期间设置可能已修改，返回时读取当前配置和连接。
        self.read_server_state(id.to_owned(), cwd.to_path_buf()).await
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
                inspection.tools.clone_from(tools);
            }
            (Ok(_), None) => {}
        }
        inspection
    }

    /// 立即移除项目持有的连接，再等待这些连接退出；重新添加项目会建立新连接。
    pub fn disconnect_directory(&self, cwd: &Path) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut pool = self.lock_pool();
        let key = directory_key(cwd);
        let cells = pool
            .entries
            .extract_if(.., |(directory, _), _| directory == &key)
            .map(|(_, (_, cell))| cell)
            .collect();
        close_connections(cells)
    }

    /// 所有任务停止后关闭全部连接，等待 SDK 回收本地进程。
    pub async fn shutdown(&self) {
        let cells = std::mem::take(&mut self.lock_pool().entries)
            .into_values()
            .map(|(_, cell)| cell)
            .collect();
        let pending = std::mem::take(&mut *self.closing.lock().expect("MCP closing lock poisoned"));
        let drain = async move {
            for handle in pending {
                if let Err(error) = handle.await {
                    tracing::warn!(error = %error, "MCP detached close task failed");
                }
            }
        };
        tokio::join!(close_connections(cells), drain);
        loop {
            let remaining = std::mem::take(&mut *self.closing.lock().expect("MCP closing lock poisoned"));
            if remaining.is_empty() {
                break;
            }
            for handle in remaining {
                if let Err(error) = handle.await {
                    tracing::warn!(error = %error, "MCP detached close task failed");
                }
            }
        }
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
    app_core::workspace_key(cwd)
}
