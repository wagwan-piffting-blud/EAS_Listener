//! First-run setup: what runs instead of the listener while there is no configuration yet.
//!
//! With no config.json -- or an empty one, or the empty directory Docker mounts in its place --
//! the listener would otherwise start on built-in defaults nobody chose, with credentials nobody
//! can sign in with. Instead only a small web server runs, serving the setup page, until a
//! configuration is saved. Whoever reaches the port first could otherwise claim the install, so
//! every setup request needs a token that is only printed to the console and written next to
//! config.json. Where there is a desktop to show it on, the page is opened in the default browser.

use crate::autostart::{self, Availability};
use crate::backend;
use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, Notify};

/// Set by the Docker entrypoint, which reads config.json itself before starting the listener --
/// choosing and fetching the TTS engine -- so a newly saved one is applied by exiting with
/// `RESTART_EXIT_CODE` and letting the entrypoint run again, not by carrying on in this process.
const RESTART_VAR: &str = "EAS_RESTART_AFTER_SETUP";
pub const RESTART_EXIT_CODE: i32 = 75;
const NO_BROWSER_VAR: &str = "EAS_NO_BROWSER";
static BROWSER_DISABLED: AtomicBool = AtomicBool::new(false);
const TOKEN_FILE: &str = "setup-token.txt";
const TOKEN_HEADER: &str = "x-setup-token";
/// Everything the setup page loads. Any other page redirects to it.
pub(crate) const PAGE_FILES: &[&str] = &[
    "setup.html",
    "setup.js",
    "config-form.js",
    "config-form.css",
    "notifications.js",
    "style.css",
    "favicon.ico",
    "site.webmanifest",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFileState {
    Present,
    Missing,
    Empty,
}

impl ConfigFileState {
    pub fn needs_setup(self) -> bool {
        self != ConfigFileState::Present
    }
}

/// `path` is where `paths::config_json` resolved to, so a config.json directory has already been
/// looked inside.
pub fn config_file_state(path: &Path) -> ConfigFileState {
    match std::fs::metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfigFileState::Missing,
        // Unreadable for any other reason: the normal load refuses it, and says why.
        Err(_) => ConfigFileState::Present,
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(text) if is_blank(&text) => ConfigFileState::Empty,
            _ => ConfigFileState::Present,
        },
    }
}

/// Empty, or an object holding nothing but `_comment`-style keys. Invalid JSON does not count:
/// that is somebody's configuration with a typo in it, which setup must never overwrite.
fn is_blank(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    matches!(
        serde_json::from_str::<Value>(trimmed),
        Ok(Value::Object(map)) if map.keys().all(|key| key.starts_with('_'))
    )
}

pub fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("the OS random number source is unavailable");
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    })
}

fn token_file() -> PathBuf {
    crate::paths::in_app_root(TOKEN_FILE)
}

fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub fn restart_after_setup() -> bool {
    env_flag(RESTART_VAR)
}

/// For a process with nobody at a desktop to see a browser: the Windows service, `--no-browser`.
pub fn disable_browser_launch() {
    BROWSER_DISABLED.store(true, Ordering::Relaxed);
}

/// Whether this process belongs to someone's desktop, where a browser or a tray icon can appear.
/// Linux needs a display; macOS asks launchd, since an SSH login or a daemon has none. Windows is
/// always a desktop here: its service never gets this far.
pub fn desktop_session() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::launchd::in_desktop_session()
    }
    #[cfg(not(target_os = "macos"))]
    {
        cfg!(windows)
            || ["DISPLAY", "WAYLAND_DISPLAY"]
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|value| !value.trim().is_empty()))
    }
}

/// Without a desktop, xdg-open falls back to a text browser, which would take over an SSH
/// session's terminal -- and a container, a systemd unit or a launchd daemon has no desktop.
fn browser_available(disabled: bool, desktop: bool) -> bool {
    !disabled && cfg!(any(windows, target_os = "linux", target_os = "macos")) && desktop
}

fn open_in_browser(url: &str) {
    let disabled = BROWSER_DISABLED.load(Ordering::Relaxed) || env_flag(NO_BROWSER_VAR);
    if !browser_available(disabled, desktop_session()) {
        return;
    }

    match open::that_detached(url) {
        Ok(()) => println!("Opened the setup page in your default browser."),
        Err(err) => println!("Could not open a browser ({err}), so open the address above."),
    }
}

