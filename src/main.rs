use anyhow::Result;
use monitoring::{MonitoringHub, MonitoringLayer};
use recording::RecordingState;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};
use tracing_subscriber::filter as other_filter;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

mod alert_stream;
mod alerts;
mod archive;
mod audio;
mod autostart;
mod backend;
mod cap;
#[path = "cap-cp.rs"]
mod cap_cp;
mod cleanup;
mod components;
mod config;
mod config_schema;
mod db;
mod e2t_ng;
mod filter;
mod header;
mod hellotts;
mod launchd;
mod monitoring;
mod notifications;
mod nws_bulletin;
mod paths;
mod recording;
mod relay;
#[cfg(all(windows, feature = "service"))]
mod service;
mod setup;
mod state;
mod systemd;
mod tone;
#[cfg(feature = "tray")]
mod tray;
mod web_assets;
mod webhook;

use config::Config;
use state::AppState;

const TEST_ALERT_STREAM_ID: &str = "Manual Test Alert";
/// Loquendo runs at roughly 4x realtime and the halve-and-retry path can add attempts, so this is
/// generous; it exists to report a hung engine rather than to hurry a slow one.
const TEST_ALERT_TTS_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for the alert pipeline to open, or release, the test alert's recording.
const TEST_ALERT_RECORDING_WAIT: Duration = Duration::from_secs(10);
const RECORDING_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 100 ms at the recording's 48 kHz.
const RECORDING_FEED_CHUNK_SAMPLES: usize = 4_800;

/// Loads config.json with no fallback to the built-in defaults: a listener running on settings
/// nobody chose -- monitoring a stream nobody picked, behind credentials nobody can sign in with --
/// is worse than one that stops and says why.
fn load_config(config_path: &Path) -> Result<Config> {
    Config::from_config_json(config_path).map_err(|err| {
        let mut backup = config_path.as_os_str().to_os_string();
        backup.push(".bak");
        let backup = PathBuf::from(backup);
        let hint = if backup.is_file() {
            format!(
                " The version before the last save from the dashboard is {}.",
                backup.display()
            )
        } else {
            String::new()
        };
        // The path is already named here, so only the innermost cause is worth repeating.
        anyhow::anyhow!(
            "{} could not be loaded: {}. Fix it and start the listener again.{hint}",
            config_path.display(),
            err.root_cause()
        )
    })
}

fn load_raw_config_json(config_path: &Path) -> Option<serde_json::Value> {
    let payload = std::fs::read_to_string(config_path).ok()?;
    serde_json::from_str::<serde_json::Value>(&payload).ok()
}

