use crate::{McpError, config::ServerConfig};
use protocol::McpTransportConfig;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest,
        RootsCapabilities, ServerResult, Tool,
    },
    service::{Peer, PeerRequestOptions, RequestContext, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig, which_command,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub(crate) struct Connection {
    peer: Peer<RoleClient>,
    service: RunningService<RoleClient, Handler>,
    config: ServerConfig,
}

pub(crate) struct Handler {
    root: String,
}

#[expect(
    deprecated,
    reason = "legacy initialize/roots negotiation with current MCP servers"
)]
impl ClientHandler for Handler {
    fn get_info(&self) -> rmcp::model::ClientInfo {
        let mut info = rmcp::model::ClientInfo::default();
        info.client_info.name = app_core::identity::APP_NAME.into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities.roots = Some(RootsCapabilities::default());
        info
    }

    async fn list_roots(
        &self,
        _: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListRootsResult::new(vec![rmcp::model::Root::new(&self.root)]))
    }
}

#[derive(Default)]
struct StderrCapture {
    logs: Arc<Mutex<Vec<u8>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl StderrCapture {
    fn attach(&self, mut stderr: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
        let logs = Arc::clone(&self.logs);
        let task = tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            while let Ok(size) = stderr.read(&mut buffer).await {
                if size == 0 {
                    break;
                }
                let mut logs = logs.lock().expect("MCP stderr lock poisoned");
                logs.extend_from_slice(&buffer[..size]);
                if logs.len() > 4096 {
                    let start = logs.len() - 4096;
                    logs.drain(..start);
                }
            }
        });
        *self.task.lock().expect("MCP stderr lock poisoned") = Some(task);
    }

    async fn read_tail(&self) -> String {
        let task = self.task.lock().expect("MCP stderr lock poisoned").take();
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_millis(200), task).await;
        }
        let logs = self.logs.lock().expect("MCP stderr lock poisoned");
        let start = logs.iter().position(|byte| byte & 0xc0 != 0x80).unwrap_or(logs.len());
        String::from_utf8_lossy(&logs[start..]).into_owned()
    }
}

impl Connection {
    pub fn is_closed(&self) -> bool {
        self.peer.is_transport_closed()
    }

    pub async fn connect(cwd: &Path, config: ServerConfig) -> Result<Self, McpError> {
        let stderr_capture = StderrCapture::default();
        let root = reqwest::Url::from_directory_path(cwd)
            .map_err(|()| McpError::new(format!("MCP 项目目录无法转换为文件 URI：{}", cwd.display())))?
            .to_string();
        let handler = Handler { root };
        let startup = Duration::from_secs(config.startup_timeout_sec);
        let connect = async {
            let service = match &config.transport {
                McpTransportConfig::Stdio { command, args, cwd: configured_cwd, env } => {
                    connect_stdio(
                        cwd,
                        handler,
                        &stderr_capture,
                        command,
                        args,
                        configured_cwd.as_deref(),
                        env,
                    )
                    .await?
                }
                McpTransportConfig::Http { url, headers } => {
                    connect_http(handler, startup, url, headers).await?
                }
            };
            service.set_response_cache_config(rmcp::service::ClientCacheConfig::disabled()).await;
            Ok(Self {
                peer: service.peer().clone(),
                service,
                config: config.clone(),
            })
        };
        let result = tokio::time::timeout(startup, connect).await.unwrap_or_else(|_| {
            Err(McpError::new(format!("MCP 初始化超过 {} 秒。", config.startup_timeout_sec)))
        });
        match result {
            Ok(connection) => Ok(connection),
            Err(error) => {
                let logs = stderr_capture.read_tail().await;
                let message = error.to_string();
                Err(McpError::new(config.redact(if logs.is_empty() {
                    message
                } else {
                    format!("{message}\n{logs}")
                })))
            }
        }
    }

    pub async fn tools(&self) -> Result<Vec<Tool>, McpError> {
        tokio::time::timeout(Duration::from_secs(self.config.startup_timeout_sec), self.peer.list_all_tools())
            .await
            .map_err(|_| McpError::new("MCP 工具发现超时。"))?
            .map_err(|error| McpError::new(self.config.redact(format!("MCP 工具发现失败：{error}"))))
    }

