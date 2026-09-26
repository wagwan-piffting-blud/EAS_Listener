use crate::archive::{self, ArchiveQuery};
use crate::db::DbHandle;
use crate::monitoring::{LogEntry, MonitoringEvent, MonitoringHub, StreamStatusPayload};
use crate::state::{ActiveAlert, AppState, CapRuntimeStatus};
use crate::web_assets;
use crate::Config;
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::HeaderMap;
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use reqwest::header;
use reqwest::header::HeaderValue;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::{self, Duration, MissedTickBehavior};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

const DEEPLINK_HOST_CACHE_FILE: &str = "deeplink_host.txt";
const DEEPLINK_HOST_LAST_SEEN_CACHE_FILE: &str = "deeplink_host_last_seen.txt";
/// How long to keep retrying a bind whose port is still held -- by the first-run setup server
/// that just handed over to this one, typically.
const BIND_RETRY_ATTEMPTS: u32 = 20;
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) static SAME_US_LOOKUP_JSON: Lazy<serde_json::Value> = Lazy::new(|| {
    serde_json::from_str(include_str!("../include/same-us.json")).expect("parse same-us.json")
});

#[derive(Clone)]
struct ApiState {
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    /// Replaced on every reload, so the sign-in credentials and paths the dashboard uses follow
    /// config.json without a restart.
    config: Arc<RwLock<Arc<Config>>>,
    db: DbHandle,
    reload_tx: mpsc::Sender<()>,
    test_alert_tx: mpsc::Sender<()>,
    deeplink_host_cache: Arc<Mutex<Option<String>>>,
    last_seen_host_cache: Arc<Mutex<Option<String>>>,
}

impl ApiState {
    fn config(&self) -> Arc<Config> {
        self.config.read().clone()
    }
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
pub(crate) struct HealthResponse {
    status: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    streams: Vec<StreamStatusPayload>,
    active_alerts: Vec<ActiveAlert>,
    cap_status: CapStatusPayload,
}

#[derive(Debug, Serialize)]
struct CapStatusPayload {
    active_alerts: usize,
    #[serde(flatten)]
    runtime: CapRuntimeStatus,
}

#[derive(Debug, Deserialize)]
struct Params {
    auth: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "payload")]
enum WsMessage {
    Snapshot(SnapshotPayload),
    Log(LogEntry),
    Stream(StreamStatusPayload),
    Alerts(Vec<ActiveAlert>),
    CapStatus(CapStatusPayload),
}

#[derive(Debug, Serialize)]
struct SnapshotPayload {
    streams: Vec<StreamStatusPayload>,
    active_alerts: Vec<ActiveAlert>,
    cap_status: CapStatusPayload,
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

fn cors_layer(config: &Config) -> CorsLayer {
    if !config.use_reverse_proxy {
        let origin: HeaderValue =
            format!("http://{}:{}/", "localhost", config.monitoring_bind_port)
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
        let origin: HeaderValue = format!("http://{}/", config.ws_reverse_proxy_url)
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

const SESSION_COOKIE: &str = "eas_session";
/// Three days, matching the session lifetime the PHP dashboard used.
const SESSION_MAX_AGE_SECS: i64 = 259_200;

async fn auth(
    State(state): State<ApiState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if req.method() == Method::OPTIONS {
        return Ok(next.run(req).await);
    }

    if request_is_authorized(req.headers(), None, &state.config()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn page_auth(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    if request_is_authorized(req.headers(), None, &state.config()) {
        return next.run(req).await;
    }

    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let location = format!("/login.html?redirect={}", urlencode(target));

    let mut headers = HeaderMap::new();
    match HeaderValue::from_str(&location) {
        Ok(value) => {
            headers.insert(header::LOCATION, value);
        }
        Err(_) => {
            headers.insert(header::LOCATION, HeaderValue::from_static("/login.html"));
        }
    }
    (StatusCode::SEE_OTHER, headers).into_response()
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The token the dashboard must present, or `None` when the credentials are unset or still the
/// shipped defaults -- in which case nothing is allowed in at all.
pub(crate) fn expected_token(config: &Config) -> Option<String> {
    let username = config.dashboard_username.as_str();
    let password = config.dashboard_password.as_str();

    if username.is_empty() || password.is_empty() || username == "admin" || password == "password" {
        return None;
    }

    Some(Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{username}:{password}"),
    ))
}

/// Compares without short-circuiting on the first differing byte, so a wrong token cannot be
/// refined one character at a time by timing the response.
pub(crate) fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().to_string())
}

/// Accepts the token from an Authorization header, the session cookie, or an explicit query
/// parameter. The cookie is what browsers send for page and `<audio>` requests, which cannot carry
/// a header; the query form stays for the WebSocket URL that already used it.
fn request_is_authorized(headers: &HeaderMap, query_token: Option<&str>, config: &Config) -> bool {
    let Some(expected) = expected_token(config) else {
        info!("Default or empty username/password in use, rejecting request");
        return false;
    };

    let header_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    let cookie_token = cookie_value(headers, SESSION_COOKIE);

    for candidate in [header_token, cookie_token.as_deref(), query_token]
        .into_iter()
        .flatten()
    {
        if tokens_match(candidate, &expected) {
            return true;
        }
    }

    false
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

fn session_cookie(token: &str, config: &Config) -> String {
    // Secure only behind a reverse proxy: a plain-HTTP LAN dashboard would otherwise never
    // receive the cookie back. This mirrors what the PHP session did.
    let secure = if config.use_reverse_proxy {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_MAX_AGE_SECS}; HttpOnly; SameSite=Lax{secure}"
    )
}

async fn login_handler(State(state): State<ApiState>, Json(body): Json<LoginRequest>) -> Response {
    let config = state.config();
    let Some(expected) = expected_token(&config) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Set DASHBOARD_USERNAME and DASHBOARD_PASSWORD in config.json to something other than \
             the defaults before signing in.",
        )
            .into_response();
    };

    let username_ok = tokens_match(&body.username, &config.dashboard_username);
    let password_ok = tokens_match(&body.password, &config.dashboard_password);

    if !(username_ok && password_ok) {
        warn!("Rejected dashboard sign-in for user '{}'", body.username);
        return (StatusCode::UNAUTHORIZED, "Invalid username or password.").into_response();
    }

    let mut headers = HeaderMap::new();
    match HeaderValue::from_str(&session_cookie(&expected, &config)) {
        Ok(cookie) => {
            headers.insert(header::SET_COOKIE, cookie);
        }
        Err(err) => {
            error!("Could not build the session cookie: {}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed.").into_response();
        }
    }

    info!("Dashboard sign-in for user '{}'", body.username);
    (headers, Json(serde_json::json!({ "ok": true }))).into_response()
}

async fn logout_handler(State(state): State<ApiState>) -> Response {
    let secure = if state.config().use_reverse_proxy {
        "; Secure"
    } else {
        ""
    };
    let cleared = format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}");