fn boolish_value(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(v) => Some(*v),
        serde_json::Value::Number(v) => Some(v.as_i64().unwrap_or(0) != 0),
        serde_json::Value::String(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn build_web_runtime_config_payload(
    config: &Config,
    raw_config: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut map = match raw_config {
        Some(serde_json::Value::Object(raw_map)) => raw_map.clone(),
        _ => serde_json::Map::new(),
    };

    let mut watched_fips = config.watched_fips.iter().cloned().collect::<Vec<_>>();
    watched_fips.sort();

    map.insert(
        "USE_REVERSE_PROXY".to_string(),
        serde_json::Value::Bool(config.use_reverse_proxy),
    );
    map.insert(
        "WS_REVERSE_PROXY_URL".to_string(),
        serde_json::Value::String(config.ws_reverse_proxy_url.clone()),
    );
    map.insert(
        "REVERSE_PROXY_URL".to_string(),
        serde_json::Value::String(config.reverse_proxy_url.clone()),
    );
    map.insert(
        "DASHBOARD_USERNAME".to_string(),
        serde_json::Value::String(config.dashboard_username.clone()),
    );
    map.insert(
        "DASHBOARD_PASSWORD".to_string(),
        serde_json::Value::String(config.dashboard_password.clone()),
    );
    map.insert(
        "SHARED_STATE_DIR".to_string(),
        serde_json::Value::String(config.shared_state_dir.to_string_lossy().to_string()),
    );
    map.insert(
        "RECORDING_DIR".to_string(),
        serde_json::Value::String(config.recording_dir.to_string_lossy().to_string()),
    );
    map.insert(
        "DEDICATED_ALERT_LOG_FILE".to_string(),
        serde_json::Value::String(
            config
                .dedicated_alert_log_file
                .to_string_lossy()
                .to_string(),
        ),
    );
    map.insert(
        "ALERT_DATABASE_FILE".to_string(),
        serde_json::Value::String(config.alert_database_file.to_string_lossy().to_string()),
    );
    map.insert(
        "MONITORING_BIND_PORT".to_string(),
        serde_json::Value::Number(serde_json::Number::from(config.monitoring_bind_port as u64)),
    );
    map.insert(
        "MONITORING_MAX_LOGS".to_string(),
        serde_json::Value::Number(serde_json::Number::from(
            config.monitoring_max_log_entries as u64,
        )),
    );
    map.insert(
        "WATCHED_FIPS".to_string(),
        serde_json::Value::String(watched_fips.join(",")),
    );
    map.insert(
        "TZ".to_string(),
        serde_json::Value::String(config.timezone.name().to_string()),
    );
    map.insert(
        "ICECAST_STREAM_URL_ARRAY".to_string(),
        serde_json::Value::Array(
            config
                .icecast_stream_urls
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );

    let alert_sound_src = map
        .get("ALERT_SOUND_SRC")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("iembot.mp3")
        .to_string();
    map.insert(
        "ALERT_SOUND_SRC".to_string(),
        serde_json::Value::String(alert_sound_src),
    );

    let alert_sound_enabled = map
        .get("ALERT_SOUND_ENABLED")
        .and_then(boolish_value)
        .unwrap_or(false);
    map.insert(
        "ALERT_SOUND_ENABLED".to_string(),
        serde_json::Value::Bool(alert_sound_enabled),
    );

    if !map.contains_key("ICECAST_STREAM_URL_MAPPING") {
        map.insert(
            "ICECAST_STREAM_URL_MAPPING".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }

    map.insert(
        "ICECAST_ALERT_STREAM_ENABLED".to_string(),
        serde_json::Value::Bool(config.icecast_alert_stream_enabled),
    );
    map.insert(
        "ICECAST_ALERT_PORT".to_string(),
        serde_json::Value::Number(serde_json::Number::from(config.icecast_alert_port as u64)),
    );
    map.insert(
        "ICECAST_ALERT_MOUNT".to_string(),
        serde_json::Value::String(config.icecast_alert_mount.clone()),
    );
    map.insert(
        "ICECAST_ALERT_PUBLIC_URL".to_string(),
        serde_json::Value::String(config.icecast_alert_public_url.clone()),
    );

    serde_json::Value::Object(map)
}

fn write_atomic_text_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    std::fs::write(&tmp_path, contents)?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

fn sync_web_runtime_config(config: &Config) {
    let raw_config = load_raw_config_json(&paths::config_json());
    let payload = build_web_runtime_config_payload(config, raw_config.as_ref());
    let serialized = match serde_json::to_string_pretty(&payload) {
        Ok(serialized) => serialized,
        Err(err) => {
            warn!("Failed to serialize web runtime config payload: {}", err);
            return;
        }
    };

    let mut wrote_any = false;
    for path in [
        paths::web_runtime_config(),
        paths::web_runtime_config_fallback(),
    ] {
        match write_atomic_text_file(&path, &serialized) {
            Ok(_) => {
                wrote_any = true;
            }
            Err(err) => {
                warn!(
                    "Failed writing web runtime config '{}': {}",
                    path.display(),
                    err
                );
            }
        }
    }

    if !wrote_any {
        warn!("Web runtime config could not be written to any configured path.");
    }
}

/// The URL a browser should open to reach the dashboard. A wildcard bind is not a usable address
/// to click, so it is reported as loopback.
#[cfg(feature = "tray")]
fn dashboard_url(config: &Config) -> String {
    let addr = config.monitoring_bind_addr;
    if addr.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", addr.port())
    } else {
        format!("http://{addr}")
    }
}

const USAGE: &str = "EAS Listener

USAGE:
    eas_listener [OPTIONS]

OPTIONS:
    --app-root <DIR>       Where config.json and the dashboard live. Same as EAS_APP_ROOT.
    --install-service      Start automatically from now on, and start now: a Windows service, a
                           systemd unit on Linux (both need admin/root), or on macOS a
                           LaunchDaemon with sudo (at boot) or a LaunchAgent without (at login).
    --uninstall-service    Remove that service, unit or job.
    --service-status       Show the service's current state.
    --service              Run as the service itself. The Service Control Manager, the systemd
                           unit and the launchd job pass this; it is not useful from a terminal.
    --no-browser           Do not open first-run setup in the default browser. Same as
                           EAS_NO_BROWSER=1.
    -h, --help             Show this message.
";

/// Applies the arguments that have to take effect before anything reads configuration, and
/// returns whatever the caller still has to act on.
fn take_app_root_arg(args: &mut Vec<String>) {
    if let Some(index) = args.iter().position(|arg| arg == "--app-root") {
        if let Some(value) = args.get(index + 1) {
            std::env::set_var("EAS_APP_ROOT", value);
        }
        args.drain(index..=(index + 1).min(args.len() - 1));
    }
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    take_app_root_arg(&mut args);
    if let Some(index) = args.iter().position(|arg| arg == "--no-browser") {
        args.remove(index);
        setup::disable_browser_launch();
    }

    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    #[cfg(all(windows, feature = "service"))]
    {
        if args.iter().any(|arg| arg == "--install-service") {
            return service::install();
        }
        if args.iter().any(|arg| arg == "--uninstall-service") {
            return service::uninstall();
        }
        if args.iter().any(|arg| arg == "--service-status") {
            return service::status();
        }
        if args.iter().any(|arg| arg == "--service") {
            // The dispatcher owns this thread and builds its own runtime.
            return service::host::start();
        }
    }

    #[cfg(target_os = "linux")]
    {
        if args.iter().any(|arg| arg == "--install-service") {
            return systemd::install();
        }
        if args.iter().any(|arg| arg == "--uninstall-service") {
            return systemd::uninstall();
        }
        if args.iter().any(|arg| arg == "--service-status") {
            return systemd::status();
        }
        // systemd needs no handshake, so this only marks the process as a service and carries
        // on as the plain listener.
        if let Some(index) = args.iter().position(|arg| arg == "--service") {
            args.remove(index);
            paths::mark_running_as_service();
            setup::disable_browser_launch();
        }
    }

    #[cfg(target_os = "macos")]
    {
        if args.iter().any(|arg| arg == "--install-service") {
            return launchd::install();
        }
        if args.iter().any(|arg| arg == "--uninstall-service") {
            return launchd::uninstall();
        }
        if args.iter().any(|arg| arg == "--service-status") {
            return launchd::status();
        }
        // launchd needs no handshake either.
        if let Some(index) = args.iter().position(|arg| arg == "--service") {
            args.remove(index);
            paths::mark_running_as_service();
            setup::disable_browser_launch();
        }
    }

    #[cfg(not(any(
        all(windows, feature = "service"),
        target_os = "linux",
        target_os = "macos"
    )))]
    {
        for flag in [
            "--install-service",
            "--uninstall-service",
            "--service-status",
            "--service",
        ] {
            if args.iter().any(|arg| arg == flag) {
                anyhow::bail!(
                    "{flag} needs Linux with systemd, macOS, or a Windows build compiled with \
                     the `service` feature (cargo build --release --features service)."
                );
            }
        }
    }

    if let Some(unknown) = args.iter().find(|arg| arg.starts_with('-')) {
        anyhow::bail!(
            "Unknown option: {unknown}

{USAGE}"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");

    #[cfg(not(feature = "tray"))]
    {
        runtime.block_on(run_listener())
    }

    #[cfg(feature = "tray")]
    {
        // A launchd daemon, a systemd unit or an SSH login has nowhere to show an icon.
        if !setup::desktop_session() {
            println!("No desktop session, so no tray icon; running without one.");
            return runtime.block_on(run_listener());
        }
        println!("Showing the tray icon.");

        // The platform event loop has to own the main thread, so the listener moves onto the
        // runtime and the tray runs here. Quitting from the tray drops the runtime, which stops
        // the listener with it.
        // Only the address and the log folder are needed here. A broken config.json stops the
        // listener itself, which says why on the console.
        let config = Config::from_config_json(&paths::config_json())
            .unwrap_or_else(|_| Config::safe_internal_defaults());
        let url = if setup::config_file_state(&paths::config_json()).needs_setup() {
            setup::setup_url(config.monitoring_bind_addr)
        } else {
            dashboard_url(&config)
        };
        let log_dir = config.shared_state_dir.clone();

        let _guard = runtime.enter();
        tray::run(url, log_dir, |stopped| {
            runtime.spawn(async move {
                let result = run_listener().await;
                if let Err(err) = &result {
                    eprintln!("EAS Listener stopped: {err:?}");
                }
                stopped.notify(result.err().map(|err| format!("{err:#}")));
            });
        })
    }
}

