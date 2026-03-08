use anyhow::Result;
use monitoring::{MonitoringHub, MonitoringLayer};
use recording::RecordingState;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_subscriber::filter as other_filter;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

mod alerts;
mod audio;
mod backend;
mod cap;
mod cleanup;
mod config;
mod e2t_ng;
mod filter;
mod header;
mod monitoring;
mod recording;
mod relay;
mod state;
mod webhook;

use config::Config;
use state::AppState;

const CONFIG_PATH: &str = "/app/config.json";
const RELOAD_SIGNAL_PATH: &str = "/app/reload_signal";
const WEB_RUNTIME_CONFIG_PATH: &str = "/app/web_config.json";
const WEB_RUNTIME_CONFIG_FALLBACK_PATH: &str = "web_server/web_config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSource {
    File,
    BuiltInDefault,
}

fn load_config_with_fallback(config_path: &str) -> (Config, ConfigSource, Option<String>) {
    match std::fs::metadata(config_path) {
        Ok(_) => match Config::from_config_json(config_path) {
            Ok(config) => (config, ConfigSource::File, None),
            Err(err) => (
                Config::safe_internal_defaults(),
                ConfigSource::BuiltInDefault,
                Some(format!(
                    "Configuration file '{}' is invalid: {:?}. Using built-in safe defaults.",
                    config_path, err
                )),
            ),
        },
        Err(err) if err.kind() == ErrorKind::NotFound => (
            Config::safe_internal_defaults(),
            ConfigSource::BuiltInDefault,
            Some(format!(
                "Configuration file '{}' was not found. Using built-in safe defaults.",
                config_path
            )),
        ),
        Err(err) => (
            Config::safe_internal_defaults(),
            ConfigSource::BuiltInDefault,
            Some(format!(
                "Failed to access configuration file '{}': {}. Using built-in safe defaults.",
                config_path, err
            )),
        ),
    }
}

