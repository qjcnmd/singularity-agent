mod client;
mod config;
mod result;

pub use result::{McpDiscovery, McpError, McpServerDiscovery, McpTool, McpToolResult};

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
    catalog: Result<Vec<McpToolInfo>, String>,
}

impl ConnectionState {
    fn new_cell() -> ConnectionCell {
        Arc::new(tokio::sync::Mutex::new(Self {
            connection: None,
            catalog: Ok(Vec::new()),
        }))
    }
}

type Connections = BTreeMap<(String, String), (ServerConfig, ConnectionCell)>;

fn evict_stale(
    entries: &mut Connections,
    config: &Configuration,
    force_id: Option<&str>,
) -> Vec<ConnectionCell> {
    entries
        .extract_if(.., |(_, id), (previous, _)| {
            force_id == Some(id.as_str()) || config.servers.get(id) != Some(previous)
        })
        .map(|(_, (_, cell))| cell)
        .collect()
}

pub struct McpManager {
    home: PathBuf,
    config_lock: Arc<Mutex<()>>,
    connections: Arc<Mutex<Connections>>,
    closing: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager").field("home", &self.home).finish_non_exhaustive()
    }
}

impl McpManager {
    pub fn open(home: PathBuf) -> Self {
        Self {
            config_lock: Arc::new(Mutex::new(())),
            connections: Arc::new(Mutex::new(BTreeMap::new())),
            closing: Mutex::new(Vec::new()),
            home,
        }
    }

