use crate::monitoring::{LogEntry, MonitoringEvent, MonitoringHub, StreamStatusPayload};
use crate::state::{ActiveAlert, AppState};
use crate::Config;
use anyhow::Result;
use axum::http::HeaderMap;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use reqwest::header;
use reqwest::header::HeaderValue;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::{self, Duration, MissedTickBehavior};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

const DEEPLINK_HOST_CACHE_FILE: &str = "deeplink_host.txt";
const DEEPLINK_HOST_LAST_SEEN_CACHE_FILE: &str = "deeplink_host_last_seen.txt";

#[derive(Clone)]
struct ApiState {
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    config: Config,
    deeplink_host_cache: Arc<Mutex<Option<String>>>,
    last_seen_host_cache: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Deserialize, Default)]
struct LogsQuery {
    tail: Option<usize>,
}

#[derive(Debug, Serialize)]
struct LogsResponse {
    logs: Vec<LogEntry>,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    streams: Vec<StreamStatusPayload>,
    active_alerts: Vec<ActiveAlert>,
}

#[derive(Debug, Deserialize)]
struct Params {
    auth: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "payload")]
enum WsMessage {
    Snapshot(SnapshotPayload),
    Log(LogEntry),
    Stream(StreamStatusPayload),
    Alerts(Vec<ActiveAlert>),
}

#[derive(Debug, Serialize)]
struct SnapshotPayload {
    streams: Vec<StreamStatusPayload>,
    active_alerts: Vec<ActiveAlert>,
    logs: Vec<LogEntry>,
}

impl From<MonitoringEvent> for WsMessage {
    fn from(event: MonitoringEvent) -> Self {
        match event {
            MonitoringEvent::Log(entry) => WsMessage::Log(entry),
            MonitoringEvent::Stream(status) => WsMessage::Stream(status),
            MonitoringEvent::Alerts(alerts) => WsMessage::Alerts(alerts),
        }
    }
}

fn cors_layer() -> CorsLayer {
    let json_config = Config::from_config_json("/app/config.json");

    if json_config.as_ref().unwrap().use_reverse_proxy.to_string() != "true" {
        let origin: HeaderValue = format!(
            "http://{}:{}/",
            "localhost",
            json_config.as_ref().unwrap().monitoring_bind_port
        )
        .parse()
        .unwrap_or_else(|_| HeaderValue::from_static("http://localhost:8080"));

        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE])
            .max_age(Duration::from_secs(86400))
    } else {
        let origin: HeaderValue = format!(
            "http://{}/",
            json_config.as_ref().unwrap().ws_reverse_proxy_url
        )
        .parse()
        .unwrap_or_else(|_| HeaderValue::from_static("http://localhost"));

        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE])
            .max_age(Duration::from_secs(86400))
    }
}

