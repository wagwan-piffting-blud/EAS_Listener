use crate::config::Config;
use crate::filter;
use crate::monitoring::MonitoringHub;
use crate::recording::{self, RecordingState};
use crate::relay::RelayState;
use crate::state::{ActiveAlert, AppState, EasAlertData};
use crate::webhook::send_alert_webhook;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::{mpsc::Receiver, Mutex};
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

const RAINY_DAY_FILE: &str = "rainy_day.txt";
const SEVERE_DAY_FILE: &str = "severe_day.txt";

fn is_alert_relevant(alert_data: &EasAlertData, watched_fips: &HashSet<String>) -> bool {
    if watched_fips.is_empty() {
        return true;
    }
    if watched_fips.contains("000000") || watched_fips.contains("") {
        return true;
    }
    if alert_data.fips.iter().any(|fips| fips == "000000") {
        return true;
    }
    alert_data
        .fips
        .iter()
        .any(|fips| watched_fips.contains(fips))
}

pub async fn run_alert_manager(
    mut config: Config,
    state: Arc<Mutex<AppState>>,
    mut rx: Receiver<(String, String, String, String, Duration, String)>,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    nnnn_rx: BroadcastReceiver<String>,
    monitoring: MonitoringHub,
    mut reload_rx: BroadcastReceiver<Config>,
) -> Result<()> {
    let mut reload_enabled = true;
    loop {
        let (event, locations, originator, raw_header, purge_time, stream_id) = tokio::select! {
            maybe_alert = rx.recv() => {
                let Some(alert) = maybe_alert else {
                    break;
                };
                alert
            }
            reload_result = reload_rx.recv(), if reload_enabled => {
                match reload_result {
                    Ok(new_config) => {
                        info!("Alert manager loaded updated configuration.");
                        config = new_config;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("Alert manager reload channel lagged; skipped {} update(s).", skipped);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        warn!("Alert manager reload channel closed; keeping current configuration.");
                        reload_enabled = false;
                    }
                }
                continue;
            }
        };

        info!("Processing alert: {}", &raw_header);

        let dsame_result = get_eas_details_and_log(&config, &raw_header).await;
        let alert_data = match &dsame_result {
            Ok(data) => data.clone(),
            Err(_) => EasAlertData {
                eas_text: "Decoder script failed.".to_string(),
                event_text: event.clone(),
                event_code: event,
                fips: vec![],
                locations,
                originator,
            },
        };

        if is_alert_relevant(&alert_data, &config.watched_fips) {
            info!("Alert for watched zone(s) received. Relaying...");
            let alert = ActiveAlert::new(alert_data.clone(), raw_header.clone(), purge_time);

            let active_snapshot = {
                let mut app_state_guard = state.lock().await;
                let now = Utc::now();
                app_state_guard.active_alerts.retain(|existing| {
                    existing.expires_at > now && existing.raw_header != raw_header
                });
                app_state_guard.active_alerts.push(alert.clone());

                if let Err(e) = update_alert_files(&config.shared_state_dir, &app_state_guard).await
                {
                    error!("Failed to update alert files: {}", e);
                }

                app_state_guard.active_alerts.clone()
            };
            monitoring.broadcast_alerts(
                active_snapshot,
                Some(stream_id.as_str()),
                Some(alert.data.event_code.as_str()),
            );

            let dsame_text = match dsame_result {
                Ok(data) => data.eas_text,
                Err(e) => format!("Decoder script failed: {}", e),
            };

            let value = handle_recording_and_webhook(
                config.clone(),
                state.clone(),
                recording_state.clone(),
                alert,
                dsame_text,
                raw_header,
                purge_time,
                stream_id,
                nnnn_rx.resubscribe(),
            );

            tokio::spawn(value);
        } else {
            info!(
                "Ignoring alert for non-watched zones: {}",
                &alert_data.locations
            );
        }
    }
    Ok(())
}

