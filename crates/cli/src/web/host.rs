//! Loopback Axum Host、同源边界与有界 WebSocket 广播。

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
use singularity_protocol::{
    ResyncRequiredPayload, StreamEnvelope, StreamEvent, WORKBENCH_PROTOCOL_VERSION,
};
use tokio::sync::broadcast;

use crate::session_options::WebSetup;

use super::origin::WebOrigin;
use super::rpc;
use super::static_files;
use super::workbench::Workbench;

pub struct HostState {
    pub origin: WebOrigin,
    pub workbench: Arc<Workbench>,
}

pub async fn run(setup: WebSetup, port: u16, no_open: bool) -> Result<(), String> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(|error| format!("cannot bind 127.0.0.1:{port}: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("cannot inspect workbench listener: {error}"))?;
    let authority = format!("127.0.0.1:{}", address.port());
    let origin = WebOrigin::new(authority);
    let entry_url = origin.entry_url();
    let workbench = Workbench::new(
        setup.runner,
        setup.catalog,
        setup.workspaces,
        setup.models,
        setup.home,
    );
    let state = Arc::new(HostState { origin, workbench });
    let app = Router::new()
        .route("/", get(root))
        .route("/favicon.svg", get(favicon))
        .route("/assets/{*path}", get(asset))
        .route("/api/rpc", post(rpc::handle))
        .route("/api/events", get(events))
        .fallback(not_found)
        .with_state(state);

    println!("Singularity workbench ready: {entry_url}");
    let _ = std::io::stdout().flush();
    if !no_open && let Err(error) = webbrowser::open(&entry_url) {
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
        .map_err(|error| format!("workbench host failed: {error}"))
}

/// 根页面、标签页图标与构建产物共用同一条静态规则：Host 校验通过后附加安全头。
fn static_response(
    state: &HostState,
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

async fn root(State(state): State<Arc<HostState>>, headers: HeaderMap) -> Response<Body> {
    static_response(&state, &headers, static_files::index())
}

async fn favicon(State(state): State<Arc<HostState>>, headers: HeaderMap) -> Response<Body> {
    static_response(&state, &headers, static_files::favicon())
}

async fn asset(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response<Body> {
    static_response(&state, &headers, static_files::asset(&path))
}

async fn events(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    if !state.origin.validate_api_source(&headers, false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let receiver = state.workbench.subscribe();
    let workbench = Arc::clone(&state.workbench);
    websocket
        .on_upgrade(move |socket| stream(socket, workbench, receiver))
        .into_response()
}

async fn stream(
    mut socket: WebSocket,
    workbench: Arc<Workbench>,
    mut receiver: broadcast::Receiver<StreamEnvelope>,
) {
    if send_frame(&mut socket, &workbench.ready_frame())
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
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let frame = StreamEnvelope {
                        version: WORKBENCH_PROTOCOL_VERSION,
                        generation: workbench.generation().to_string(),
                        revision: workbench.revision(),
                        event: StreamEvent::ResyncRequired { payload: ResyncRequiredPayload { reason: "client_lagged".into() } },
                    };
                    let _ = send_frame(&mut socket, &frame).await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// 发送一帧；失败一律结束该连接。客户端断开是正常收尾，不报告；序列化失败与
/// 超时表示本进程或链路出了问题，报告一次以便定位。
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
    // font-src 放开 data:：打包器会把小于阈值的 KaTeX 字体内联成 data: URL，
    // 不放开会让这一档大号数学符号字体被拦下并退回替代字体。图片早已如此例外。
    let policy = format!(
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self' data:; connect-src 'self' ws://{authority}"
    );
    if let Ok(value) = HeaderValue::from_str(&policy) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
}