/// Starts the listener and blocks until one of its tasks exits.
async fn run_listener() -> Result<()> {
    let config_path = paths::config_json();

    // Logging is configured by config.json, so setup reports to the console and nothing else.
    if setup::config_file_state(&config_path).needs_setup() {
        let handover = setup::run(Config::safe_internal_defaults().monitoring_bind_addr).await?;
        if handover == setup::Handover::Service {
            // The service is already binding the port this process just released.
            std::process::exit(0);
        }
        if setup::restart_after_setup() {
            std::process::exit(setup::RESTART_EXIT_CODE);
        }
    }

    let config =
        load_config(&config_path).map_err(|err| anyhow::anyhow!("Refusing to start: {err}"))?;

    if let Err(err) = std::fs::create_dir_all(&config.shared_state_dir) {
        eprintln!(
            "Warning: failed to create shared state directory {:?}: {}",
            config.shared_state_dir, err
        );
    }
    if let Err(err) = std::fs::create_dir_all(&config.recording_dir) {
        eprintln!(
            "Warning: failed to create recording directory {:?}: {}",
            config.recording_dir, err
        );
    }

    let monitoring = MonitoringHub::new(
        config.monitoring_max_log_entries,
        Duration::from_secs(config.monitoring_activity_window_secs),
    );

    let timer = ChronoLocal::new("%Y-%m-%d %I:%M:%S.%3f %p ".to_string());
    let file_appender =
        tracing_appender::rolling::daily(&config.shared_state_dir, &config.alert_log_file);
    let (non_blocking_file, _guard) = tracing_appender::non_blocking(file_appender);
    // Only the Docker entrypoint exports config.json's RUST_LOG into the environment. Without that
    // fallback, a run outside the container gets EnvFilter's ERROR-only default and silently
    // discards everything the configured log level asked for.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.as_str()));
    let log_level = config
        .log_level
        .parse::<LevelFilter>()
        .unwrap_or(LevelFilter::INFO);
    let monitoring_layer = MonitoringLayer::new(monitoring.clone());
    let filter = other_filter::Targets::new()
        .with_default(log_level)
        .with_target("symphonia", tracing::Level::ERROR)
        .with_target("sameold", tracing::Level::WARN);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking_file)
                .with_ansi(false)
                .with_timer(timer.clone()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_timer(timer),
        )
        .with(monitoring_layer)
        .with(filter)
        .init();

    info!("Loaded configuration from {}", config_path.display());
    if paths::config_json_is_directory() {
        info!(
            "config.json is a directory (Docker makes one when the file it mounts does not exist \
             yet), so the configuration is kept inside it."
        );
    }
    info!(
        "Application root resolved to {}",
        paths::app_root().display()
    );

    webhook::apply_runtime_config(&config);
    components::apply_runtime_config(&config);
    sync_web_runtime_config(&config);

    let mut component_status = components::probe_all().await;
    if components::fetch_missing_required(&component_status).await {
        components::apply_runtime_config(&config);
        component_status = components::probe_all().await;
    }
    components::report(&component_status);
    tokio::spawn(cap::prefetch_tts_engine(config.clone()));

    let missing = components::missing_required(&component_status);
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|status| status.key)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow::anyhow!(
            "Missing required component(s): {names}. Install them, set the matching *_PATH key in \
             config.json, or place the binaries in {}.",
            paths::tools_dir().display()
        ));
    }

    let db = db::DbHandle::open(&config.alert_database_file)?;
    if let Err(err) = db.migrate_legacy_log(&config.dedicated_alert_log_file, &config.recording_dir)
    {
        warn!("Legacy alert log migration failed: {}", err);
    }

    info!("Starting EAS Listener...");

    let app_state = Arc::new(Mutex::new(AppState::new(config.filters.clone())));
    let recording_state = Arc::new(Mutex::new(HashMap::<String, RecordingState>::new()));

    let (tx, rx) = mpsc::channel::<(String, String, String, String, Duration, String)>(32);
    let (nnnn_tx, _nnnn_rx) = broadcast::channel::<String>(16);
    let (reload_tx, _reload_rx) = broadcast::channel::<Config>(16);
    // Depth 1: a second request while one is queued is the same request, so it coalesces.
    let (reload_request_tx, reload_request_rx) = mpsc::channel::<()>(1);
    let (test_alert_request_tx, test_alert_request_rx) = mpsc::channel::<()>(1);

    let test_alert_tx = tx.clone();
    let test_alert_nnnn_tx = nnnn_tx.clone();

    let audio_processor_handle = tokio::spawn(audio::run_audio_processor(
        config.clone(),
        tx,
        recording_state.clone(),
        nnnn_tx.clone(),
        monitoring.clone(),
        app_state.clone(),
        reload_tx.subscribe(),
    ));
    let alert_manager_handle = tokio::spawn(alerts::run_alert_manager(
        config.clone(),
        app_state.clone(),
        rx,
        recording_state.clone(),
        nnnn_tx.subscribe(),
        monitoring.clone(),
        reload_tx.subscribe(),
        db.clone(),
    ));
    let state_cleanup_handle = tokio::spawn(alerts::run_state_cleanup(
        config.clone(),
        app_state.clone(),
        monitoring.clone(),
    ));
    let log_cleanup_handle = tokio::spawn(cleanup::run_log_cleanup(config.clone()));
    let reload_handler_handle = tokio::spawn(run_reload_handler(
        app_state.clone(),
        reload_tx.clone(),
        reload_request_rx,
    ));
    let test_alert_handler_handle = tokio::spawn(run_test_alert_handler(
        config.clone(),
        test_alert_tx,
        test_alert_nnnn_tx,
        recording_state,
        reload_tx.subscribe(),
        test_alert_request_rx,
    ));
    let signal_watcher_handle = tokio::spawn(run_signal_file_watcher(
        reload_request_tx.clone(),
        test_alert_request_tx.clone(),
    ));
    let api_handle = tokio::spawn(backend::run_server(
        config.monitoring_bind_addr,
        app_state.clone(),
        monitoring.clone(),
        config.clone(),
        reload_tx.subscribe(),
        db.clone(),
        reload_request_tx,
        test_alert_request_tx,
    ));
    let cap_supervisor_handle = tokio::spawn(cap::run_cap_supervisor(
        config.clone(),
        app_state.clone(),
        monitoring.clone(),
        reload_tx.subscribe(),
        db.clone(),
    ));
    let capcp_supervisor_handle = tokio::spawn(cap_cp::run_capcp_supervisor(
        config.clone(),
        app_state.clone(),
        monitoring.clone(),
        reload_tx.subscribe(),
        db.clone(),
    ));
    let alert_stream_handle = tokio::spawn(alert_stream::run_alert_stream(
        config.clone(),
        reload_tx.subscribe(),
    ));

    tokio::select! {
        _ = audio_processor_handle => info!("Audio processor task exited."),
        _ = alert_manager_handle => info!("Alert manager task exited."),
        _ = state_cleanup_handle => info!("State cleanup task exited."),
        _ = log_cleanup_handle => info!("Log cleanup task exited."),
        _ = cap_supervisor_handle => info!("CAP supervisor task exited."),
        _ = capcp_supervisor_handle => info!("CAP-CP supervisor task exited."),
        _ = reload_handler_handle => info!("Reload handler task exited."),
        _ = test_alert_handler_handle => info!("Test alert handler task exited."),
        _ = signal_watcher_handle => info!("Signal file watcher task exited."),
        _ = alert_stream_handle => info!("Alert stream task exited."),
        _ = api_handle => info!("Monitoring API task exited."),
    };

    Ok(())
}

