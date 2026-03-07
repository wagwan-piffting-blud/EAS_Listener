use crate::config::Config;
use crate::e2t_ng::ParsedEasSerialized;
use crate::filter;
use crate::monitoring::MonitoringHub;
use crate::recording::{self, RecordingState};
use crate::relay::RelayState;
use crate::state::{ActiveAlert, AlertRecordingState, AppState, EasAlertData};
use crate::webhook::send_alert_webhook;
use anyhow::{anyhow, Result};
use chrono::Utc;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::{mpsc::Receiver, Mutex};
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

const IMPACT_DAY_FILE: &str = "impact_day.txt";
const SEVERE_DAY_FILE: &str = "severe_day.txt";
const ACTIVE_ALERTS_FILE: &str = "active_alerts.json";
const ALERT_DEDUP_WINDOW: Duration = Duration::from_secs(15 * 60);
const ALERT_DEDUP_PRUNE_INTERVAL: usize = 256;

#[inline]
fn is_severe_alert_event_code(event_code: &str) -> bool {
    matches!(
        event_code,
        "AVW"
            | "BZW"
            | "CFW"
            | "DSW"
            | "EWW"
            | "FFW"
            | "FLW"
            | "FRW"
            | "FSW"
            | "FZW"
            | "HUW"
            | "HWW"
            | "SMW"
            | "SQW"
            | "SSW"
            | "SVR"
            | "TOR"
            | "TRW"
            | "TSW"
            | "WSW"
    )
}

#[inline]
fn is_impact_day_event_code(event_code: &str) -> bool {
    matches!(
        event_code,
        "AVA"
            | "CFA"
            | "FFA"
            | "FLA"
            | "HUA"
            | "HWA"
            | "SSA"
            | "SVA"
            | "TOA"
            | "TRA"
            | "TSA"
            | "WSA"
    )
}

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

#[derive(Debug, Clone)]
struct AlertDedupEntry {
    received_at: Instant,
}

#[inline]
fn dedup_key_without_sender(raw_header: &str) -> Option<(String, String)> {
    let trimmed = raw_header.trim().trim_end_matches('-');
    let (prefix, sender_id) = trimmed.rsplit_once('-')?;
    if prefix.is_empty() || sender_id.is_empty() {
        return None;
    }

    let mut key = String::with_capacity(prefix.len() + 1);
    key.push_str(prefix);
    key.push('-');

    Some((key, sender_id.to_string()))
}

#[inline]
fn prune_dedup_cache(cache: &mut HashMap<String, AlertDedupEntry>, now: Instant) {
    cache.retain(|_, entry| now.duration_since(entry.received_at) < ALERT_DEDUP_WINDOW);
}

#[inline]
fn should_process_alert(
    cache: &mut HashMap<String, AlertDedupEntry>,
    raw_header: &str,
    preferred_senderid: &str,
    now: Instant,
) -> bool {
    let Some((dedup_key, sender_id)) = dedup_key_without_sender(raw_header) else {
        return true;
    };

    let preferred = preferred_senderid.trim();
    let incoming_is_preferred = !preferred.is_empty() && sender_id.eq_ignore_ascii_case(preferred);
    if incoming_is_preferred {
        cache.insert(
            dedup_key,
            AlertDedupEntry {
                received_at: now,
            },
        );
        return true;
    }

    if let Some(existing) = cache.get_mut(&dedup_key) {
        if now.duration_since(existing.received_at) < ALERT_DEDUP_WINDOW {
            existing.received_at = now;
            return false;
        }
    }

    cache.insert(
        dedup_key,
        AlertDedupEntry {
            received_at: now,
        },
    );
    true
}

