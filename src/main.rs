use anyhow::Result;
use monitoring::{MonitoringHub, MonitoringLayer};
use recording::RecordingState;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{error, info, warn};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter as other_filter;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

mod alerts;
mod audio;
mod backend;
mod cleanup;
mod config;
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

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_config_json(CONFIG_PATH)?;

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
        .with_target("symphonia", tracing::Level::ERROR);

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

    info!("Starting EAS Listener...");

    let app_state = Arc::new(Mutex::new(AppState::new(config.filters.clone())));
    let recording_state = Arc::new(Mutex::new(Option::<RecordingState>::None));

    let (tx, rx) = mpsc::channel::<(String, String, String, String, Duration, String)>(32);
    let (nnnn_tx, _nnnn_rx) = broadcast::channel::<()>(1);
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
    let reload_handler_handle = tokio::spawn(run_reload_handler(app_state.clone(), reload_tx));
    let api_handle = tokio::spawn(backend::run_server(
        config.monitoring_bind_addr,
        app_state.clone(),
        monitoring,
    ));

    tokio::select! {
        _ = audio_processor_handle => info!("Audio processor task exited."),
        _ = alert_manager_handle => info!("Alert manager task exited."),
        _ = state_cleanup_handle => info!("State cleanup task exited."),
        _ = log_cleanup_handle => info!("Log cleanup task exited."),
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

        match Config::from_config_json(CONFIG_PATH) {
            Ok(new_config) => {
                {
                    let mut guard = app_state.lock().await;
                    guard.update_filters(new_config.filters.clone());
                }

                if reload_tx.send(new_config).is_err() {
                    warn!("No active reload receivers were available for configuration update.");
                }

                info!("Applied configuration reload from reload signal.");

                if let Err(err) = tokio::fs::remove_file(RELOAD_SIGNAL_PATH).await {
                    if err.kind() != ErrorKind::NotFound {
                        warn!("Failed to remove reload signal file: {}", err);
                    }
                }
            }
            Err(err) => {
                error!("Failed to reload configuration from {}: {:?}", CONFIG_PATH, err);
            }
        }

        last_seen_modified = Some(modified);
    }
}