async fn handle_recording_and_webhook(
    config: Config,
    state: Arc<Mutex<AppState>>,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    alert: ActiveAlert,
    dsame_text: String,
    raw_header: String,
    _purge_time: Duration,
    stream_id: String,
    mut nnnn_rx: BroadcastReceiver<String>,
) {
    let event_code = alert.data.event_code.clone();
    let mut recorded_state: Option<(PathBuf, String)> = None;
    let mut join_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;

    let mut recorder = recording_state.lock().await;
    if !recorder.contains_key(stream_id.as_str()) {
        match recording::start_encoding_task(&config, &raw_header, &stream_id) {
            Ok((handle, new_state)) => {
                info!("Recording started for alert: {}", event_code);
                recorder.insert(stream_id.clone(), new_state);
                join_handle = Some(handle);
            }
            Err(e) => warn!("Failed to start recording: {}", e),
        }
    }
    drop(recorder);

    if let Some(handle) = join_handle {
        let sleep_duration = Duration::from_secs(300);
        info!(
            "Waiting for alert to end ({}s timeout or NNNN)...",
            sleep_duration.as_secs()
        );

        let deadline = tokio::time::Instant::now() + sleep_duration;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    info!("Recording timer expired for alert: {}", event_code);
                    break;
                }
                res = nnnn_rx.recv() => {
                    match res {
                        Ok(nnnn_stream_id) if nnnn_stream_id == stream_id => {
                            info!("NNNN received for stream {}, stopping recording for alert: {}", stream_id, event_code);
                            break;
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("NNNN channel lagged; skipped {} message(s).", skipped);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("NNNN broadcast channel closed.");
                            break;
                        }
                    }
                }
            }
        }

        info!("Stopping recording for alert: {}", event_code);

        if let Some(RecordingState {
            audio_tx,
            output_path,
            source_stream,
        }) = recording_state.lock().await.remove(&stream_id)
        {
            drop(audio_tx);
            recorded_state = Some((output_path, source_stream));
        } else {
            warn!(
                "Recording state missing when finalizing alert {}",
                alert.data.event_code
            );
        }

        if let Err(e) = handle.await {
            warn!("Encoder task failed: {:?}", e);
        }
    }

    if filter::should_log_alert(&event_code) {
        let recording_path_for_webhook = recorded_state.as_ref().map(|(path, _)| path.clone());
        send_alert_webhook(
            &stream_id,
            &alert,
            &dsame_text,
            &raw_header,
            recording_path_for_webhook,
        )
        .await;
    }

    if filter::should_forward_alert(&event_code) {
        info!("Forwarding alert {} to configured webhook(s)", event_code);
        let recording_path_for_webhook = recorded_state.as_ref().map(|(path, _)| path.clone());
        send_alert_webhook(
            &stream_id,
            &alert,
            &dsame_text,
            &raw_header,
            recording_path_for_webhook,
        )
        .await;
        return;
    }

    if config.should_relay {
        if let Some((ref recording_path, ref source_stream)) = recorded_state {
            let filters = {
                let guard = state.lock().await;
                guard.cloned_filters()
            };

            let relay_state = match RelayState::new(config.clone()).await {
                Ok(state) => state,
                Err(err) => {
                    warn!("Skipping relay due to configuration error: {:?}", err);
                    return;
                }
            };

            if let Err(err) = relay_state
                .start_relay(
                    event_code.as_str(),
                    filters.as_slice(),
                    recording_path,
                    Some(source_stream.as_str()),
                    &raw_header,
                )
                .await
            {
                warn!("FFmpeg relay failed: {:?}", err);
            }
        } else {
            warn!("No completed recording available for relay; skipping FFmpeg relay.");
        }
    }
}

pub async fn run_state_cleanup(
    config: Config,
    state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
) -> Result<()> {
    let mut timer = interval(Duration::from_secs(60));
    loop {
        timer.tick().await;

        let mut app_state_guard = state.lock().await;
        let initial_count = app_state_guard.active_alerts.len();
        let now = Utc::now();
        app_state_guard
            .active_alerts
            .retain(|alert| alert.expires_at > now);
        let removed_count = initial_count - app_state_guard.active_alerts.len();

        if removed_count > 0 {
            info!("Removed {} expired alert(s).", removed_count);
            if let Err(e) = update_alert_files(&config.shared_state_dir, &app_state_guard).await {
                error!("Failed to update alert files after cleanup: {}", e);
            }
        }

        let alert_snapshot = app_state_guard.active_alerts.clone();
        drop(app_state_guard);

        if removed_count > 0 {
            monitoring.broadcast_alerts(alert_snapshot, None, None);
        }
    }
}

async fn get_eas_details_and_log(config: &Config, raw_header: &str) -> Result<EasAlertData> {
    let header_clone = raw_header.to_string();
    let timezone = config.timezone.clone().to_string();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("python3")
            .arg("/usr/local/bin/decoder.py")
            .arg("--msg")
            .arg(header_clone)
            .arg("--tz")
            .arg(timezone)
            .output()
    })
    .await??;

    if output.status.success() {
        let alert_data: EasAlertData = serde_json::from_slice(&output.stdout)?;

        let received_at = Utc::now();
        let local_time = received_at.with_timezone(&config.timezone);
        let timestamp = local_time.format("%Y-%m-%d %l:%M:%S %p");
        let log_line = format!(
            "{}: {} (Received @ {})\n\n",
            raw_header, alert_data.eas_text, timestamp
        );

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.dedicated_alert_log_file)
            .await?;
        file.write_all(log_line.as_bytes()).await?;

        Ok(alert_data)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("decoder.py script failed: {}", stderr);
    }
}

#[instrument(skip(state_dir, app_state))]
async fn update_alert_files(state_dir: &Path, app_state: &AppState) -> Result<()> {
    let has_severe_warning = app_state
        .active_alerts
        .iter()
        .any(|a| matches!(a.data.event_code.trim(), "SVR" | "TOR"));
    let has_severe_watch = app_state
        .active_alerts
        .iter()
        .any(|a| a.data.event_code.trim() == "TOA");
    let has_moderate_watch = app_state
        .active_alerts
        .iter()
        .any(|a| a.data.event_code.trim() == "SVA");

    let rainy_path = state_dir.join(RAINY_DAY_FILE);
    let severe_path = state_dir.join(SEVERE_DAY_FILE);

    if has_severe_warning || has_severe_watch {
        info!("Severe alert active. Ensuring `severe_day.txt` exists.");
        fs::write(&severe_path, "").await?;
        if fs::try_exists(&rainy_path).await? {
            fs::remove_file(&rainy_path).await?;
        }
    } else if has_moderate_watch {
        info!("Moderate watch active. Ensuring `rainy_day.txt` exists.");
        fs::write(&rainy_path, "").await?;
        if fs::try_exists(&severe_path).await? {
            fs::remove_file(&severe_path).await?;
        }
    } else {
        info!("No relevant alerts active. Cleaning up state files.");
        if fs::try_exists(&rainy_path).await? {
            fs::remove_file(&rainy_path).await?;
        }
        if fs::try_exists(&severe_path).await? {
            fs::remove_file(&severe_path).await?;
        }
    }

    Ok(())
}