    let mut headers = HeaderMap::new();
    if let Ok(cookie) = HeaderValue::from_str(&cleared) {
        headers.insert(header::SET_COOKIE, cookie);
    }
    (headers, Json(serde_json::json!({ "ok": true }))).into_response()
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

    let shared_state_dir = state.config().shared_state_dir.clone();

    if should_write_last_seen {
        let last_seen_file = shared_state_dir.join(DEEPLINK_HOST_LAST_SEEN_CACHE_FILE);
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

    let host_file = shared_state_dir.join(DEEPLINK_HOST_CACHE_FILE);
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

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    bind_addr: SocketAddr,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    config: Config,
    mut config_updates: broadcast::Receiver<Config>,
    db: DbHandle,
    reload_tx: mpsc::Sender<()>,
    test_alert_tx: mpsc::Sender<()>,
) -> Result<()> {
    // The CORS policy is fixed when the router is built, so it keeps the startup config.
    let cors = cors_layer(&config);

    let state = ApiState {
        app_state,
        monitoring,
        config: Arc::new(RwLock::new(Arc::new(config))),
        db,
        reload_tx,
        test_alert_tx,
        deeplink_host_cache: Arc::new(Mutex::new(None)),
        last_seen_host_cache: Arc::new(Mutex::new(None)),
    };

    let live_config = state.config.clone();
    tokio::spawn(async move {
        loop {
            match config_updates.recv().await {
                Ok(mut update) => {
                    // A reload of a broken config.json falls back to the built-in defaults,
                    // whose credentials nobody can sign in with; keeping the running ones
                    // leaves a way back in to fix it.
                    if expected_token(&update).is_none() {
                        warn!(
                            "The reloaded configuration has no usable dashboard credentials; \
                             keeping the current ones."
                        );
                        let current = live_config.read().clone();
                        update.dashboard_username = current.dashboard_username.clone();
                        update.dashboard_password = current.dashboard_password.clone();
                    }
                    *live_config.write() = Arc::new(update);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let protected_router = Router::new()
        .route("/api/logs", get(logs_handler))
        .route("/api/status", get(status_handler))
        .route("/api/cap-status", get(cap_status_handler))
        .route("/api/components", get(components_handler))
        .route("/api/dashboard-config", get(dashboard_config_handler))
        .route("/api/alerts", get(alerts_handler))
        .route("/api/vacuum", post(vacuum_handler))
        .route(
            "/api/config",
            get(get_config_handler).put(put_config_handler),
        )
        .route("/api/config/schema", get(config_schema_handler))
        .route("/api/reload", post(reload_handler))
        .route("/api/test-alert", post(test_alert_handler))
        .route(
            "/api/alerts/latest-recording-id",
            get(latest_recording_id_handler),
        )
        .route("/api/same-us", get(same_us_lookup_handler))
        .route(
            "/api/notifications",
            get(notifications_get_handler).put(notifications_put_handler),
        )
        .route(
            "/api/notifications/services",
            get(notifications_services_handler),
        )
        .route("/api/notifications/test", post(notifications_test_handler))
        .route(
            "/api/uninstall",
            get(uninstall_get_handler).post(uninstall_post_handler),
        )
        .layer(cors.clone())
        .with_state(state.clone())
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    // The dashboard's own files sit behind the same session check as the API; only the login page
    // and what it needs to render are reachable signed out.
    let gated_static = Router::new()
        .fallback(web_assets::fallback)
        // `layer` rather than `route_layer`: everything here is reached through the fallback,
        // which a route layer would skip entirely.
        .layer(middleware::from_fn_with_state(state.clone(), page_auth));

    let static_router = Router::new()
        .route("/login.html", get(|| web_assets::named("login.html")))
        .route("/login.js", get(|| web_assets::named("login.js")))
        .route("/style.css", get(|| web_assets::named("style.css")))
        .route("/favicon.ico", get(|| web_assets::named("favicon.ico")))
        .route(
            "/site.webmanifest",
            get(|| web_assets::named("site.webmanifest")),
        )
        .fallback_service(gated_static);

    info!(source = %web_assets::describe(), "Serving the dashboard");

    let router = Router::new()
        .route("/api/health", get(health_handler))
        .route("/api/setup/status", get(setup_status_handler))
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        .route("/ws", get(ws_handler))
        // Authenticates itself so it can accept a query-string token from an <audio> element.
        .route("/api/recordings", get(recording_handler))
        .layer(cors)
        .merge(protected_router)
        .with_state(state.clone())
        .merge(static_router);

    let listener = bind_with_retry(bind_addr).await?;
    record_health_address(&listener);
    info!(%bind_addr, "Monitoring API listening");
    axum::serve(listener, router.into_make_service()).await?;
    Ok(())
}

pub(crate) async fn bind_with_retry(bind_addr: SocketAddr) -> std::io::Result<TcpListener> {
    let mut attempt = 1;
    loop {
        match TcpListener::bind(bind_addr).await {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && attempt < BIND_RETRY_ATTEMPTS =>
            {
                attempt += 1;
                time::sleep(BIND_RETRY_INTERVAL).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Set by the Docker image. Its HEALTHCHECK runs outside the entrypoint's environment, so it has
/// no way to know the port config.json chose; the server that answers `/api/health` -- the
/// dashboard or first-run setup -- writes where to reach it here instead. Called by those two
/// only: the alert stream binds a port of its own, and recording that would send the check to a
/// server with no `/api/health`.
const HEALTH_ADDR_FILE_VAR: &str = "EAS_HEALTH_ADDR_FILE";

pub(crate) fn record_health_address(listener: &TcpListener) {
    let Ok(bound) = listener.local_addr() else {
        return;
    };
    let Some(path) = std::env::var_os(HEALTH_ADDR_FILE_VAR).filter(|path| !path.is_empty()) else {
        return;
    };
    if let Err(err) = std::fs::write(&path, health_address(bound)) {
        warn!(
            "Could not write the healthcheck address to {:?}: {}",
            path, err
        );
    }
}

/// `host:port` a local client reaches `bound` on: loopback when bound to every interface.
fn health_address(bound: SocketAddr) -> String {
    let host = match bound.ip() {
        ip if ip.is_unspecified() && bound.is_ipv6() => "[::1]".to_string(),
        ip if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        std::net::IpAddr::V4(ip) => ip.to_string(),
    };
    format!("{host}:{}", bound.port())
}

pub(crate) async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "OK".to_string(),
    })
}

/// First-run setup answers this too, with `phase: "setup"`, which is how its page tells the
/// listener taking over apart from the setup server still winding down.
async fn setup_status_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "setup_required": false, "phase": "running" }))
}

async fn config_schema_handler() -> Json<serde_json::Value> {
    Json(crate::config_schema::payload())
}

async fn same_us_lookup_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    maybe_persist_deeplink_host(&headers, &state).await;
    Json(SAME_US_LOOKUP_JSON.clone())
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
    let (active_alerts, cap_status) = {
        let guard = state.app_state.lock().await;
        (
            guard.active_alerts.clone(),
            build_cap_status_payload(&guard.active_alerts, &guard.cap_status),
        )
    };
    Json(StatusResponse {
        streams,
        active_alerts,
        cap_status,
    })
}

async fn cap_status_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Json<CapStatusPayload> {
    maybe_persist_deeplink_host(&headers, &state).await;
    Json(cap_status_snapshot(&state).await)
}

#[derive(Debug, Deserialize, Default)]
struct AlertsQuery {
    max_alerts: Option<String>,
    filter_alerts: Option<String>,
}

async fn alerts_handler(
    State(state): State<ApiState>,
    Query(params): Query<AlertsQuery>,
    headers: HeaderMap,
) -> Response {
    maybe_persist_deeplink_host(&headers, &state).await;

    // "all" means no limit; anything unparseable falls back to the dashboard's own default.
    let limit = match params.max_alerts.as_deref().map(str::trim) {
        Some("all") => None,
        Some(value) if !value.is_empty() => Some(value.parse::<usize>().unwrap_or(50)),
        _ => Some(50),
    };

    let query = ArchiveQuery {
        limit,
        filter_watched_fips: params.filter_alerts.as_deref() == Some("watched_fips"),
    };

    match archive::archived_alerts(&state.db, &state.config(), &query).await {
        Ok(alerts) => Json(alerts).into_response(),
        Err(err) => {
            error!("Failed to read archived alerts: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read archived alerts",
            )
                .into_response()
        }
    }
}

async fn latest_recording_id_handler(State(state): State<ApiState>) -> Json<i64> {
    Json(archive::latest_recording_id(&state.config().recording_dir))
}

#[derive(Debug, Deserialize, Default)]
struct RecordingQuery {
    name: Option<String>,
    id: Option<usize>,
    /// Audio elements cannot send an Authorization header, so this endpoint also accepts the
    /// token in the query string, the same way /ws already does.
    auth: Option<String>,
}

async fn recording_handler(
    State(state): State<ApiState>,
    Query(params): Query<RecordingQuery>,
    headers: HeaderMap,
) -> Response {
    let config = state.config();
    if !request_is_authorized(&headers, params.auth.as_deref(), &config) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let recording_dir = &config.recording_dir;
    let file = match (params.name.as_deref(), params.id) {
        (Some(name), _) => archive::resolve_recording_name(recording_dir, name),
        (None, Some(id)) => archive::resolve_recording_id(recording_dir, id),
        (None, None) => None,
    };

    let Some(file) = file else {
        return (StatusCode::NOT_FOUND, "File not found.").into_response();
    };

    if !archive::is_finalized_recording(&file) {
        return (StatusCode::TOO_EARLY, "Recording is still in progress.").into_response();
    }

    let bytes = match tokio::fs::read(&file).await {
        Ok(bytes) => bytes,
        Err(err) => {
            archive::warn_unreadable(&file, &err);
            return (StatusCode::NOT_FOUND, "File not found.").into_response();
        }
    };

    let total = bytes.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_byte_range(value, total));

    let file_name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recording");

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(archive::content_type_for(&file)),
    );
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(disposition) = HeaderValue::from_str(&format!("inline; filename=\"{file_name}\"")) {
        response_headers.insert(header::CONTENT_DISPOSITION, disposition);
    }