/// A wildcard bind is not an address a browser can open, so it is shown as loopback.
pub fn setup_url(bind_addr: SocketAddr) -> String {
    let reachable = if bind_addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], bind_addr.port()))
    } else {
        bind_addr
    };
    format!("http://{reachable}/setup.html?token={}", token())
}

#[derive(Clone)]
struct SetupState {
    done: Arc<Notify>,
    /// One save at a time, so two browsers cannot both pass the "still unconfigured" check.
    saving: Arc<Mutex<()>>,
    /// Set when setup installed the listener as a service, which is then already starting.
    handed_to_service: Arc<AtomicBool>,
}

/// Who runs the listener once setup is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handover {
    /// This process, carrying on.
    ThisProcess,
    /// The service setup just installed and started, so this process has nothing left to do.
    Service,
}

/// Serves the setup page on `bind_addr` until a configuration has been saved.
pub async fn run(bind_addr: SocketAddr) -> Result<Handover> {
    let state = SetupState {
        done: Arc::new(Notify::new()),
        saving: Arc::new(Mutex::new(())),
        handed_to_service: Arc::new(AtomicBool::new(false)),
    };
    let done = state.done.clone();
    let handed_to_service = state.handed_to_service.clone();

    let listener = backend::bind_with_retry(bind_addr)
        .await
        .with_context(|| format!("Could not listen on {bind_addr} for first-run setup"))?;
    backend::record_health_address(&listener);

    let token_file = token_file();
    let token_note = match std::fs::write(&token_file, format!("{}\n", token())) {
        Ok(()) => format!("It is also in {}", token_file.display()),
        Err(err) => format!("(Could not write it to {}: {err})", token_file.display()),
    };

    let rule = "=".repeat(78);
    println!("{rule}");
    println!("EAS Listener has no configuration yet, so it is waiting to be set up.");
    println!("Finish setting it up in a browser:");
    println!();
    println!("    {}", setup_url(bind_addr));
    println!();
    println!("Setup token: {}", token());
    println!("{token_note}");
    if bind_addr.ip().is_unspecified() {
        println!(
            "The dashboard listens on every interface, so from another machine replace \
             127.0.0.1 with this machine's address (or the Docker host's)."
        );
    }
    if crate::paths::config_json_is_directory() {
        println!(
            "config.json is a directory (Docker makes one when the file it mounts does not \
             exist yet), so the configuration will be saved inside it, at {}.",
            crate::paths::config_json().display()
        );
    }
    println!("{rule}");
    open_in_browser(&setup_url(bind_addr));

    // The peer address is what tells a page on this machine, which can answer a Windows consent
    // prompt, from one on another.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { done.notified().await })
    .await
    .context("The first-run setup server failed")?;

    let _ = std::fs::remove_file(&token_file);
    Ok(if handed_to_service.load(Ordering::SeqCst) {
        Handover::Service
    } else {
        Handover::ThisProcess
    })
}

fn router(state: SetupState) -> Router {
    let gated = Router::new()
        .route("/api/config/schema", get(schema_handler))
        .route("/api/same-us", get(same_us_handler))
        .route("/api/setup/autostart", get(autostart_handler))
        .route("/api/setup/config", put(save_handler))
        .route(
            "/api/setup/notifications",
            get(notifications_get_handler).put(notifications_put_handler),
        )
        .route(
            "/api/setup/notifications/services",
            get(crate::notifications::api::services_response),
        )
        .route(
            "/api/setup/notifications/test",
            post(notifications_test_handler),
        )
        .route_layer(middleware::from_fn(require_token))
        .with_state(state);

    let mut router = Router::new()
        .route("/api/health", get(backend::health_handler))
        .route("/api/setup/status", get(status_handler))
        .merge(gated)
        .route("/assets/*path", get(crate::web_assets::fallback));
    for file in PAGE_FILES {
        router = router.route(
            &format!("/{file}"),
            get(move || crate::web_assets::named(file)),
        );
    }
    router.fallback(fallback_handler)
}

async fn require_token(req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);

    if presented.is_some_and(|candidate| backend::tokens_match(candidate, token())) {
        return next.run(req).await;
    }
    rejected(
        StatusCode::UNAUTHORIZED,
        "The setup token is missing or wrong.".to_string(),
    )
}