    fn lock_pool(&self) -> std::sync::MutexGuard<'_, Connections> {
        self.connections.lock().expect("MCP connection lock poisoned")
    }

    async fn with_config<T: Send + 'static>(
        &self,
        use_config: impl FnOnce(Configuration, &mut Connections) -> Result<T, McpError> + Send + 'static,
    ) -> Result<T, McpError> {
        let home = self.home.clone();
        let config_lock = Arc::clone(&self.config_lock);
        let connections = Arc::clone(&self.connections);
        tokio::task::spawn_blocking(move || {
            let _config_guard = config_lock.lock().expect("MCP config lock poisoned");
            let config = config::read(&home)?;
            let mut pool = connections.lock().expect("MCP connection lock poisoned");
            use_config(config, &mut pool)
        })
        .await
        .expect("MCP config read completes while the runtime is running")
    }

    fn track_close(&self, task: tokio::task::JoinHandle<()>) {
        let mut closing = self.closing.lock().expect("MCP closing lock poisoned");
        closing.retain(|existing| !existing.is_finished());
        closing.push(task);
    }

    fn spawn_close(&self, cells: Vec<ConnectionCell>) {
        if !cells.is_empty() {
            self.track_close(tokio::spawn(close_connections(cells)));
        }
    }

    pub fn list(&self, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        let _config_guard = self.config_lock.lock().expect("MCP config lock poisoned");
        let config = config::read(&self.home)?;
        Ok(Self::states(config, cwd, &self.lock_pool()))
    }

    pub fn save(&self, input: McpServerInput, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        let id = input.server_id.clone();
        let config = ServerConfig::from_input(input);
        config.validate(&id)?;
        self.update(&id.clone(), cwd, |all| {
            all.servers.insert(id, config);
            Ok(())
        })
    }

    pub fn remove(&self, id: &str, cwd: &Path) -> Result<Vec<McpServerState>, McpError> {
        self.update(id, cwd, |all| {
            all.servers.remove(id).ok_or_else(McpError::not_found)?;
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
            let _config_guard = self.config_lock.lock().expect("MCP config lock poisoned");
            let mut config = config::read(&self.home)?;
            change(&mut config)?;
            config::write(&self.home, &config)?;
            let mut pool = self.lock_pool();
            let evicted = evict_stale(&mut pool, &config, Some(id));
            let states = Self::states(config, cwd, &pool);
            (evicted, states)
        };
        self.spawn_close(evicted);
        Ok(states)
    }

    fn states(config: Configuration, cwd: &Path, connections: &Connections) -> Vec<McpServerState> {
        let directory = app_core::workspace_key(cwd);
        config
            .servers
            .into_iter()
            .map(|(id, config)| Self::server_state(id, config, &directory, connections))
            .collect()
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
                        .map(|tool| {
                            McpTool::new(
                                tool.name.into_owned(),
                                tool.description.map(String::from).unwrap_or_default(),
                                tool.input_schema,
                                Arc::clone(&connection),
                            )
                        })
                        .collect();
                    let prompt = connection.instructions().map(|text| format!("MCP server {id}:\n{text}"));
                    Ok(McpServerDiscovery {
                        id: id.clone(),
                        tools,
                        prompt,
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
            state.catalog = Err(error.clone());
            McpServerDiscovery {
                id,
                tools: Vec::new(),
                prompt: Some(prompt),
                error: Some(error),
            }
        })
    }

    pub async fn discover(&self, cwd: &Path, signal: &CancellationToken, skip: &[String]) -> McpDiscovery {
        if signal.is_cancelled() {
            return McpDiscovery::default();
        }
        let directory = app_core::workspace_key(cwd);
        let skip = skip.to_vec();
        let preparing = signal.clone();
        let prepared = self
            .with_config(move |config, pool| {
                if preparing.is_cancelled() {
                    return Ok((Vec::new(), Vec::new()));
                }
                let evicted = evict_stale(pool, &config, None);
                let mut entries = Vec::new();
                for (id, config) in config.servers {
                    if skip.contains(&id) {
                        continue;
                    }
                    let key = (directory.clone(), id.clone());
                    let (_, cell) =
                        pool.entry(key).or_insert_with(|| (config.clone(), ConnectionState::new_cell()));
                    entries.push((id, config, Arc::clone(cell)));
                }
                Ok((entries, evicted))
            })
            .await;
        let (entries, evicted) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return McpDiscovery {
                    servers: Vec::new(),
                    error: Some(error.to_string()),
                };
            }
        };
        self.spawn_close(evicted);
        let mut pending = tokio::task::JoinSet::new();
        for (id, config, cell) in entries {
            let cwd = cwd.to_path_buf();
            pending.spawn(async move {
                let state = cell.lock_owned().await;
                Self::tools(id, &cwd, config, state).await
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

    pub async fn reconnect(
        &self,
        id: &str,
        cwd: &Path,
        shutdown: &CancellationToken,
    ) -> Result<McpServerState, McpError> {
        let cancelled = || McpError::new("工作台正在退出。");
        let key = (app_core::workspace_key(cwd), id.to_owned());
        let prepare_key = key.clone();
        let preparing = shutdown.clone();
        let (config, cell, guard, evicted) = self
            .with_config(move |config, pool| {
                if preparing.is_cancelled() {
                    return Err(cancelled());
                }
                let server_config =
                    config.servers.get(&prepare_key.1).cloned().ok_or_else(McpError::not_found)?;
                let mut evicted = evict_stale(pool, &config, None);
                if let Some((_, old_cell)) = pool.remove(&prepare_key) {
                    evicted.push(old_cell);
                }
                let cell = ConnectionState::new_cell();
                let guard =
                    Arc::clone(&cell).try_lock_owned().expect("fresh MCP connection cell is unlocked");
                pool.insert(prepare_key, (server_config.clone(), Arc::clone(&cell)));
                Ok((server_config, cell, guard, evicted))
            })
            .await?;
        let evicted_closed = if evicted.is_empty() {
            None
        } else {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            self.track_close(tokio::spawn(async move {
                close_connections(evicted).await;
                let _ = done_tx.send(());
            }));
            Some(done_rx)
        };
        shutdown
            .run_until_cancelled(async {
                if let Some(closed) = evicted_closed {
                    let _ = closed.await;
                }
                let still_current =
                    self.lock_pool().get(&key).is_some_and(|(_, current)| Arc::ptr_eq(current, &cell));
                if still_current {
                    let _ = Self::tools(id.to_owned(), cwd, config, guard).await;
                } else {
                    drop(guard);
                }
                self.with_config(move |mut config, pool| {
                    let (directory, id) = key;
                    let config = config.servers.remove(&id).ok_or_else(McpError::not_found)?;
                    Ok(Self::server_state(id, config, &directory, pool))
                })
                .await
            })
            .await
            .ok_or_else(cancelled)?
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
                inspection.tools.clone_from(tools);
            }
            (Ok(_), None) => {}
        }
        inspection
    }

    pub fn disconnect_directory(&self, cwd: &Path) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut pool = self.lock_pool();
        let key = app_core::workspace_key(cwd);
        let cells = pool
            .extract_if(.., |(directory, _), _| directory == &key)
            .map(|(_, (_, cell))| cell)
            .collect();
        close_connections(cells)
    }

    pub async fn shutdown(&self) {
        let cells = std::mem::take(&mut *self.lock_pool()).into_values().map(|(_, cell)| cell).collect();
        let pending = std::mem::take(&mut *self.closing.lock().expect("MCP closing lock poisoned"));
        let drain = async move {
            for handle in pending {
                if let Err(error) = handle.await {
                    tracing::warn!(error = %error, "MCP detached close task failed");
                }
            }
        };
        tokio::join!(close_connections(cells), drain);
    }
}

async fn close_connections(cells: Vec<ConnectionCell>) {
    let mut closing = tokio::task::JoinSet::new();
    for cell in cells {
        closing.spawn(async move {
            let connection = cell.lock().await.connection.take();
            if let Some(connection) = connection.and_then(Arc::into_inner) {
                connection.close().await;
            }
        });
    }
    while closing.join_next().await.is_some() {}
}