/// Applies a configuration reload each time the dashboard asks for one.
///
/// This used to poll a signal file once a second, because the PHP front end was a separate
/// process with no other way to reach in. The dashboard is served from this process now, so the
/// request arrives directly on a channel.
async fn run_reload_handler(
    app_state: Arc<Mutex<AppState>>,
    reload_tx: broadcast::Sender<Config>,
    mut requests: mpsc::Receiver<()>,
) -> Result<()> {
    let config_path = paths::config_json();

    while requests.recv().await.is_some() {
        // Like startup, a reload never falls back to the built-in defaults; a broken config.json
        // leaves the listener on the configuration it already has.
        let new_config = match load_config(&config_path) {
            Ok(config) => config,
            Err(err) => {
                error!("Reload refused; the current configuration stays in effect. {err}");
                continue;
            }
        };

        webhook::apply_runtime_config(&new_config);
        components::apply_runtime_config(&new_config);
        components::report(&components::probe_all().await);
        tokio::spawn(cap::prefetch_tts_engine(new_config.clone()));
        sync_web_runtime_config(&new_config);

        {
            let mut guard = app_state.lock().await;
            guard.update_filters(new_config.filters.clone());
        }

        if reload_tx.send(new_config).is_err() {
            warn!("No active reload receivers were available for configuration update.");
        }

        // The trigger itself is logged by whoever requested it, so this stays trigger-agnostic.
        info!("Applied the requested configuration reload.");
    }

    Ok(())
}