    match range {
        Some(Err(())) => {
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
                response_headers.insert(header::CONTENT_RANGE, value);
            }
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                response_headers,
                Vec::new(),
            )
                .into_response()
        }
        Some(Ok((start, end))) => {
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
                response_headers.insert(header::CONTENT_RANGE, value);
            }
            let slice = bytes[start as usize..=end as usize].to_vec();
            (StatusCode::PARTIAL_CONTENT, response_headers, slice).into_response()
        }
        None => (StatusCode::OK, response_headers, bytes).into_response(),
    }
}

/// Parses a single `bytes=` range. `Some(Err(()))` marks a syntactically valid but unsatisfiable
/// range, which owes the client a 416 rather than the whole file.
fn parse_byte_range(value: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }

    let (raw_start, raw_end) = spec.split_once('-')?;
    let raw_start = raw_start.trim();
    let raw_end = raw_end.trim();

    if total == 0 {
        return Some(Err(()));
    }
    let last = total - 1;

    let (start, end) = match (raw_start.is_empty(), raw_end.is_empty()) {
        // "bytes=-N" asks for the final N bytes.
        (true, false) => {
            let suffix: u64 = match raw_end.parse() {
                Ok(value) => value,
                Err(_) => return Some(Err(())),
            };
            if suffix == 0 {
                return Some(Err(()));
            }
            (total.saturating_sub(suffix), last)
        }
        (false, true) => {
            let start: u64 = match raw_start.parse() {
                Ok(value) => value,
                Err(_) => return Some(Err(())),
            };
            (start, last)
        }
        (false, false) => {
            let start: u64 = match raw_start.parse() {
                Ok(value) => value,
                Err(_) => return Some(Err(())),
            };
            let end: u64 = match raw_end.parse() {
                Ok(value) => value,
                Err(_) => return Some(Err(())),
            };
            (start, end.min(last))
        }
        (true, true) => return Some(Err(())),
    };

    if start > end || start > last {
        return Some(Err(()));
    }

    Some(Ok((start, end)))
}