fn load_raw_config_json(config_path: &str) -> Option<serde_json::Value> {
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
        serde_json::Value::String(config.dedicated_alert_log_file.to_string_lossy().to_string()),
    );
    map.insert(
        "MONITORING_BIND_PORT".to_string(),
        serde_json::Value::Number(serde_json::Number::from(config.monitoring_bind_port as u64)),
    );
    map.insert(
        "MONITORING_MAX_LOGS".to_string(),
        serde_json::Value::Number(serde_json::Number::from(
            config.monitoring_max_log_entries as u64
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

    serde_json::Value::Object(map)
}

fn write_atomic_text_file(path: &str, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let tmp_path = format!("{path}.tmp");
    std::fs::write(&tmp_path, contents)?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

fn sync_web_runtime_config(config: &Config) {
    let raw_config = load_raw_config_json(CONFIG_PATH);
    let payload = build_web_runtime_config_payload(config, raw_config.as_ref());
    let serialized = match serde_json::to_string_pretty(&payload) {
        Ok(serialized) => serialized,
        Err(err) => {
            warn!("Failed to serialize web runtime config payload: {}", err);
            return;
        }
    };

    let mut wrote_any = false;
    for path in [WEB_RUNTIME_CONFIG_PATH, WEB_RUNTIME_CONFIG_FALLBACK_PATH] {
        match write_atomic_text_file(path, &serialized) {
            Ok(_) => {
                wrote_any = true;
            }
            Err(err) => {
                warn!("Failed writing web runtime config '{}': {}", path, err);
            }
        }
    }

    if !wrote_any {
        warn!("Web runtime config could not be written to any configured path.");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let (config, config_source, config_warning) = load_config_with_fallback(CONFIG_PATH);

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
    let env_filter = EnvFilter::from_default_env();
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

    if config_source == ConfigSource::BuiltInDefault {
        if let Some(message) = config_warning.as_deref() {
            warn!("{}", message);
        }
    } else {
        info!("Loaded configuration from {}", CONFIG_PATH);
    }

    webhook::apply_runtime_config(&config);
    sync_web_runtime_config(&config);

    info!("Starting EAS Listener...");

    let app_state = Arc::new(Mutex::new(AppState::new(config.filters.clone())));
    let recording_state = Arc::new(Mutex::new(HashMap::<String, RecordingState>::new()));

    let (tx, rx) = mpsc::channel::<(String, String, String, String, Duration, String)>(32);
    let (nnnn_tx, _nnnn_rx) = broadcast::channel::<String>(16);
    let (reload_tx, _reload_rx) = broadcast::channel::<Config>(16);

    let audio_processor_handle = tokio::spawn(audio::run_audio_processor(
        config.clone(),
        tx,
        recording_state.clone(),
        nnnn_tx.clone(),
        monitoring.clone(),
        reload_tx.subscribe(),
    ));
    let alert_manager_handle = tokio::spawn(alerts::run_alert_manager(
        config.clone(),
        app_state.clone(),
        rx,
        recording_state,
        nnnn_tx.subscribe(),
        monitoring.clone(),
        reload_tx.subscribe(),
    ));
    let state_cleanup_handle = tokio::spawn(alerts::run_state_cleanup(
        config.clone(),
        app_state.clone(),
        monitoring.clone(),
    ));
    let log_cleanup_handle = tokio::spawn(cleanup::run_log_cleanup(config.clone()));
    let reload_handler_handle =
        tokio::spawn(run_reload_handler(app_state.clone(), reload_tx.clone()));
    let api_handle = tokio::spawn(backend::run_server(
        config.monitoring_bind_addr,
        app_state.clone(),
        monitoring.clone(),
        config.clone(),
    ));
    let cap_supervisor_handle = tokio::spawn(cap::run_cap_supervisor(
        config.clone(),
        app_state.clone(),
        monitoring.clone(),
        reload_tx.subscribe(),
    ));

    tokio::select! {
        _ = audio_processor_handle => info!("Audio processor task exited."),
        _ = alert_manager_handle => info!("Alert manager task exited."),
        _ = state_cleanup_handle => info!("State cleanup task exited."),
        _ = log_cleanup_handle => info!("Log cleanup task exited."),
        _ = cap_supervisor_handle => info!("CAP supervisor task exited."),
        _ = reload_handler_handle => info!("Reload handler task exited."),
        _ = api_handle => info!("Monitoring API task exited."),
    };

    Ok(())
}

async fn run_reload_handler(
    app_state: Arc<Mutex<AppState>>,
    reload_tx: broadcast::Sender<Config>,
) -> Result<()> {
    let mut poller = tokio::time::interval(Duration::from_secs(1));
    poller.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_seen_modified: Option<std::time::SystemTime> = None;

    loop {
        poller.tick().await;

        let metadata = match tokio::fs::metadata(RELOAD_SIGNAL_PATH).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                warn!("Failed checking reload signal file: {}", err);
                continue;
            }
        };

        let modified = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let should_reload = last_seen_modified
            .map(|known_modified| modified > known_modified)
            .unwrap_or(true);
        if !should_reload {
            continue;
        }

        let (new_config, config_source, config_warning) = load_config_with_fallback(CONFIG_PATH);

        if config_source == ConfigSource::BuiltInDefault {
            if let Some(message) = config_warning.as_deref() {
                warn!("{}", message);
            }
        }

        webhook::apply_runtime_config(&new_config);
        sync_web_runtime_config(&new_config);

        {
            let mut guard = app_state.lock().await;
            guard.update_filters(new_config.filters.clone());
        }

        if reload_tx.send(new_config).is_err() {
            warn!("No active reload receivers were available for configuration update.");
        }

        if config_source == ConfigSource::File {
            info!("Applied configuration reload from reload signal.");
        } else {
            warn!("Applied built-in safe defaults for configuration reload.");
        }

        if let Err(err) = tokio::fs::remove_file(RELOAD_SIGNAL_PATH).await {
            if err.kind() != ErrorKind::NotFound {
                warn!("Failed to remove reload signal file: {}", err);
            }
        }

        last_seen_modified = Some(modified);
    }
}