fn build_test_alert_header() -> String {
    use chrono::{Datelike, Timelike};

    let now = chrono::Utc::now();
    let issuance = format!("{:03}{:02}{:02}", now.ordinal(), now.hour(), now.minute());

    format!("ZCZC-EAS-RWT-000000+0015-{issuance}-EASLSTNR-")
}

/// Injects a synthetic RWT through the full pipeline each time the dashboard asks for one.
/// Consumes a signal file, reporting whether it was there to consume.
///
/// Deleting it is what makes the next `touch` a new request, so a delete that fails must report no
/// trigger -- otherwise the same leftover file would fire once a second forever.
async fn take_signal_file(path: &Path) -> bool {
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(err) if err.kind() == ErrorKind::NotFound => false,
        Err(err) => {
            warn!("Failed to consume signal file {}: {}", path.display(), err);
            false
        }
    }
}

/// Watches the legacy `reload_signal` and `test_alert_signal` files.
///
/// These predate the dashboard's POST endpoints and existing deployments script against them, so
/// they keep working by feeding the very same request channels the API uses -- there is still only
/// one reload path and one test-alert path.
async fn run_signal_file_watcher(
    reload_requests: mpsc::Sender<()>,
    test_alert_requests: mpsc::Sender<()>,
) -> Result<()> {
    let reload_signal = paths::reload_signal();
    let test_alert_signal = paths::test_alert_signal();

    // A file left behind by a previous run is not a request.
    for stale in [&reload_signal, &test_alert_signal] {
        if let Err(err) = tokio::fs::remove_file(stale).await {
            if err.kind() != ErrorKind::NotFound {
                warn!(
                    "Failed to clear stale signal file {}: {}",
                    stale.display(),
                    err
                );
            }
        }
    }

    info!(
        reload = %reload_signal.display(),
        test_alert = %test_alert_signal.display(),
        "Watching legacy signal files"
    );

    let mut poller = tokio::time::interval(Duration::from_secs(1));
    poller.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        poller.tick().await;

        if take_signal_file(&reload_signal).await {
            info!("Reload requested by {}.", reload_signal.display());
            match reload_requests.try_send(()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(())) => {
                    info!("A configuration reload is already pending; coalescing.")
                }
                Err(mpsc::error::TrySendError::Closed(())) => {
                    warn!("Reload handler is gone; stopping the signal file watcher.");
                    return Ok(());
                }
            }
        }

        if take_signal_file(&test_alert_signal).await {
            info!("Test alert requested by {}.", test_alert_signal.display());
            match test_alert_requests.try_send(()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(())) => {
                    info!("A test alert is already pending; coalescing.")
                }
                Err(mpsc::error::TrySendError::Closed(())) => {
                    warn!("Test alert handler is gone; stopping the signal file watcher.");
                    return Ok(());
                }
            }
        }
    }
}

