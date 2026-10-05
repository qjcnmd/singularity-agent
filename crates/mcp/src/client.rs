//! SDK 传输、进程生命周期及可取消的 MCP 调用。

use crate::config::{ServerConfig, TransportConfig};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::*,
    service::{Peer, PeerRequestOptions, RequestContext, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig, which_command,
    },
};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub(crate) struct Connection {
    peer: Peer<RoleClient>,
    service: tokio::sync::Mutex<Option<RunningService<RoleClient, Handler>>>,
    config: ServerConfig,
}

// 当前主流服务器仍以 legacy initialize/roots 协商；不广告未实现的 sampling/elicitation。
#[allow(deprecated)]
pub(crate) struct Handler {
    root: String,
}

#[allow(deprecated)]
impl ClientHandler for Handler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info.name = "Singularity".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities.roots = Some(RootsCapabilities::default());
        info
    }

    async fn list_roots(&self, _: RequestContext<RoleClient>) -> Result<ListRootsResult, rmcp::ErrorData> {
        Ok(ListRootsResult::new(vec![Root::new(&self.root)]))
    }
}

impl Connection {
    pub async fn connect(cwd: &Path, config: ServerConfig) -> Result<Self, String> {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let root = url::Url::from_directory_path(cwd)
            .map_err(|_| "MCP 项目目录无法转换为文件 URI。")?
            .to_string();
        let handler = Handler { root };
        let startup = Duration::from_secs(config.startup_timeout_sec);
        let connect = async {
            let service = match &config.transport {
                TransportConfig::Stdio { command, args, cwd: configured_cwd, env } => {
                    let mut command =
                        which_command(command).map_err(|error| format!("无法启动 MCP：{error}"))?;
                    command.args(args).envs(env).current_dir(
                        configured_cwd
                            .as_ref()
                            .map(|path| cwd.join(path))
                            .unwrap_or_else(|| cwd.to_path_buf()),
                    );
                    #[cfg(windows)]
                    let command = {
                        use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject};
                        let mut wrapped = CommandWrap::from(command);
                        wrapped
                            .wrap(CreationFlags(windows::Win32::System::Threading::CREATE_NO_WINDOW))
                            .wrap(JobObject);
                        wrapped
                    };
                    let (transport, stderr) = TokioChildProcess::builder(command)
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .map_err(|error| format!("无法启动 MCP：{error}"))?;
                    if let Some(mut stderr) = stderr {
                        let logs = Arc::clone(&logs);
                        tokio::spawn(async move {
                            let mut buffer = [0u8; 2048];
                            while let Ok(size) = stderr.read(&mut buffer).await {
                                if size == 0 {
                                    break;
                                }
                                let mut logs = logs.lock().expect("MCP stderr lock poisoned");
                                // 管道分块可能落在 UTF-8 字符内部；保留字节，到报告错误时再解码。
                                logs.extend_from_slice(&buffer[..size]);
                                if logs.len() > 4096 {
                                    let start = logs.len() - 4096;
                                    logs.drain(..start);
                                }
                            }
                        });
                    }
                    handler.serve(transport).await.map_err(|error| format!("MCP 初始化失败：{error}"))?
                }
                TransportConfig::Http { url, headers } => {
                    // 与现有模型传输使用同一 ring provider，避免两个 TLS 默认实现冲突。
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    let client = reqwest::Client::builder()
                        .connect_timeout(startup)
                        .build()
                        .map_err(|error| format!("无法建立 MCP HTTP 客户端：{error}"))?;
                    let headers = headers
                        .iter()
                        .map(|(name, value)| {
                            (
                                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                                    .expect("MCP header name was validated when loading configuration"),
                                reqwest::header::HeaderValue::from_str(value)
                                    .expect("MCP header value was validated when loading configuration"),
                            )
                        })
                        .collect();
                    let transport = StreamableHttpClientTransport::with_client(
                        client,
                        StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(headers),
                    );
                    handler.serve(transport).await.map_err(|error| format!("MCP 初始化失败：{error}"))?
                }
            };
            Ok(Self {
                peer: service.peer().clone(),
                service: tokio::sync::Mutex::new(Some(service)),
                config: config.clone(),
            })
        };
        tokio::time::timeout(startup, connect)
            .await
            .unwrap_or_else(|_| Err(format!("MCP 初始化超过 {} 秒。", config.startup_timeout_sec)))
            .map_err(|error| {
                let logs = logs.lock().expect("MCP stderr lock poisoned");
                // 尾部预算可能切掉首字符的一部分，只跳过开头残留的 UTF-8 续字节。
                let start = logs.iter().position(|byte| byte & 0xc0 != 0x80).unwrap_or(logs.len());
                let logs = String::from_utf8_lossy(&logs[start..]);
                config.redact(if logs.is_empty() {
                    error
                } else {
                    format!("{error}\n{logs}")
                })
            })
    }

    pub async fn tools(&self) -> Result<Vec<Tool>, String> {
        // 每个回合重新发现，接纳服务器目录变更；回合内部由 Agent 冻结 schema。
        tokio::time::timeout(Duration::from_secs(self.config.startup_timeout_sec), self.peer.list_all_tools())
            .await
            .map_err(|_| "MCP 工具发现超时。".to_string())?
            .map_err(|error| self.config.redact(format!("MCP 工具发现失败：{error}")))
    }

    pub fn instructions(&self) -> Option<String> {
        self.peer.peer_info().and_then(|info| info.instructions.clone())
    }

    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<CallToolResult, String> {
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
        let request = self
            .peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::with_timeout(Duration::from_secs(self.config.tool_timeout_sec)),
            )
            .await
            .map_err(|error| self.config.redact(format!("MCP 调用失败：{error}")))?;
        let id = request.id.clone();
        let result = tokio::select! {
            () = signal.cancelled() => {
                self.peer.notify_cancelled(CancelledNotificationParam::new(Some(id), Some("User stopped the turn".into()))).await
                    .map_err(|error| self.config.redact(format!("MCP 取消通知失败：{error}")))?;
                return Err("Operation aborted".into());
            }
            result = request.await_response() => result,
        }.map_err(|error| self.config.redact(format!("MCP 调用失败：{error}")))?;
        match result {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err("MCP 服务器返回了不支持的工具结果。".into()),
        }
    }

    pub async fn close(&self) {
        if let Some(mut service) = self.service.lock().await.take()
            && let Err(error) = service.close_with_timeout(Duration::from_secs(3)).await
        {
            eprintln!("MCP shutdown: {}", self.config.redact(error.to_string()));
        }
    }
}

/// 常规工具名保留可读前缀；长名称或分隔符冲突使用稳定摘要，适配模型 API 的名称约束。
pub(crate) fn tool_alias(server: &str, tool: &str) -> String {
    let alias = format!("mcp__{server}__{tool}");
    if alias.len() <= 64
        && !server.contains("__")
        && alias.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return alias;
    }
    let readable = |value: &str| {
        value
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .take(16)
            .collect::<String>()
    };
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let suffix = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("mcp__{}__{}_{suffix}", readable(server), readable(tool))
}