async fn vacuum_handler(State(state): State<ApiState>) -> Response {
    match archive::vacuum(&state.db, &state.config()).await {
        Ok(report) => {
            info!(
                "Vacuum complete: {} alerts deleted, {} recordings archived to {}, {} kept.",
                report.alerts_deleted,
                report.recordings_archived,
                report.archive_dir,
                report.recordings_kept
            );
            Json(report).into_response()
        }
        Err(err) => {
            error!("Vacuum failed: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Vacuum failed").into_response()
        }
    }
}

/// The live config.json, verbatim. Only a signed-in dashboard user reaches this, and they
/// authenticated with the very credentials it contains.
async fn get_config_handler() -> Response {
    let path = crate::paths::config_json();
    match std::fs::read_to_string(&path) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (
            [(header::CONTENT_TYPE, "application/json")],
            "{}".to_string(),
        )
            .into_response(),
        Err(err) => {
            error!("Could not read {}: {}", path.display(), err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read the configuration file.",
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct ConfigWriteQuery {
    /// Validate only, leaving the file alone.
    dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ConfigWriteResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<String>,
    /// True when the sign-in credentials changed, which invalidates the current session.
    credentials_changed: bool,
    dry_run: bool,
}

pub(crate) enum ConfigRejection {
    /// The candidate is wrong, and the message says how.
    Invalid(String),
    /// Something on this side failed; the details are in the log.
    Internal(&'static str),
}

impl ConfigRejection {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            ConfigRejection::Invalid(_) => StatusCode::BAD_REQUEST,
            ConfigRejection::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            ConfigRejection::Invalid(message) => message.clone(),
            ConfigRejection::Internal(message) => message.to_string(),
        }
    }
}

/// Loads a candidate config.json exactly the way startup does, so the editor's rules can never
/// drift from the ones that matter. It goes to a temporary file rather than over the live one,
/// which stays untouched until the candidate is known good.
pub(crate) fn check_candidate(body: &str) -> Result<Config, ConfigRejection> {
    let temp = tempfile::NamedTempFile::new().map_err(|err| {
        error!(
            "Could not create a temporary file to validate config: {}",
            err
        );
        ConfigRejection::Internal("Validation failed.")
    })?;
    std::fs::write(temp.path(), body.as_bytes()).map_err(|err| {
        error!("Could not stage the candidate config: {}", err);
        ConfigRejection::Internal("Validation failed.")
    })?;

    let candidate = Config::from_config_json(temp.path()).map_err(|err| {
        // The chain carries the useful part, e.g. which key is wrong. The staging file's name is
        // an implementation detail, so it is not shown to the user.
        ConfigRejection::Invalid(
            format!("{err:#}").replace(&temp.path().display().to_string(), "config.json"),
        )
    })?;

    // Startup would accept these, and then nobody could sign in to change them back.
    if expected_token(&candidate).is_none() {
        return Err(ConfigRejection::Invalid(
            "DASHBOARD_USERNAME and DASHBOARD_PASSWORD must both be set, to something other than \
             admin and password -- the dashboard refuses those, so nobody could sign in."
                .to_string(),
        ));
    }

    Ok(candidate)
}

/// Replaces config.json, first copying the outgoing version to config.json.bak so a bad edit is
/// recoverable by hand. Returns the backup's path when there was a file to back up.
pub(crate) fn write_config(body: &str) -> Result<Option<String>, ConfigRejection> {
    let path = crate::paths::config_json();

    // paths::config_json already looks inside a config.json directory, so this is a directory
    // nested inside that one, which nothing sensible creates.
    if path.is_dir() {
        return Err(ConfigRejection::Invalid(format!(
            "{} is a directory, so the configuration cannot be written there.",
            path.display()
        )));
    }

    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    let backup = std::path::PathBuf::from(backup);

    let backup_path = match std::fs::copy(&path, &backup) {
        Ok(_) => Some(backup.to_string_lossy().into_owned()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            error!("Could not back up {}: {}", path.display(), err);
            return Err(ConfigRejection::Internal(
                "Could not back up the existing configuration; nothing was written.",
            ));
        }
    };

    write_atomic(&path, body.as_bytes()).map_err(|err| {
        error!("Could not write {}: {}", path.display(), err);
        ConfigRejection::Internal("Could not write the configuration file.")
    })?;

    Ok(backup_path)
}

async fn put_config_handler(
    State(state): State<ApiState>,
    Query(params): Query<ConfigWriteQuery>,
    body: String,
) -> Response {
    let dry_run = params.dry_run.unwrap_or(false);

    let rejected = |rejection: ConfigRejection| {
        (
            rejection.status(),
            Json(ConfigWriteResult {
                ok: false,
                error: Some(rejection.message()),
                backup_path: None,
                credentials_changed: false,
                dry_run,
            }),
        )
            .into_response()
    };

    let candidate = match check_candidate(&body) {
        Ok(candidate) => candidate,
        Err(rejection) => return rejected(rejection),
    };

    let running = state.config();
    let credentials_changed = candidate.dashboard_username != running.dashboard_username
        || candidate.dashboard_password != running.dashboard_password;

    if dry_run {
        return Json(ConfigWriteResult {
            ok: true,
            error: None,
            backup_path: None,
            credentials_changed,
            dry_run,
        })
        .into_response();
    }

    let backup_path = match write_config(&body) {
        Ok(backup_path) => backup_path,
        Err(rejection) => return rejected(rejection),
    };

    info!("Configuration saved from the dashboard.");
    Json(ConfigWriteResult {
        ok: true,
        error: None,
        backup_path,
        credentials_changed,
        dry_run,
    })
    .into_response()
}

pub(crate) fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);

    std::fs::write(&tmp, contents)?;
    if std::fs::rename(&tmp, path).is_err() {
        // A file bind-mounted into a container is a mount point, which rename() cannot replace
        // (EBUSY), so Docker's config.json can only be rewritten in place.
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn notifications_path(state: &ApiState) -> std::path::PathBuf {
    crate::notifications::resolve(&state.config().apprise_config_path)
}

async fn notifications_get_handler(State(state): State<ApiState>) -> Response {
    crate::notifications::api::view(&notifications_path(&state))
}

/// Always the running configuration's file: a path from the page would let it write anywhere.
async fn notifications_put_handler(
    State(state): State<ApiState>,
    Json(body): Json<crate::notifications::api::SaveBody>,
) -> Response {
    crate::notifications::api::save(&notifications_path(&state), &body.targets())
}

async fn notifications_services_handler() -> Response {
    crate::notifications::api::services_response().await
}

async fn notifications_test_handler(
    State(state): State<ApiState>,
    Json(body): Json<crate::notifications::api::TestBody>,
) -> Response {
    crate::notifications::api::test_response(&body.url, &state.config().eas_relay_name).await
}

async fn uninstall_get_handler() -> Response {
    Json(crate::uninstall::plan()).into_response()
}

#[derive(Debug, serde::Deserialize)]
struct UninstallBody {
    confirm: String,
    #[serde(default)]
    delete_data: bool,
    #[serde(default)]
    with_program: bool,
}

/// Hands the uninstall to a process of its own and, unless removing the service is what stops
/// this one, exits once the answer has had time to reach the page.
async fn uninstall_post_handler(Json(body): Json<UninstallBody>) -> Response {
    match crate::uninstall::start_from_dashboard(&body.confirm, body.delete_data, body.with_program) {
        Ok((log, stop_this_process)) => {
            warn!(
                "Uninstalling this instance from the dashboard; the helper's output is in {}",
                log.display()
            );
            if stop_this_process {
                tokio::spawn(async {
                    time::sleep(Duration::from_secs(1)).await;
                    std::process::exit(0);
                });
            }
            Json(serde_json::json!({ "ok": true, "log": log.display().to_string() }))
                .into_response()
        }
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": error })),
        )
            .into_response(),
    }
}

async fn reload_handler(State(state): State<ApiState>) -> Response {
    dispatch(&state.reload_tx, "reload").await
}

async fn test_alert_handler(State(state): State<ApiState>) -> Response {
    dispatch(&state.test_alert_tx, "test alert").await
}

/// Hands a request to the task that services it. A full channel means one is already queued, which
/// is success as far as the caller is concerned; a closed channel means that task is gone.
async fn dispatch(sender: &mpsc::Sender<()>, what: &str) -> Response {
    match sender.try_send(()) {
        Ok(_) => {
            info!("Dashboard requested {}.", what);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            info!(
                "Dashboard requested {} while one was already pending.",
                what
            );
            Json(serde_json::json!({ "ok": true, "already_pending": true })).into_response()
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            error!("The {} handler is no longer running.", what);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("The {what} handler is not running."),
            )
                .into_response()
        }
    }
}