async fn auth(req: Request, next: Next) -> Result<Response, StatusCode> {
    if req.method() == Method::OPTIONS {
        return Ok(next.run(req).await);
    }

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    match auth_header {
        Some(auth_header) if token_is_valid(auth_header) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn token_is_valid(auth_header: &str) -> bool {
    let json_config = Config::from_config_json("/app/config.json");

    if !auth_header.starts_with("Bearer ") {
        info!("Auth header does not start with 'Bearer '");
        return false;
    }

    let token = &auth_header[7..];
    let username = json_config.as_ref().unwrap().dashboard_username.clone();
    let password = json_config.as_ref().unwrap().dashboard_password.clone();

    if username.is_empty() || password.is_empty() || username == "admin" || password == "password" {
        info!("Default or empty username/password in use, rejecting token");
        return false;
    }

    let expected_token = Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", username, password),
    );

    token == expected_token
}

fn sanitize_host_header(raw: &str) -> Option<String> {
    let candidate = raw.split(',').next()?.trim();
    if candidate.is_empty() {
        return None;
    }

    let host_only = if candidate.starts_with('[') {
        let end = candidate.find(']')?;
        candidate.get(1..end)?
    } else if candidate.matches(':').count() == 1 {
        candidate.split(':').next().unwrap_or(candidate)
    } else {
        candidate
    }
    .trim()
    .trim_matches('.');

    if host_only.is_empty() {
        return None;
    }

    Some(host_only.to_string())
}

fn is_loopback_host(host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    lowered == "localhost" || lowered == "127.0.0.1" || lowered == "::1"
}

fn extract_deeplink_host_candidate(headers: &HeaderMap) -> Option<String> {
    if let Some(xfh) = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .and_then(sanitize_host_header)
    {
        return Some(xfh);
    }

    headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .and_then(sanitize_host_header)
}

async fn maybe_persist_deeplink_host(headers: &HeaderMap, state: &ApiState) {
    let Some(host) = extract_deeplink_host_candidate(headers) else {
        return;
    };

    let should_write_last_seen = {
        let guard = state.last_seen_host_cache.lock().await;
        guard.as_deref() != Some(host.as_str())
    };

    if should_write_last_seen {
        let last_seen_file = state
            .config
            .shared_state_dir
            .join(DEEPLINK_HOST_LAST_SEEN_CACHE_FILE);
        match tokio::fs::write(&last_seen_file, &host).await {
            Ok(_) => {
                let mut guard = state.last_seen_host_cache.lock().await;
                *guard = Some(host.clone());
            }
            Err(err) => warn!(
                "Failed to persist last-seen deeplink host '{}' to {:?}: {}",
                host, last_seen_file, err
            ),
        }
    }

    if is_loopback_host(&host) {
        return;
    }

    let should_write_preferred = {
        let guard = state.deeplink_host_cache.lock().await;
        guard.as_deref() != Some(host.as_str())
    };

    if !should_write_preferred {
        return;
    }

    let host_file = state.config.shared_state_dir.join(DEEPLINK_HOST_CACHE_FILE);
    match tokio::fs::write(&host_file, &host).await {
        Ok(_) => {
            let mut guard = state.deeplink_host_cache.lock().await;
            *guard = Some(host);
        }
        Err(err) => warn!(
            "Failed to persist deeplink host '{}' to {:?}: {}",
            host, host_file, err
        ),
    }
}

pub async fn run_server(
    bind_addr: SocketAddr,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    config: Config,
) -> Result<()> {
    let state = ApiState {
        app_state,
        monitoring,
        config,
        deeplink_host_cache: Arc::new(Mutex::new(None)),
        last_seen_host_cache: Arc::new(Mutex::new(None)),
    };

    let protected_router = Router::new()
        .route("/api/logs", get(logs_handler))
        .route("/api/status", get(status_handler))
        .layer(cors_layer())
        .with_state(state.clone())
        .route_layer(middleware::from_fn(auth));

    let router = Router::new()
        .route("/api/health", get(health_handler))
        .route("/ws", get(ws_handler))
        .layer(cors_layer())
        .merge(protected_router)
        .with_state(state.clone());

    let listener = TcpListener::bind(bind_addr).await?;
    info!(%bind_addr, "Monitoring API listening");
    axum::serve(listener, router.into_make_service()).await?;
    Ok(())
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "OK".to_string(),
    })
}

async fn logs_handler(
    Query(params): Query<LogsQuery>,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Json<LogsResponse> {
    maybe_persist_deeplink_host(&headers, &state).await;
    let max_logs = state.monitoring.max_logs();
    let tail = params.tail.unwrap_or(100).clamp(1, max_logs);
    let logs = state.monitoring.recent_logs(tail);
    Json(LogsResponse { logs })
}

async fn status_handler(State(state): State<ApiState>, headers: HeaderMap) -> Json<StatusResponse> {
    maybe_persist_deeplink_host(&headers, &state).await;
    let streams = state.monitoring.stream_snapshots();
    let active_alerts = {
        let guard = state.app_state.lock().await;
        guard.active_alerts.clone()
    };
    Json(StatusResponse {
        streams,
        active_alerts,
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ApiState>,
    Query(params): Query<Params>,
) -> impl IntoResponse {
    let auth_header = format!("Bearer {}", params.auth);

    if !token_is_valid(&auth_header) {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    } else {
        ws.on_upgrade(move |socket| ws_connection(socket, state))
    }
}

async fn ws_connection(mut socket: WebSocket, state: ApiState) {
    if let Err(err) = send_snapshot(&mut socket, &state).await {
        error!("Failed to send initial snapshot: {err}");
        let _ = socket.close().await;
        return;
    }

    let mut events = state.monitoring.subscribe();
    let mut heartbeat = time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let message: WsMessage = event.into();
                        if let Err(err) = send_ws_message(&mut socket, &message).await {
                            error!("Failed to send monitoring event: {err}");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Err(_err)) => {
                        //error!("WebSocket receive error: {err}");
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }

    let _ = socket.close().await;
}

async fn send_snapshot(socket: &mut WebSocket, state: &ApiState) -> Result<()> {
    let streams = state.monitoring.stream_snapshots();
    let logs = state.monitoring.recent_logs(100);
    let active_alerts = {
        let guard = state.app_state.lock().await;
        guard.active_alerts.clone()
    };
    let snapshot = WsMessage::Snapshot(SnapshotPayload {
        streams,
        active_alerts,
        logs,
    });
    send_ws_message(socket, &snapshot).await
}

async fn send_ws_message(socket: &mut WebSocket, message: &WsMessage) -> Result<()> {
    let payload = serde_json::to_string(message)?;
    socket.send(Message::Text(payload)).await?;
    Ok(())
}
