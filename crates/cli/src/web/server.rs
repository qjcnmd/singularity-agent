//! 只监听回环地址的 Axum Host、同源边界，以及有上限的 WebSocket 广播。

use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use singularity_protocol::{EmptyParams, StreamEnvelope, StreamEvent};
use tokio::sync::broadcast;

use crate::session_options::WebSetup;

use super::app_server::AppServer;
use super::origin::WebOrigin;
use super::rpc;
use super::static_files;

pub struct ServerState {
    pub origin: WebOrigin,
    pub app_server: Arc<AppServer>,
}

pub async fn run(setup: WebSetup, port: u16, no_open: bool) -> Result<(), String> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(|error| format!("cannot bind 127.0.0.1:{port}: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("cannot inspect HTTP listener: {error}"))?;
    let authority = format!("127.0.0.1:{}", address.port());
    let origin = WebOrigin::new(authority);
    let entry_url = origin.entry_url();
    let app_server = AppServer::new(
        setup.runner,
        setup.catalog,
        setup.workspaces,
        setup.models,
        setup.home,
    );
    let state = Arc::new(ServerState { origin, app_server });
    let app = Router::new()
        .route("/", get(root))
        .route("/favicon.svg", get(favicon))
        .route("/assets/{*path}", get(asset))
        .route("/api/rpc", post(rpc::handle))
        .route("/api/events", get(events))
        .fallback(not_found)
        .with_state(state);

    println!("Singularity server ready: {entry_url}");
    let _ = std::io::stdout().flush();
    if !no_open && let Err(error) = open_default_browser(&entry_url) {
        eprintln!(
            "{}: default browser handoff failed: {error}",
            crate::PROGRAM_NAME
        );
        eprintln!("{}: open this URL: {entry_url}", crate::PROGRAM_NAME);
    }
    let _runtime_guard = setup.runtime;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| format!("HTTP server failed: {error}"))
}

/// 把入口地址交给系统默认浏览器。产品只支持 Windows，本 crate 也已经启用了
/// Win32_UI_Shell，所以直接调平台接口，不为了这一处再引入跨平台浏览器库。
fn open_default_browser(url: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::HSTRING;
    let operation = HSTRING::from("open");
    let target = HSTRING::from(url);
    // SAFETY: 两个 HSTRING 在调用期间都存活；owner 窗口、参数和工作目录都传空。
    let code =
        unsafe { ShellExecuteW(None, &operation, &target, None, None, SW_SHOWNORMAL) }.0 as usize;
    // 不变量：ShellExecuteW 返回值 <= 32 表示失败，大于 32 才算成功。
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW returned {code}"))
    }
}

/// 根页面、标签页图标和构建产物走同一条静态规则：Host 校验通过后再附加安全头。
fn static_response(
    state: &ServerState,
    headers: &HeaderMap,
    response: Response<Body>,
) -> Response<Body> {
    if !state.origin.validate_host(headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = response;
    secure_headers(response.headers_mut(), state.origin.authority());
    response
}

async fn root(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response<Body> {
    static_response(&state, &headers, static_files::index())
}

async fn favicon(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response<Body> {
    static_response(&state, &headers, static_files::favicon())
}

async fn asset(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response<Body> {
    static_response(&state, &headers, static_files::asset(&path))
}

async fn events(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    if !state.origin.validate_api_source(&headers, false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let receiver = state.app_server.subscribe();
    let app_server = Arc::clone(&state.app_server);
    websocket
        .on_upgrade(move |socket| stream(socket, app_server, receiver))
        .into_response()
}

async fn stream(
    mut socket: WebSocket,
    app_server: Arc<AppServer>,
    mut receiver: broadcast::Receiver<StreamEnvelope>,
) {
    if send_frame(
        &mut socket,
        &app_server.frame(StreamEvent::Ready {
            payload: EmptyParams {},
        }),
    )
    .await
    .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            outgoing = receiver.recv() => match outgoing {
                Ok(frame) => {
                    if send_frame(&mut socket, &frame).await.is_err() {
                        break;
                    }
                }
                // 客户端接收落后，积压的事件已被丢弃，只能让它重拉基线。
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let frame = app_server.frame(StreamEvent::ResyncRequired { payload: EmptyParams {} });
                    let _ = send_frame(&mut socket, &frame).await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// 发送一帧；只要失败就结束这条连接。客户端断开属于正常收尾，不报告；序列化
/// 失败和超时说明本进程或链路出了问题，各报告一次，方便定位。
async fn send_frame(socket: &mut WebSocket, frame: &StreamEnvelope) -> Result<(), ()> {
    let text = match serde_json::to_string(frame) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("could not serialize a stream frame: {error}");
            return Err(());
        }
    };
    match tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(text.into())),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(()),
        Err(_) => {
            eprintln!("stream frame send timed out after 5s");
            Err(())
        }
    }
}

async fn not_found() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

fn secure_headers(headers: &mut HeaderMap, authority: &str) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    // font-src 特意放开 data:：打包器会把小于阈值的 KaTeX 字体内联成 data: URL，
    // 不放行的话，这一档大号数学符号字体会被拦下、退回替代字体。图片早就这样例外了。
    let policy = format!(
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self' data:; connect-src 'self' ws://{authority}"
    );
    if let Ok(value) = HeaderValue::from_str(&policy) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
}