fn rejected(status: StatusCode, error: String) -> Response {
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

async fn status_handler() -> Json<Value> {
    let path = crate::paths::config_json();
    let state = config_file_state(&path);
    Json(json!({
        "phase": "setup",
        "setup_required": state.needs_setup(),
        "config_state": state,
        "config_path": path.display().to_string(),
        "config_json_is_directory": crate::paths::config_json_is_directory(),
        "token_file": token_file().display().to_string(),
    }))
}

async fn schema_handler() -> Json<Value> {
    Json(crate::config_schema::payload())
}

async fn same_us_handler() -> Json<Value> {
    Json(backend::SAME_US_LOOKUP_JSON.clone())
}

/// Behind the token: a manual command names where the listener is installed.
async fn autostart_handler(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Json<Availability> {
    Json(availability_for(peer).await)
}

/// A dual-stack socket reports an IPv4 peer as `::ffff:a.b.c.d`, which is not IPv6 loopback.
fn is_local_peer(peer: SocketAddr) -> bool {
    match peer.ip() {
        std::net::IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or(ip.is_loopback(), |mapped| mapped.is_loopback()),
        ip => ip.is_loopback(),
    }
}

/// `sc.exe`, `net.exe` and a few file checks: quick, but blocking.
async fn availability_for(peer: SocketAddr) -> Availability {
    let local = is_local_peer(peer);
    tokio::task::spawn_blocking(move || autostart::availability(local))
        .await
        .unwrap_or_else(|err| Availability::Unavailable {
            reason: format!("Could not check: {err}"),
        })
}

/// Installs and starts the service after config.json is saved. Checked again here rather than
/// trusted from the page, which may be stale.
async fn install_service(peer: SocketAddr) -> Value {
    if !matches!(availability_for(peer).await, Availability::Automatic { .. }) {
        return json!({
            "installed": false,
            "error": "Starting at boot cannot be set up from this page any more.",
        });
    }
    match tokio::task::spawn_blocking(autostart::install_and_start).await {
        Ok(Ok(())) => json!({ "installed": true }),
        Ok(Err(err)) => json!({ "installed": false, "error": format!("{err:#}") }),
        Err(err) => json!({ "installed": false, "error": err.to_string() }),
    }
}

#[derive(Debug, Deserialize, Default)]
struct SaveQuery {
    dry_run: Option<bool>,
    /// `install` also installs the listener as a service and hands over to it.
    service: Option<String>,
}

async fn save_handler(
    State(state): State<SetupState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(params): Query<SaveQuery>,
    body: String,
) -> Response {
    let dry_run = params.dry_run.unwrap_or(false);
    let _saving = state.saving.lock().await;

    let path = crate::paths::config_json();
    if !config_file_state(&path).needs_setup() {
        return rejected(
            StatusCode::CONFLICT,
            "config.json has already been written. Reload this page.".to_string(),
        );
    }
    if is_blank(&body) {
        return rejected(
            StatusCode::BAD_REQUEST,
            "There is nothing to save yet.".to_string(),
        );
    }

    let candidate = match backend::check_candidate(&body) {
        Ok(candidate) => candidate,
        Err(rejection) => return rejected(rejection.status(), rejection.message()),
    };

    if dry_run {
        return Json(json!({ "ok": true, "dry_run": true })).into_response();
    }

    if let Err(rejection) = backend::write_config(&body) {
        return rejected(rejection.status(), rejection.message());
    }

    // config.json is already saved, so a service that fails to install costs nothing: this
    // process starts the listener as it would have anyway, and the page says why.
    let service = if params.service.as_deref() == Some("install") {
        let outcome = install_service(peer).await;
        if outcome["installed"] == true {
            state.handed_to_service.store(true, Ordering::SeqCst);
        }
        Some(outcome)
    } else {
        None
    };

    if state.handed_to_service.load(Ordering::SeqCst) {
        println!(
            "First-run setup saved {} and installed the listener as a service, which is \
             starting. This process is done.",
            path.display()
        );
    } else {
        println!(
            "First-run setup saved {}. Starting the listener.",
            path.display()
        );
    }
    state.done.notify_one();

    Json(json!({
        "ok": true,
        "dry_run": false,
        "restarting": restart_after_setup(),
        "port": candidate.monitoring_bind_port,
        "service": service,
    }))
    .into_response()
}

/// What an existing apprise.yml already holds -- a Docker volume can carry one into a new install.
async fn notifications_get_handler() -> Response {
    crate::notifications::api::view(&crate::notifications::resolve(""))
}

/// Saved just before config.json, to the APPRISE_CONFIG_PATH that configuration names. The token
/// that allows this also allows writing config.json, which could name any path anyway.
async fn notifications_put_handler(
    State(state): State<SetupState>,
    Json(body): Json<crate::notifications::api::SaveBody>,
) -> Response {
    let _saving = state.saving.lock().await;
    if !config_file_state(&crate::paths::config_json()).needs_setup() {
        return rejected(
            StatusCode::CONFLICT,
            "config.json has already been written. Reload this page.".to_string(),
        );
    }
    let path = crate::notifications::resolve(body.path.as_deref().unwrap_or_default());
    crate::notifications::api::save(&path, &body.urls)
}

async fn notifications_test_handler(
    Json(body): Json<crate::notifications::api::TestBody>,
) -> Response {
    crate::notifications::api::test_response(&body.url, "").await
}

/// A stale dashboard tab gets a clear error from the API instead of the setup page's HTML.
async fn fallback_handler(uri: Uri) -> Response {
    if uri.path().starts_with("/api/") || uri.path() == "/ws" {
        return rejected(
            StatusCode::SERVICE_UNAVAILABLE,
            "The listener is waiting for first-run setup.".to_string(),
        );
    }

    let location = match uri.query() {
        Some(query) => format!("/setup.html?{query}"),
        None => "/setup.html".to_string(),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).unwrap_or(HeaderValue::from_static("/setup.html")),
    );
    (StatusCode::SEE_OTHER, headers).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_means_empty_or_comments_only_never_invalid() {
        assert!(is_blank(""));
        assert!(is_blank("  \n\t"));
        assert!(is_blank("{}"));
        assert!(is_blank(r#"{ "_comment": "fill me in" }"#));

        assert!(!is_blank(r#"{ "TZ": "UTC" }"#));
        assert!(!is_blank("{ not json"));
        assert!(!is_blank("[]"));
    }

    #[test]
    fn the_file_state_tells_missing_empty_and_present_apart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("config.json");

        assert_eq!(config_file_state(&file), ConfigFileState::Missing);

        std::fs::write(&file, "{}\n").expect("write");
        assert_eq!(config_file_state(&file), ConfigFileState::Empty);

        std::fs::write(&file, r#"{ "TZ": "UTC" }"#).expect("write");
        assert_eq!(config_file_state(&file), ConfigFileState::Present);

        // A typo is still a configuration, which setup must not overwrite.
        std::fs::write(&file, r#"{ "TZ": "UTC", }"#).expect("write");
        assert_eq!(config_file_state(&file), ConfigFileState::Present);

        assert!(ConfigFileState::Missing.needs_setup());
        assert!(ConfigFileState::Empty.needs_setup());
        assert!(!ConfigFileState::Present.needs_setup());
    }

    #[test]
    fn a_browser_is_only_opened_where_someone_can_see_it() {
        assert!(!browser_available(true, true));
        assert_eq!(
            browser_available(false, true),
            cfg!(any(windows, target_os = "linux", target_os = "macos"))
        );
        // No desktop is a container, a unit, a daemon or SSH.
        assert!(!browser_available(false, false));
        #[cfg(windows)]
        assert!(desktop_session());
    }

    #[test]
    fn the_token_is_stable_and_long_enough_to_guess_never() {
        assert_eq!(token(), token());
        assert_eq!(token().len(), 32);
        assert!(token().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn only_a_page_on_this_machine_counts_as_local() {
        for (peer, local) in [
            ("127.0.0.1:50000", true),
            ("[::1]:50000", true),
            ("[::ffff:127.0.0.1]:50000", true),
            ("192.168.1.30:50000", false),
            ("[::ffff:192.168.1.30]:50000", false),
            ("[fe80::1]:50000", false),
        ] {
            let peer: SocketAddr = peer.parse().expect("socket address");
            assert_eq!(is_local_peer(peer), local, "{peer}");
        }
    }

    #[test]
    fn the_setup_url_never_points_at_a_wildcard() {
        let url = setup_url(SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert!(
            url.starts_with("http://127.0.0.1:8080/setup.html?token="),
            "{url}"
        );

        let url = setup_url(SocketAddr::from(([192, 168, 1, 20], 9000)));
        assert!(
            url.starts_with("http://192.168.1.20:9000/setup.html?token="),
            "{url}"
        );
    }
}