/// Polls until nothing is recording under `stream_id`. False means the wait ran out.
async fn wait_for_recording_to_close(
    recording_state: &Mutex<HashMap<String, RecordingState>>,
    stream_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !recording_state.lock().await.contains_key(stream_id) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(RECORDING_POLL_INTERVAL).await;
    }
}

/// Polls until a recording opens under `stream_id` and hands back a sender into it.
///
/// The recorder only finishes once every sender is gone, so the caller must drop this before the
/// NNNN that ends the recording, or finalising it waits forever.
async fn wait_for_recording_sender(
    recording_state: &Mutex<HashMap<String, RecordingState>>,
    stream_id: &str,
    timeout: Duration,
) -> Option<mpsc::Sender<Vec<f32>>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(state) = recording_state.lock().await.get(stream_id) {
            return Some(state.audio_tx.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(RECORDING_POLL_INTERVAL).await;
    }
}

async fn feed_recording(audio_tx: mpsc::Sender<Vec<f32>>, samples: &[f32]) {
    for chunk in samples.chunks(RECORDING_FEED_CHUNK_SAMPLES) {
        if audio_tx.send(chunk.to_vec()).await.is_err() {
            warn!("The test alert's recording closed before its narration was written.");
            return;
        }
    }
}

/// Runs the configured TTS engine over the test alert's script and says plainly how it went, so
/// every engine can be checked on demand instead of by waiting for a CAP alert without audio.
async fn narrate_test_alert(config: &Config) -> Option<Vec<f32>> {
    let engine = config.tts_engine.as_str();
    let model = config.tts_model.as_deref().unwrap_or("default");
    info!(
        engine,
        model, "Narrating the test alert with the configured TTS engine"
    );

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        TEST_ALERT_TTS_TIMEOUT,
        cap::synthesize_test_alert_narration(config),
    )
    .await;

    let (sample_rate, samples) = match outcome {
        Ok(Ok(Some(audio))) => audio,
        Ok(Ok(None)) => {
            warn!(
                "Test alert TTS FAILED with {}: the engine produced no audio (see the warnings \
                 above for what it reported). The test alert is being sent without narration.",
                engine
            );
            return None;
        }
        Ok(Err(err)) => {
            warn!(
                "Test alert TTS FAILED with {}: {:#}. The test alert is being sent without \
                 narration.",
                engine, err
            );
            return None;
        }
        Err(_) => {
            warn!(
                "Test alert TTS FAILED with {}: no result within {}s, so the engine looks hung. \
                 The test alert is being sent without narration.",
                engine,
                TEST_ALERT_TTS_TIMEOUT.as_secs()
            );
            return None;
        }
    };

    match recording::pcm_for_recording(&samples, sample_rate) {
        Ok(pcm) => {
            info!(
                "Test alert TTS OK with {}: {:.1}s of {} Hz audio in {:.1}s.",
                engine,
                samples.len() as f64 / sample_rate as f64,
                sample_rate,
                started.elapsed().as_secs_f64()
            );
            Some(pcm)
        }
        Err(err) => {
            warn!(
                "Test alert TTS with {} produced audio that could not be converted for the \
                 recording: {:#}",
                engine, err
            );
            None
        }
    }
}