async fn read_persisted_active_alerts(state_dir: &Path) -> Result<Vec<ActiveAlert>> {
    let persisted_path = state_dir.join(ACTIVE_ALERTS_FILE);
    if !fs::try_exists(&persisted_path).await? {
        return Ok(Vec::new());
    }

    let bytes = fs::read(&persisted_path).await?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let alerts = serde_json::from_slice::<Vec<ActiveAlert>>(&bytes).map_err(|err| {
        anyhow!(
            "Failed to parse persisted active alerts from {}: {}",
            persisted_path.display(),
            err
        )
    })?;
    Ok(alerts)
}

async fn restore_active_alert_state(
    state_dir: &Path,
    state: &Arc<Mutex<AppState>>,
) -> Result<Option<Vec<ActiveAlert>>> {
    let mut persisted_alerts = read_persisted_active_alerts(state_dir).await?;
    let now = Utc::now();
    persisted_alerts.retain(|alert| alert.expires_at > now);
    for alert in &mut persisted_alerts {
        if matches!(alert.recording_state, AlertRecordingState::Pending) {
            alert.recording_state = if alert.recording_file_name.is_some() {
                AlertRecordingState::Ready
            } else {
                AlertRecordingState::Missing
            };
        }
    }

    let mut app_state_guard = state.lock().await;
    let initial_len = app_state_guard.active_alerts.len();
    app_state_guard
        .active_alerts
        .retain(|alert| alert.expires_at > now);

    let mut known_headers = app_state_guard
        .active_alerts
        .iter()
        .map(|alert| alert.raw_header.clone())
        .collect::<HashSet<_>>();

    for alert in persisted_alerts {
        if known_headers.insert(alert.raw_header.clone()) {
            app_state_guard.active_alerts.push(alert);
        }
    }

    let changed = app_state_guard.active_alerts.len() != initial_len;
    if changed {
        update_alert_files(state_dir, &app_state_guard).await?;
        return Ok(Some(app_state_guard.active_alerts.clone()));
    }

    Ok(None)
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
    match restore_active_alert_state(&config.shared_state_dir, &state).await {
        Ok(Some(alert_snapshot)) => {
            info!(
                "Restored {} active alert(s) from persisted state.",
                alert_snapshot.len()
            );
            monitoring.broadcast_alerts(alert_snapshot, None, None);
        }
        Ok(None) => {}
        Err(err) => warn!("Failed restoring active alerts from disk: {}", err),
    }

    let mut reload_enabled = true;
    let mut dedup_cache: HashMap<String, AlertDedupEntry> = HashMap::new();
    let mut dedup_prune_counter = 0usize;

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
                        match restore_active_alert_state(&config.shared_state_dir, &state).await {
                            Ok(Some(alert_snapshot)) => {
                                info!(
                                    "Restored {} active alert(s) from persisted state after reload.",
                                    alert_snapshot.len()
                                );
                                monitoring.broadcast_alerts(alert_snapshot, None, None);
                            }
                            Ok(None) => {}
                            Err(err) => {
                                warn!("Failed restoring active alerts after reload: {}", err)
                            }
                        }
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

        dedup_prune_counter += 1;
        let dedup_now = Instant::now();
        if dedup_prune_counter >= ALERT_DEDUP_PRUNE_INTERVAL {
            dedup_prune_counter = 0;
            prune_dedup_cache(&mut dedup_cache, dedup_now);
        }

        if !should_process_alert(
            &mut dedup_cache,
            &raw_header,
            &config.preferred_senderid,
            dedup_now,
        ) {
            info!(
                "Skipping duplicate alert within dedup window: {}",
                &raw_header
            );
            continue;
        }

        let action = {
            let guard = state.lock().await;
            let filters = guard.cloned_filters();
            filter::evaluate_action(filters.as_slice(), &event)
        };

        if action == filter::FilterAction::Ignore {
            info!(
                "Ignoring alert due to filter action=ignore: {}",
                &raw_header
            );
            continue;
        }

        info!("Processing alert: {}", &raw_header);

        let dsame_result =
            get_eas_details_and_log(&config, &raw_header, &event, &locations, &originator).await;
        let alert_data = match &dsame_result {
            Ok(data) => data.clone(),
            Err(_) => EasAlertData {
                eas_text: "EAS decode failed.".to_string(),
                event_text: event.clone(),
                event_code: event,
                fips: vec![],
                locations,
                originator,
                description: None,
                parsed_header: None,
            },
        };

        if is_alert_relevant(&alert_data, &config.watched_fips) {
            info!("Alert for watched zone(s) received. Relaying...");
            let alert = ActiveAlert::new(alert_data.clone(), raw_header.clone(), purge_time)
                .with_source_stream_url(stream_id.clone());

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
                Err(e) => format!("EAS decode failed: {}", e),
            };

            let value = handle_recording_and_webhook(
                config.clone(),
                state.clone(),
                monitoring.clone(),
                recording_state.clone(),
                alert,
                dsame_text,
                raw_header,
                purge_time,
                stream_id,
                action,
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

fn recording_file_name_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

async fn update_alert_recording_metadata(
    config: &Config,
    state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
    raw_header: &str,
    recording_state: AlertRecordingState,
    recording_file_name: Option<String>,
) {
    let active_snapshot = {
        let mut guard = state.lock().await;
        if !guard.update_alert_recording_metadata(raw_header, recording_state, recording_file_name)
        {
            return;
        }

        if let Err(err) = update_alert_files(&config.shared_state_dir, &guard).await {
            error!(
                "Failed to update alert files with recording metadata: {}",
                err
            );
        }

        guard.active_alerts.clone()
    };

    monitoring.broadcast_alerts(active_snapshot, None, None);
}

async fn handle_recording_and_webhook(
    config: Config,
    state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    alert: ActiveAlert,
    dsame_text: String,
    raw_header: String,
    _purge_time: Duration,
    stream_id: String,
    action: filter::FilterAction,
    mut nnnn_rx: BroadcastReceiver<String>,
) {
    let event_code = alert.data.event_code.clone();
    let mut recorded_state: Option<(PathBuf, String)> = None;
    let mut join_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut initial_recording_metadata: Option<(AlertRecordingState, Option<String>)> = None;

    let mut recorder = recording_state.lock().await;
    if !recorder.contains_key(stream_id.as_str()) {
        match recording::start_encoding_task(&config, &raw_header, &stream_id) {
            Ok((handle, new_state)) => {
                info!("Recording started for alert: {}", event_code);
                recorder.insert(stream_id.clone(), new_state);
                join_handle = Some(handle);
            }
            Err(e) => {
                warn!("Failed to start recording: {}", e);
                initial_recording_metadata = Some((AlertRecordingState::Missing, None));
            }
        }
    } else {
        warn!(
            "Recording already active for stream {}; alert {} will not receive a dedicated recording.",
            stream_id, event_code
        );
        initial_recording_metadata = Some((AlertRecordingState::Missing, None));
    }
    drop(recorder);

    if let Some((recording_state_value, recording_file_name)) = initial_recording_metadata {
        update_alert_recording_metadata(
            &config,
            &state,
            &monitoring,
            &raw_header,
            recording_state_value,
            recording_file_name,
        )
        .await;
    }

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

        let final_recording_state = if recorded_state.is_some() {
            AlertRecordingState::Ready
        } else {
            AlertRecordingState::Missing
        };
        let final_recording_file_name = recorded_state
            .as_ref()
            .and_then(|(recording_path, _)| recording_file_name_from_path(recording_path));
        update_alert_recording_metadata(
            &config,
            &state,
            &monitoring,
            &raw_header,
            final_recording_state,
            final_recording_file_name,
        )
        .await;
    }

    if filter::should_forward_action(action) {
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
    }

    if action != filter::FilterAction::Relay {
        return;
    }

    if config.should_relay && (config.should_relay_icecast || config.should_relay_dasdec) {
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

async fn get_eas_details_and_log(
    config: &Config,
    raw_header: &str,
    _event_text: &str,
    locations: &str,
    _originator: &str,
) -> Result<EasAlertData> {
    let timezone = config.timezone.to_string();

    let parsed_json = crate::e2t_ng::parse_header_json(raw_header)
        .map_err(|err| anyhow!("Invalid EAS header format: {} ({})", raw_header, err))?;
    let parsed_header: ParsedEasSerialized = serde_json::from_str(&parsed_json)
        .map_err(|err| anyhow!("Failed to decode parsed EAS header JSON: {}", err))?;

    let eas_text = crate::e2t_ng::E2T(raw_header, "", false, Some(timezone.as_str()));

    if eas_text == "Invalid EAS header format" {
        anyhow::bail!("Invalid EAS header format: {}", raw_header);
    }

    let event_text = crate::webhook::determine_event_title(&parsed_header.event_code);

    let locations = if locations.trim().is_empty() {
        parsed_header.fips_codes.join(", ")
    } else {
        locations.to_string()
    };

    let originator = crate::webhook::determine_originator_name(&parsed_header.originator);

    let alert_data = EasAlertData {
        eas_text,
        event_text,
        event_code: parsed_header.event_code.clone(),
        fips: parsed_header.fips_codes.clone(),
        locations,
        originator,
        description: None,
        parsed_header: Some(parsed_header),
    };

    let watched_fips = &config.watched_fips;
    let write_anyways = config.should_log_all_alerts;
    let received_at = Utc::now();
    let local_time = received_at.with_timezone(&config.timezone);
    let timestamp = local_time.format("%Y-%m-%d %l:%M:%S %p");
    let log_line = format!(
        "{}: {} (Received @ {})\n\n",
        raw_header, alert_data.eas_text, timestamp
    );

    if is_alert_relevant(&alert_data, watched_fips) || write_anyways {
        info!("Logging alert to file: {}", log_line.trim());

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.dedicated_alert_log_file)
            .await?;
        file.write_all(log_line.as_bytes()).await?;
    } else {
        info!(
            "Alert not in watched FIPS (zones: {}). Skipping log write.",
            alert_data.locations
        );
    }

    Ok(alert_data)
}

#[instrument(skip(state_dir, app_state))]
pub async fn update_alert_files(state_dir: &Path, app_state: &AppState) -> Result<()> {
    let now = Utc::now();
    let active_alerts = app_state
        .active_alerts
        .iter()
        .filter(|alert| alert.expires_at > now)
        .cloned()
        .collect::<Vec<_>>();

    let active_alerts_path = state_dir.join(ACTIVE_ALERTS_FILE);
    let active_alerts_payload = serde_json::to_vec(&active_alerts)
        .map_err(|err| anyhow!("Failed to serialize active alerts: {}", err))?;
    fs::write(&active_alerts_path, active_alerts_payload).await?;

    let mut has_severe_alert = false;
    let mut has_impact_day_alert = false;
    for alert in &active_alerts {
        let event_code = alert.data.event_code.trim();
        if is_severe_alert_event_code(event_code) {
            has_severe_alert = true;
            break;
        }
        if is_impact_day_event_code(event_code) {
            has_impact_day_alert = true;
        }
    }

    let impact_path = state_dir.join(IMPACT_DAY_FILE);
    let severe_path = state_dir.join(SEVERE_DAY_FILE);

    if has_severe_alert {
        info!("Severe alert active. Ensuring `severe_day.txt` exists.");
        fs::write(&severe_path, "").await?;
        if fs::try_exists(&impact_path).await? {
            fs::remove_file(&impact_path).await?;
        }
    } else if has_impact_day_alert {
        info!("Impact day alert active. Ensuring `impact_day.txt` exists.");
        fs::write(&impact_path, "").await?;
        if fs::try_exists(&severe_path).await? {
            fs::remove_file(&severe_path).await?;
        }
    } else {
        info!("No relevant alerts active. Cleaning up state files.");
        if fs::try_exists(&impact_path).await? {
            fs::remove_file(&impact_path).await?;
        }
        if fs::try_exists(&severe_path).await? {
            fs::remove_file(&severe_path).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_alert_data(event_code: &str, fips: &[&str]) -> EasAlertData {
        EasAlertData {
            eas_text: "sample text".to_string(),
            event_text: "Sample Event".to_string(),
            event_code: event_code.to_string(),
            fips: fips.iter().map(|value| value.to_string()).collect(),
            locations: "Sample Location".to_string(),
            originator: "WXR".to_string(),
            description: None,
            parsed_header: None,
        }
    }

    #[test]
    fn classify_severe_and_impact_codes() {
        assert!(is_severe_alert_event_code("TOR"));
        assert!(!is_severe_alert_event_code("RWT"));
        assert!(is_impact_day_event_code("TOA"));
        assert!(!is_impact_day_event_code("SVR"));
    }

    #[test]
    fn alert_relevance_respects_watched_fips() {
        let alert = sample_alert_data("TOR", &["031055", "031153"]);

        let empty = HashSet::new();
        assert!(is_alert_relevant(&alert, &empty));

        let mut watched = HashSet::new();
        watched.insert("031055".to_string());
        assert!(is_alert_relevant(&alert, &watched));

        watched.clear();
        watched.insert("000000".to_string());
        assert!(is_alert_relevant(&alert, &watched));

        watched.clear();
        watched.insert("999999".to_string());
        assert!(!is_alert_relevant(&alert, &watched));
    }

    #[test]
    fn dedup_key_without_sender_extracts_key_and_sender() {
        let header = "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-";
        let (key, sender) = dedup_key_without_sender(header).expect("dedup key");
        assert_eq!(key, "ZCZC-WXR-TOR-031055+0030-1231645-");
        assert_eq!(sender, "KWO35");
        assert!(dedup_key_without_sender("invalid").is_none());
    }

    #[test]
    fn should_process_alert_prefers_preferred_senderid() {
        let mut cache = HashMap::new();
        let now = Instant::now();
        let raw_1 = "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-";
        let raw_2 = "ZCZC-WXR-TOR-031055+0030-1231645-KIH61-";

        assert!(should_process_alert(&mut cache, raw_1, "KIH61", now));
        assert!(!should_process_alert(
            &mut cache,
            raw_1,
            "KIH61",
            now + Duration::from_secs(5)
        ));

        assert!(should_process_alert(
            &mut cache,
            raw_2,
            "KIH61",
            now + Duration::from_secs(10)
        ));

        assert!(!should_process_alert(
            &mut cache,
            raw_1,
            "KIH61",
            now + Duration::from_secs(11)
        ));
    }

    #[test]
    fn should_process_alert_always_processes_preferred_sender_duplicates() {
        let mut cache = HashMap::new();
        let now = Instant::now();
        let raw = "ZCZC-WXR-TOR-031055+0030-1231645-KIH61-";

        assert!(should_process_alert(&mut cache, raw, "KIH61", now));
        assert!(should_process_alert(
            &mut cache,
            raw,
            "KIH61",
            now + Duration::from_secs(5)
        ));
    }

    #[test]
    fn prune_dedup_cache_removes_stale_entries() {
        let mut cache = HashMap::new();
        let now = Instant::now();
        cache.insert(
            "recent".to_string(),
            AlertDedupEntry {
                received_at: now - Duration::from_secs(30),
            },
        );
        cache.insert(
            "stale".to_string(),
            AlertDedupEntry {
                received_at: now - ALERT_DEDUP_WINDOW - Duration::from_secs(1),
            },
        );

        prune_dedup_cache(&mut cache, now);
        assert!(cache.contains_key("recent"));
        assert!(!cache.contains_key("stale"));
    }
}