/// Everything the dashboard used to get from PHP interpolating into the page. Built as an explicit
/// allowlist rather than by filtering the config, so a new credential field cannot leak into it.
#[derive(Debug, Serialize)]
struct DashboardConfig {
    version: &'static str,
    monitoring_max_logs: usize,
    alert_sound_enabled: bool,
    alert_sound_src: String,
    icecast_stream_url_mapping: serde_json::Value,
    watched_fips: Vec<String>,
    timezone: String,
    icecast_alert_stream_enabled: bool,
    icecast_alert_public_url: String,
    deprecation_notice: String,
    tts_engine_fallback_reason: String,
}

/// Runtime image metadata the Docker entrypoint writes on every boot.
fn image_info() -> serde_json::Value {
    std::fs::read_to_string(crate::paths::in_install_root("image_info.json"))
        .ok()
        .and_then(|payload| serde_json::from_str(&payload).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn image_info_string(info: &serde_json::Value, key: &str) -> String {
    info.get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string()
}

async fn dashboard_config_handler(State(state): State<ApiState>) -> Json<DashboardConfig> {
    let config = state.config();
    let info = image_info();

    let mut watched_fips: Vec<String> = config.watched_fips.iter().cloned().collect();
    watched_fips.sort();

    // The stream nickname map has no typed home in Config; it is passed through from config.json.
    let icecast_stream_url_mapping = crate::paths::config_json()
        .to_str()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|raw| raw.get("ICECAST_STREAM_URL_MAPPING").cloned())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let raw_config = crate::paths::config_json()
        .to_str()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());

    let alert_sound_enabled = raw_config
        .as_ref()
        .and_then(|raw| raw.get("ALERT_SOUND_ENABLED"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let alert_sound_src = raw_config
        .as_ref()
        .and_then(|raw| raw.get("ALERT_SOUND_SRC"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("iembot.mp3")
        .to_string();

    Json(DashboardConfig {
        version: env!("CARGO_PKG_VERSION"),
        monitoring_max_logs: config.monitoring_max_log_entries,
        alert_sound_enabled,
        alert_sound_src,
        icecast_stream_url_mapping,
        watched_fips,
        timezone: config.timezone.name().to_string(),
        icecast_alert_stream_enabled: config.icecast_alert_stream_enabled,
        icecast_alert_public_url: config.icecast_alert_public_url.clone(),
        deprecation_notice: image_info_string(&info, "deprecation_notice"),
        tts_engine_fallback_reason: image_info_string(&info, "tts_engine_fallback_reason"),
    })
}

#[derive(Serialize)]
struct ComponentsPayload {
    tools_dir: String,
    components: Vec<crate::components::ComponentStatus>,
}

async fn components_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Json<ComponentsPayload> {
    maybe_persist_deeplink_host(&headers, &state).await;
    Json(ComponentsPayload {
        tools_dir: crate::paths::tools_dir().to_string_lossy().into_owned(),
        components: crate::components::last_probe(),
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ApiState>,
    Query(params): Query<Params>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !request_is_authorized(&headers, params.auth.as_deref(), &state.config()) {
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
                        let should_send_cap_status = matches!(event, MonitoringEvent::Alerts(_));
                        let message: WsMessage = event.into();
                        if let Err(err) = send_ws_message(&mut socket, &message).await {
                            error!("Failed to send monitoring event: {err}");
                            break;
                        }
                        if should_send_cap_status {
                            if let Err(err) = send_cap_status_update(&mut socket, &state).await {
                                error!("Failed to send CAP status update: {err}");
                                break;
                            }
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
                if let Err(err) = send_cap_status_update(&mut socket, &state).await {
                    error!("Failed to send CAP status heartbeat update: {err}");
                    break;
                }
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
    let (active_alerts, cap_status) = {
        let guard = state.app_state.lock().await;
        (
            guard.active_alerts.clone(),
            build_cap_status_payload(&guard.active_alerts, &guard.cap_status),
        )
    };
    let snapshot = WsMessage::Snapshot(SnapshotPayload {
        streams,
        active_alerts,
        cap_status,
        logs,
    });
    send_ws_message(socket, &snapshot).await
}

async fn send_cap_status_update(socket: &mut WebSocket, state: &ApiState) -> Result<()> {
    let status = cap_status_snapshot(state).await;
    send_ws_message(socket, &WsMessage::CapStatus(status)).await
}

async fn cap_status_snapshot(state: &ApiState) -> CapStatusPayload {
    let guard = state.app_state.lock().await;
    build_cap_status_payload(&guard.active_alerts, &guard.cap_status)
}

fn build_cap_status_payload(
    active_alerts: &[ActiveAlert],
    runtime: &CapRuntimeStatus,
) -> CapStatusPayload {
    let active_cap_alerts = active_alerts
        .iter()
        .filter(|alert| crate::cap::is_cap_raw_header(&alert.raw_header))
        .count();

    CapStatusPayload {
        active_alerts: active_cap_alerts,
        runtime: runtime.clone(),
    }
}

async fn send_ws_message(socket: &mut WebSocket, message: &WsMessage) -> Result<()> {
    let payload = serde_json::to_string(message)?;
    socket.send(Message::Text(payload)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EasAlertData;

    fn sample_config(username: &str, password: &str) -> Config {
        let mut cfg = Config::safe_internal_defaults();
        cfg.dashboard_username = username.to_string();
        cfg.dashboard_password = password.to_string();
        cfg
    }

    #[test]
    fn the_healthcheck_reaches_whatever_address_was_bound() {
        for (bound, expected) in [
            ("0.0.0.0:8080", "127.0.0.1:8080"),
            ("127.0.0.1:18099", "127.0.0.1:18099"),
            ("192.168.1.20:8080", "192.168.1.20:8080"),
            ("[::]:8080", "[::1]:8080"),
            ("[::1]:9000", "[::1]:9000"),
        ] {
            let bound: SocketAddr = bound.parse().expect("socket address");
            assert_eq!(health_address(bound), expected, "{bound}");
        }
    }

    fn headers_with(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn default_credentials_are_never_accepted() {
        for (user, pass) in [
            ("admin", "hunter2"),
            ("wags", "password"),
            ("", "hunter2"),
            ("wags", ""),
        ] {
            let cfg = sample_config(user, pass);
            assert!(
                expected_token(&cfg).is_none(),
                "credentials {user:?}/{pass:?} should be refused"
            );
            assert!(!request_is_authorized(&HeaderMap::new(), Some("any"), &cfg));
        }
    }

    #[test]
    fn a_valid_token_is_accepted_from_header_cookie_or_query() {
        let cfg = sample_config("wags", "hunter2");
        let token = expected_token(&cfg).expect("token");

        let via_header = headers_with(&[(header::AUTHORIZATION, &format!("Bearer {token}"))]);
        assert!(request_is_authorized(&via_header, None, &cfg));

        let via_cookie = headers_with(&[(header::COOKIE, &format!("{SESSION_COOKIE}={token}"))]);
        assert!(request_is_authorized(&via_cookie, None, &cfg));

        assert!(request_is_authorized(&HeaderMap::new(), Some(&token), &cfg));
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        let cfg = sample_config("wags", "hunter2");

        assert!(!request_is_authorized(&HeaderMap::new(), None, &cfg));
        assert!(!request_is_authorized(
            &HeaderMap::new(),
            Some("nope"),
            &cfg
        ));

        let bad_header = headers_with(&[(header::AUTHORIZATION, "Bearer nope")]);
        assert!(!request_is_authorized(&bad_header, None, &cfg));

        // A token without the Bearer prefix is not a token.
        let token = expected_token(&cfg).expect("token");
        let unprefixed = headers_with(&[(header::AUTHORIZATION, &token)]);
        assert!(!request_is_authorized(&unprefixed, None, &cfg));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let cfg = sample_config("wags", "hunter2");
        let token = expected_token(&cfg).expect("token");

        let headers = headers_with(&[(
            header::COOKIE,
            &format!("theme=dark; {SESSION_COOKIE}={token}; other=1"),
        )]);
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE).as_deref(),
            Some(token.as_str())
        );
        assert!(request_is_authorized(&headers, None, &cfg));
        assert!(cookie_value(&headers, "nonexistent").is_none());
    }

    #[test]
    fn the_session_cookie_is_http_only_and_only_secure_behind_a_proxy() {
        let mut cfg = sample_config("wags", "hunter2");

        let plain = session_cookie("tok", &cfg);
        assert!(plain.contains("HttpOnly"));
        assert!(plain.contains("SameSite=Lax"));
        assert!(plain.contains("Path=/"));
        // A plain-HTTP LAN dashboard would never get a Secure cookie back.
        assert!(!plain.contains("Secure"));

        cfg.use_reverse_proxy = true;
        assert!(session_cookie("tok", &cfg).contains("; Secure"));
    }

    #[test]
    fn token_comparison_rejects_length_mismatches() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abcd"));
        assert!(!tokens_match("abcd", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(tokens_match("", ""));
    }

    fn make_alert(raw_header: &str) -> ActiveAlert {
        let data = EasAlertData {
            eas_text: "sample".to_string(),
            event_text: "Sample Event".to_string(),
            event_code: "TOR".to_string(),
            fips: vec!["031055".to_string()],
            locations: "Douglas County".to_string(),
            originator: "WXR".to_string(),
            description: None,
            instructions: None,
            parsed_header: None,
        };
        ActiveAlert::new(data, raw_header.to_string(), Duration::from_secs(120))
    }

    #[test]
    fn token_validation_rejects_default_and_accepts_matching_bearer() {
        let default_cfg = sample_config("admin", "password");
        let bearer = headers_with(&[(header::AUTHORIZATION, "Bearer abc")]);
        assert!(!request_is_authorized(&bearer, None, &default_cfg));

        let cfg = sample_config("alice", "s3cret");
        let basic = headers_with(&[(header::AUTHORIZATION, "Basic abc")]);
        assert!(!request_is_authorized(&basic, None, &cfg));

        let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
        let good = headers_with(&[(header::AUTHORIZATION, &format!("Bearer {expected}"))]);
        assert!(request_is_authorized(&good, None, &cfg));

        let wrong = headers_with(&[(header::AUTHORIZATION, "Bearer wrong")]);
        assert!(!request_is_authorized(&wrong, None, &cfg));
    }

    #[test]
    fn sanitize_host_header_handles_ports_ipv6_and_lists() {
        assert_eq!(
            sanitize_host_header("example.com:8080"),
            Some("example.com".to_string())
        );
        assert_eq!(
            sanitize_host_header("[2001:db8::1]:443"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(
            sanitize_host_header("proxy.example.com, origin.example.com"),
            Some("proxy.example.com".to_string())
        );
        assert_eq!(sanitize_host_header("  "), None);
    }

    #[test]
    fn extract_deeplink_host_candidate_prefers_forwarded_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            "edge.example.net:443".parse().expect("header"),
        );
        headers.insert("host", "fallback.local:8080".parse().expect("header"));

        assert_eq!(
            extract_deeplink_host_candidate(&headers),
            Some("edge.example.net".to_string())
        );

        headers.remove("x-forwarded-host");
        assert_eq!(
            extract_deeplink_host_candidate(&headers),
            Some("fallback.local".to_string())
        );
    }

    #[test]
    fn loopback_detection_and_cap_status_payload_work() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("example.com"));

        // Every CAP feed counts; CAP-CP used to be missed because the check looked for "IPAWS".
        let alerts = vec![
            make_alert("ZCZC-WXR-TOR-031055+0030-1231645-IPAWSCAP-"),
            make_alert("ZCZC-WXR-TOR-031055+0030-1231645-IPAWSWEA-"),
            make_alert("ZCZC-WXR-TOR-043100+0030-1231645-NAADSCAP-"),
            make_alert("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-"),
        ];
        let runtime = CapRuntimeStatus::default();
        let payload = build_cap_status_payload(&alerts, &runtime);
        assert_eq!(payload.active_alerts, 3);
    }
}