async fn run_test_alert_handler(
    mut config: Config,
    tx: mpsc::Sender<(String, String, String, String, Duration, String)>,
    nnnn_tx: broadcast::Sender<String>,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    mut reload_rx: broadcast::Receiver<Config>,
    mut requests: mpsc::Receiver<()>,
) -> Result<()> {
    while requests.recv().await.is_some() {
        // Take any reload since the last test, so switching engines and testing again just works.
        loop {
            match reload_rx.try_recv() {
                Ok(newer) => config = newer,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }

        // A test sent straight after another must not land in the previous one's recording.
        if !wait_for_recording_to_close(
            &recording_state,
            TEST_ALERT_STREAM_ID,
            TEST_ALERT_RECORDING_WAIT,
        )
        .await
        {
            warn!("The previous test alert is still recording; this one will not get its own.");
        }

        let raw_header = build_test_alert_header();
        info!("Manual test alert triggered: {}", raw_header);

        let alert = (
            "RWT".to_string(),
            String::new(),
            "EAS".to_string(),
            raw_header,
            Duration::from_secs(15 * 60),
            TEST_ALERT_STREAM_ID.to_string(),
        );

        if let Err(err) = tx.send(alert).await {
            warn!("Failed to inject test alert into pipeline: {}", err);
            continue;
        }

        // The alert is on the dashboard by now; the narration fills its recording's empty body.
        let narration = narrate_test_alert(&config).await;

        match wait_for_recording_sender(
            &recording_state,
            TEST_ALERT_STREAM_ID,
            TEST_ALERT_RECORDING_WAIT,
        )
        .await
        {
            Some(audio_tx) => {
                if let Some(samples) = narration {
                    feed_recording(audio_tx, &samples).await;
                }
            }
            None => warn!(
                "The test alert was not recorded -- a filter or the duplicate check dropped it -- \
                 so there was nowhere to put its narration."
            ),
        }

        // A synthetic alert never produces the NNNN that ends a recording, so one is sent. The
        // recording is known to be open by now, so it cannot be missed.
        if let Err(err) = nnnn_tx.send(TEST_ALERT_STREAM_ID.to_string()) {
            warn!("Failed to broadcast synthetic NNNN for test alert: {}", err);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alert_header_is_a_valid_decodable_rwt() {
        let header = build_test_alert_header();
        assert!(header.starts_with("ZCZC-EAS-RWT-000000+0015-"));
        assert!(header.ends_with("-EASLSTNR-"));

        let parsed_json =
            crate::e2t_ng::parse_header_json(&header).expect("test alert header should parse");
        assert!(parsed_json.contains("RWT"));
        assert!(parsed_json.contains("000000"));

        crate::header::generate_same_header_samples(&header, 44_100, 0.5)
            .expect("test alert header should generate SAME samples");
    }

    #[tokio::test]
    async fn take_signal_file_fires_once_per_touch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let signal = dir.path().join("reload_signal");

        assert!(!take_signal_file(&signal).await);

        std::fs::write(&signal, b"").expect("touch");
        assert!(take_signal_file(&signal).await);
        assert!(!signal.exists());
        assert!(!take_signal_file(&signal).await);

        std::fs::write(&signal, b"").expect("touch again");
        assert!(take_signal_file(&signal).await);
    }

    fn fake_recording(audio_tx: mpsc::Sender<Vec<f32>>) -> RecordingState {
        RecordingState {
            audio_tx,
            output_path: PathBuf::from("test.wav"),
            source_stream: TEST_ALERT_STREAM_ID.to_string(),
        }
    }

    /// The invariant that keeps finalising from hanging: once the narration is fed and the alert
    /// pipeline drops its own sender, the recorder's receiver has to see the end of the stream.
    #[tokio::test]
    async fn narration_reaches_the_recording_and_releases_it() {
        let recordings = Arc::new(Mutex::new(HashMap::<String, RecordingState>::new()));
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<f32>>(32);

        // The pipeline opens the recording a moment after the alert is injected.
        let opener = recordings.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            opener
                .lock()
                .await
                .insert(TEST_ALERT_STREAM_ID.to_string(), fake_recording(audio_tx));
        });

        let sender =
            wait_for_recording_sender(&recordings, TEST_ALERT_STREAM_ID, Duration::from_secs(5))
                .await
                .expect("recording opens");

        let narration: Vec<f32> = (0..(RECORDING_FEED_CHUNK_SAMPLES * 3 + 17))
            .map(|i| (i % 100) as f32 / 100.0)
            .collect();
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(chunk) = audio_rx.recv().await {
                received.extend(chunk);
            }
            received
        });

        feed_recording(sender, &narration).await;
        // What the alert pipeline does on NNNN.
        recordings.lock().await.remove(TEST_ALERT_STREAM_ID);

        let received = tokio::time::timeout(Duration::from_secs(5), reader)
            .await
            .expect("the recorder sees the end of the stream")
            .expect("reader task");
        assert_eq!(received, narration);
    }

    #[tokio::test]
    async fn recording_waits_give_up_rather_than_block() {
        let recordings = Mutex::new(HashMap::<String, RecordingState>::new());
        let short = Duration::from_millis(120);

        assert!(
            wait_for_recording_sender(&recordings, TEST_ALERT_STREAM_ID, short)
                .await
                .is_none()
        );
        assert!(wait_for_recording_to_close(&recordings, TEST_ALERT_STREAM_ID, short).await);

        let (audio_tx, _audio_rx) = mpsc::channel::<Vec<f32>>(1);
        recordings
            .lock()
            .await
            .insert(TEST_ALERT_STREAM_ID.to_string(), fake_recording(audio_tx));
        assert!(!wait_for_recording_to_close(&recordings, TEST_ALERT_STREAM_ID, short).await);
    }
}