    pub fn instructions(&self) -> Option<String> {
        self.peer.peer_info().and_then(|info| info.instructions.clone())
    }

    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
        let mut request_id = None;
        // SDK 的响应超时仍会等待取消通知刷入管道；在这里统一限制发送和响应等待。
        let reason = tokio::select! {
            biased;
            () = signal.cancelled() => "Operation aborted".to_string(),
            result = tokio::time::timeout(Duration::from_secs(self.config.tool_timeout_sec), async {
                let request = self.peer.send_cancellable_request(
                    ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                    PeerRequestOptions::no_options(),
                ).await?;
                request_id = Some(request.id.clone());
                request.await_response().await
            }) => match result {
                Ok(result) => {
                    let result = result.map_err(|error| {
                        McpError::new(self.config.redact(format!("MCP 调用失败：{error}")))
                    })?;
                    return match result {
                        ServerResult::CallToolResult(result) => Ok(result),
                        _ => Err(McpError::new("MCP 服务器返回了不支持的工具结果。")),
                    };
                }
                Err(_) => format!("MCP 调用超过 {} 秒。", self.config.tool_timeout_sec),
            },
        };
        let message = if let Some(id) = request_id {
            match tokio::time::timeout(
                Duration::from_secs(3),
                self.peer
                    .notify_cancelled(CancelledNotificationParam::new(Some(id), Some(reason.clone()))),
            )
            .await
            {
                Ok(Ok(())) => reason,
                Ok(Err(error)) => self.config.redact(format!("{reason} MCP 取消通知失败：{error}")),
                Err(_) => format!("{reason} MCP 取消通知超过 3 秒。"),
            }
        } else {
            reason
        };
        Err(McpError::new(message))
    }

    pub async fn close(mut self) {
        // 给 rmcp 内部 3 秒优雅退出的基础上预留额外时间，确保超时后能完成 kill。
        match self.service.close_with_timeout(Duration::from_secs(5)).await {
            Ok(Some(_)) => {}
            Ok(None) => tracing::warn!("MCP shutdown timed out after 5s"),
            Err(error) => {
                tracing::warn!(error = %self.config.redact(error.to_string()), "MCP shutdown failed");
            }
        }
    }
}

async fn connect_stdio(
    cwd: &Path,
    handler: Handler,
    stderr_capture: &StderrCapture,
    command: &str,
    args: &[String],
    configured_cwd: Option<&str>,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<RunningService<RoleClient, Handler>, McpError> {
    // 剥离 Windows verbatim 前缀后再拼接相对路径与设置 current_dir，避免 Win32/cmd.exe 拒绝。
    let base = PathBuf::from(app_core::display_path(cwd));
    let directory = configured_cwd.map(|path| base.join(path)).unwrap_or(base);
    let directory = std::path::absolute(&directory).unwrap_or(directory);
    let executable: PathBuf = if Path::new(command).components().count() > 1 {
        directory.join(command)
    } else {
        command.into()
    };
    let mut command = tokio::task::spawn_blocking(move || which_command(executable))
        .await
        .map_err(|error| McpError::new(format!("无法启动 MCP：{error}")))?
        .map_err(|error| McpError::new(format!("无法启动 MCP：{error}")))?;
    command.args(args).envs(env).current_dir(directory);
    let command = {
        use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
        let mut wrapped = CommandWrap::from(command);
        wrapped
            .wrap(CreationFlags(windows::Win32::System::Threading::CREATE_NO_WINDOW))
            .wrap(JobObject)
            .wrap(KillOnDrop);
        wrapped
    };
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| McpError::new(format!("无法启动 MCP：{error}")))?;
    if let Some(stderr) = stderr {
        stderr_capture.attach(stderr);
    }
    handler
        .serve(transport)
        .await
        .map_err(|error| McpError::new(format!("MCP 初始化失败：{error}")))
}

async fn connect_http(
    handler: Handler,
    startup: Duration,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<RunningService<RoleClient, Handler>, McpError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        // reqwest 跨源重定向时只移除预定义的鉴权头。
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .connect_timeout(startup)
        .build()
        .map_err(|error| McpError::new(format!("无法建立 MCP HTTP 客户端：{error}")))?;
    let headers = headers
        .iter()
        .map(|(name, value)| {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| McpError::new(format!("MCP 请求头名称无效（{name}）：{error}")))?;
            let mut header_value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|error| McpError::new(format!("MCP 请求头值无效（{name}）：{error}")))?;
            header_value.set_sensitive(true);
            Ok((header_name, header_value))
        })
        .collect::<Result<_, McpError>>()?;
    let mut transport_config =
        StreamableHttpClientTransportConfig::with_uri(url.to_owned()).custom_headers(headers);
    // 会话过期也直接报告调用失败，不能由 SDK 重放可能有副作用的工具。
    transport_config.reinit_on_expired_session = false;
    let transport = StreamableHttpClientTransport::with_client(client, transport_config);
    handler
        .serve(transport)
        .await
        .map_err(|error| McpError::new(format!("MCP 初始化失败：{error}")))
}
