//! Loopback Axum Host、browser-auth 边界与有界 WebSocket 广播。

use std::collections::HashMap;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use singularity_protocol::{StreamEnvelope, StreamType, WORKBENCH_PROTOCOL_VERSION};
use tokio::sync::broadcast;

use crate::session_options::WebSetup;

use super::auth::BrowserAuth;
use super::rpc;
use super::static_files;
use super::workbench::Workbench;

pub struct HostState {
    pub auth: BrowserAuth,
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
    let auth = BrowserAuth::open(&setup.home, authority.clone())?;
    let entry_url = auth.entry_url();
    let workbench = Workbench::new(
        authority,
        setup.runner,
        setup.catalog,
        setup.workspaces,
        setup.models,
    );
    let state = Arc::new(HostState { auth, workbench });
    let app = Router::new()
        .route("/", get(root))
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

async fn root(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !state.auth.validate_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(cookie) = query
        .get("token")
        .and_then(|token| state.auth.exchange(token))
    {
        let Ok(cookie) = HeaderValue::from_str(&cookie) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SEE_OTHER;
        response
            .headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/"));
        response.headers_mut().insert(header::SET_COOKIE, cookie);
        secure_headers(response.headers_mut(), state.auth.authority());
        return response;
    }
    if !state.auth.has_valid_cookie(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response = static_files::index();
    secure_headers(response.headers_mut(), state.auth.authority());
    response
}

async fn asset(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response<Body> {
    if !state.auth.validate_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.auth.has_valid_cookie(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response = static_files::asset(&path);
    secure_headers(response.headers_mut(), state.auth.authority());
    response
}

async fn events(
    State(state): State<Arc<HostState>>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    if !state.auth.validate_api_source(&headers, false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !state.auth.has_valid_cookie(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
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
                        event_type: StreamType::ResyncRequired,
                        session_id: None,
                        payload: serde_json::json!({"reason": "client_lagged"}),
                    };
                    let _ = send_frame(&mut socket, &frame).await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &StreamEnvelope) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(text.into())),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
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
    let policy = format!(
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self' ws://{authority}"
    );
    if let Ok(value) = HeaderValue::from_str(&policy) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
}
