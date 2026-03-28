use crate::alerts::update_alert_files;
use crate::config::Config;
use crate::filter::{self, FilterAction};
use crate::header;
use crate::monitoring::MonitoringHub;
use crate::relay::RelayState;
use crate::state::{ActiveAlert, AlertRecordingState, AppState, EasAlertData};
use crate::webhook::send_alert_webhook;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use hound::{WavSpec, WavWriter};
use roxmltree::{Document, Node};
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

const CAP_POLL_INTERVAL_SECS: u64 = 60;
const CAP_HTTP_TIMEOUT_SECS: u64 = 10;
const CAP_DEFAULT_PURGE_SECS: u64 = 30 * 60;
const CAP_SEEN_DEFAULT_TTL_SECS: i64 = 6 * 60 * 60;
const CAP_FORBIDDEN_SKIP_TTL_SECS: i64 = 24 * 60 * 60;
const CAP_PARSE_ERROR_SKIP_TTL_SECS: i64 = 24 * 60 * 60;
const CAP_AUDIO_MAX_BYTES: usize = 25 * 1024 * 1024;
const CAP_RECORDING_SAMPLE_RATE: u32 = 48_000;
const CAP_HEADER_AMPLITUDE: f64 = 0.42;
const CAP_TTS_WINE_PATH: &str = "/usr/lib/wine/wine";
const CAP_TTS_DUMPER_PATH: &str = "/app/Speechify/bin/spfy_dumpwav.exe";
const CAP_TTS_REPLACEMENT_DICT_PATH: &str = "/app/cap_tts_replacement_config.json";
const CAP_ACTIVE_ALERTS_FILE: &str = "active_alerts.json";
const CAP_HEADER_SOURCE_MARKER_CAP: &str = "IPAWSCAP";
const CAP_HEADER_SOURCE_MARKER_WEA: &str = "IPAWSWEA";

static CAP_TTS_SYNTH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CapAlert {
    identifier: String,
    originator_code: String,
    sender: String,
    sender_name: Option<String>,
    sent: Option<DateTime<Utc>>,
    expires: Option<DateTime<Utc>>,
    msg_type: String,
    scope: String,
    event_text: String,
    event_code: String,
    urgency: Option<String>,
    severity: Option<String>,
    certainty: Option<String>,
    description: String,
    description_raw: String,
    instructions: Option<String>,
    simple_description: String,
    areas: Vec<String>,
    fips: Vec<String>,
    audio_uri: Option<String>,
    audio_deref_uri: Option<String>,
    audio_mime_type: Option<String>,
    source_url: String,
}

fn spawn_cap_processor_task(
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_cap_processor(config, app_state, monitoring).await {
            warn!("CAP processor task exited with error: {}", err);
        }
    })
}

async fn sync_cap_runtime_config_status(app_state: &Arc<Mutex<AppState>>, config: &Config) {
    let mut guard = app_state.lock().await;
    guard.cap_status.enabled = config.process_cap_alerts;
    guard.cap_status.endpoint_count = config.cap_endpoints.len();
    guard.cap_status.endpoints = config.cap_endpoints.clone();
}

pub async fn run_cap_supervisor(
    initial_config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    mut reload_rx: broadcast::Receiver<Config>,
) -> Result<()> {
    let mut current_config = initial_config;
    sync_cap_runtime_config_status(&app_state, &current_config).await;
    let mut cap_task: Option<JoinHandle<()>> = if current_config.process_cap_alerts {
        Some(spawn_cap_processor_task(
            current_config.clone(),
            app_state.clone(),
            monitoring.clone(),
        ))
    } else {
        info!("CAP processor disabled because PROCESS_CAP_ALERTS is false in your config.json file. No CAP alerts will be processed or forwarded to webhooks.");
        None
    };

    loop {
        match reload_rx.recv().await {
            Ok(new_config) => {
                current_config = new_config;
                sync_cap_runtime_config_status(&app_state, &current_config).await;

                if let Some(task) = cap_task.take() {
                    task.abort();
                    match task.await {
                        Ok(_) => {}
                        Err(err) if err.is_cancelled() => {}
                        Err(err) => warn!("CAP processor task join error: {}", err),
                    }
                }

                if current_config.process_cap_alerts {
                    info!("CAP processor configuration reloaded; restarting CAP processor task.");
                    cap_task = Some(spawn_cap_processor_task(
                        current_config.clone(),
                        app_state.clone(),
                        monitoring.clone(),
                    ));
                } else {
                    info!("CAP processor disabled by reloaded configuration.");
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    "CAP supervisor lagged on config updates (skipped {} message(s)); waiting for next update.",
                    skipped
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    if let Some(task) = cap_task.take() {
        task.abort();
        let _ = task.await;
    }

    Ok(())
}

pub async fn run_cap_processor(
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
) -> Result<()> {
    if !config.process_cap_alerts {
        info!("CAP processor disabled by configuration.");
        return Ok(());
    }

    if config.cap_endpoints.is_empty() {
        warn!("CAP processor enabled but CAP_ENDPOINTS is empty; CAP monitoring will not run.");
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(CAP_HTTP_TIMEOUT_SECS))
        .pool_max_idle_per_host(0)
        .build()
        .context("Failed to create CAP HTTP client")?;

    let mut seen_alerts: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut persisted_active_dedupe_keys =
        load_persisted_active_dedupe_keys(&config.shared_state_dir).await;
    let mut ticker = interval(Duration::from_secs(CAP_POLL_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    info!(
        "CAP processor started with {} endpoint(s).",
        config.cap_endpoints.len()
    );

    loop {
        ticker.tick().await;

        let now = Utc::now();
        seen_alerts.retain(|_, expires_at| *expires_at > now);

        for endpoint in &config.cap_endpoints {
            let endpoint_url = endpoint.url.as_str();
            let poll_time = Utc::now();
            {
                let mut guard = app_state.lock().await;
                guard.cap_status.last_poll_at = Some(poll_time);
                guard.cap_status.polls_attempted =
                    guard.cap_status.polls_attempted.saturating_add(1);
            }

            debug!("Polling CAP endpoint {}", endpoint_url);
            let feed_xml = match fetch_text(&client, endpoint_url).await {
                Ok(xml) => {
                    {
                        let mut guard = app_state.lock().await;
                        guard.cap_status.last_successful_poll_at = Some(poll_time);
                        guard.cap_status.last_poll_error = None;
                    }
                    debug!(
                        "Fetched CAP endpoint {} successfully ({} bytes)",
                        endpoint_url,
                        xml.len()
                    );
                    xml
                }
                Err(err) => {
                    let err_text = err.to_string();
                    {
                        let mut guard = app_state.lock().await;
                        guard.cap_status.polls_failed =
                            guard.cap_status.polls_failed.saturating_add(1);
                        guard.cap_status.last_poll_error = Some(err_text.clone());
                    }
                    warn!("Failed to fetch CAP endpoint {}: {}", endpoint_url, err);
                    continue;
                }
            };

            let alert_sources = if looks_like_alert_xml(&feed_xml) {
                debug!(
                    "CAP endpoint {} returned an alert document directly",
                    endpoint_url
                );
                vec![(endpoint_url.to_string(), feed_xml)]
            } else if let Some(alerts) = match parse_inline_alert_documents(&feed_xml, endpoint_url)
            {
                Ok(alerts) => alerts,
                Err(err) => {
                    warn!(
                        "Failed to parse embedded CAP alerts from {}: {}",
                        endpoint_url, err
                    );
                    continue;
                }
            } {
                debug!(
                    "Parsed {} embedded CAP alert(s) from {}",
                    alerts.len(),
                    endpoint_url
                );
                alerts
            } else {
                let links = match parse_feed_alert_links(&feed_xml) {
                    Ok(links) => {
                        debug!(
                            "Parsed {} CAP alert link(s) from {}",
                            links.len(),
                            endpoint_url
                        );
                        links
                    }
                    Err(err) => {
                        warn!("Failed to parse CAP feed {}: {}", endpoint_url, err);
                        continue;
                    }
                };

                if links.is_empty() {
                    debug!("No CAP entries found at endpoint {}", endpoint_url);
                    continue;
                }

                let mut alerts = Vec::with_capacity(links.len());
                for link in links {
                    let url_seen_key = format!("url:{link}");
                    if seen_alerts.contains_key(&url_seen_key) {
                        debug!("Skipping already-seen CAP alert URL {}", link);
                        continue;
                    }

                    match fetch_text(&client, &link).await {
                        Ok(alert_xml) => {
                            debug!("Fetched CAP alert {} ({} bytes)", link, alert_xml.len());
                            alerts.push((link, alert_xml));
                        }
                        Err(err) => {
                            if is_http_status(&err, reqwest::StatusCode::FORBIDDEN) {
                                let until = Utc::now()
                                    + ChronoDuration::seconds(CAP_FORBIDDEN_SKIP_TTL_SECS);
                                seen_alerts.insert(url_seen_key, until);
                                debug!(
                                    "Skipping CAP alert {} due to HTTP 403 (cached for {}s).",
                                    link, CAP_FORBIDDEN_SKIP_TTL_SECS
                                );
                            } else {
                                warn!("Failed to fetch CAP alert {}: {}", link, err);
                            }
                        }
                    }
                }
                alerts
            };

            for (alert_url, alert_xml) in alert_sources {
                let url_seen_key = format!("url:{}", alert_url);
                if seen_alerts.contains_key(&url_seen_key) {
                    debug!("Skipping already-seen CAP alert URL {}", alert_url);
                    continue;
                }

                let parsed = match parse_cap_alert(&alert_xml, &alert_url) {
                    Ok(alert) => {
                        debug!(
                            "Parsed CAP alert {} successfully (identifier={}, event_code={})",
                            alert_url, alert.identifier, alert.event_code
                        );
                        alert
                    }
                    Err(err) => {
                        warn!(
                            "Failed to parse CAP alert {} : {}, marking as seen",
                            alert_url, err
                        );

                        let dedupe_key = format!(
                            "parse-error:id:{}|url:{}",
                            parsed_identifier_from_url(&alert_url),
                            alert_url
                        );

                        if seen_alerts.contains_key(&dedupe_key) {
                            debug!(
                                "Skipping CAP alert {} (identifier={}) because it is already seen (dedupe key={})",
                                alert_url, parsed_identifier_from_url(&alert_url), dedupe_key
                            );
                            continue;
                        }

                        let seen_until =
                            Utc::now() + ChronoDuration::seconds(CAP_PARSE_ERROR_SKIP_TTL_SECS);
                        seen_alerts.insert(dedupe_key, seen_until);
                        seen_alerts.insert(url_seen_key, seen_until);
                        continue;
                    }
                };

                let dedupe_key = build_dedupe_key(&parsed);
                if seen_alerts.contains_key(&dedupe_key) {
                    debug!(
                        "Skipping CAP alert {} (identifier={}) because it is already seen (dedupe key={})",
                        alert_url, parsed.identifier, dedupe_key
                    );
                    continue;
                }

                let now = Utc::now();
                if let Some(expires_at) = parsed.expires {
                    if expires_at <= now {
                        let seen_until = now + ChronoDuration::seconds(CAP_SEEN_DEFAULT_TTL_SECS);
                        debug!(
                            "Skipping expired CAP alert {} (identifier={}, event_code={}) expired_at={} now={} (cached for {}s)",
                            alert_url,
                            parsed.identifier,
                            parsed.event_code,
                            expires_at.to_rfc3339(),
                            now.to_rfc3339(),
                            CAP_SEEN_DEFAULT_TTL_SECS
                        );
                        seen_alerts.insert(dedupe_key, seen_until);
                        seen_alerts.insert(url_seen_key, seen_until);
                        continue;
                    }
                }

                if persisted_active_dedupe_keys.contains(&dedupe_key)
                    || cap_alert_is_active(&app_state, &dedupe_key).await
                {
                    let seen_until = match parsed.expires {
                        Some(expires_at) if expires_at > Utc::now() => expires_at,
                        _ => Utc::now() + ChronoDuration::seconds(CAP_SEEN_DEFAULT_TTL_SECS),
                    };
                    debug!(
                        "Skipping CAP alert {} (identifier={}, event_code={}) because it already exists in active state",
                        alert_url, parsed.identifier, parsed.event_code
                    );
                    seen_alerts.insert(dedupe_key, seen_until);
                    seen_alerts.insert(url_seen_key, seen_until);
                    continue;
                }

                debug!(
                    "Beginning CAP alert processing for {} (identifier={}, event_code={})",
                    alert_url, parsed.identifier, parsed.event_code
                );
                process_cap_alert(
                    &config,
                    &app_state,
                    &monitoring,
                    &client,
                    endpoint_url,
                    parsed.clone(),
                )
                .await;

                update_alert_files(&config.shared_state_dir, &*app_state.lock().await)
                    .await
                    .ok();

                debug!(
                    "Finished CAP alert processing for {} (identifier={}, event_code={})",
                    alert_url, parsed.identifier, parsed.event_code
                );

                let seen_until = match parsed.expires {
                    Some(expires_at) if expires_at > Utc::now() => expires_at,
                    _ => Utc::now() + ChronoDuration::seconds(CAP_SEEN_DEFAULT_TTL_SECS),
                };
                persisted_active_dedupe_keys.insert(dedupe_key.clone());
                seen_alerts.insert(dedupe_key, seen_until);
                seen_alerts.insert(url_seen_key, seen_until);
            }
        }
    }
}

fn parsed_identifier_from_url(url: &str) -> String {
    if let Some((_, fragment)) = url.rsplit_once('#') {
        let fragment = fragment.trim();
        if !fragment.is_empty() {
            return fragment.to_string();
        }
    }

    url.rsplit('/')
        .next()
        .unwrap_or(url)
        .split('.')
        .next()
        .unwrap_or(url)
        .to_string()
}

async fn load_persisted_active_dedupe_keys(shared_state_dir: &Path) -> HashSet<String> {
    let path = shared_state_dir.join(CAP_ACTIVE_ALERTS_FILE);
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashSet::new(),
        Err(err) => {
            warn!(
                "Failed reading persisted CAP active alerts from {}: {}",
                path.display(),
                err
            );
            return HashSet::new();
        }
    };

    if bytes.is_empty() {
        return HashSet::new();
    }

    let alerts = match serde_json::from_slice::<Vec<ActiveAlert>>(&bytes) {
        Ok(alerts) => alerts,
        Err(err) => {
            warn!(
                "Failed parsing persisted CAP active alerts from {}: {}",
                path.display(),
                err
            );
            return HashSet::new();
        }
    };

    let now = Utc::now();
    alerts
        .into_iter()
        .filter(|alert| alert.expires_at > now)
        .filter_map(|alert| build_dedupe_key_from_raw_header(&alert.raw_header))
        .collect()
}

fn active_alert_has_dedupe_key(alerts: &[ActiveAlert], dedupe_key: &str) -> bool {
    let now = Utc::now();
    alerts.iter().any(|alert| {
        alert.expires_at > now
            && build_dedupe_key_from_raw_header(&alert.raw_header).as_deref() == Some(dedupe_key)
    })
}

async fn cap_alert_is_active(app_state: &Arc<Mutex<AppState>>, dedupe_key: &str) -> bool {
    let guard = app_state.lock().await;
    active_alert_has_dedupe_key(&guard.active_alerts, dedupe_key)
}

fn recording_file_name_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

async fn update_cap_alert_recording_metadata(
    config: &Config,
    app_state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
    raw_header: &str,
    recording_state: AlertRecordingState,
    recording_file_name: Option<String>,
) {
    let active_snapshot = {
        let mut guard = app_state.lock().await;
        if !guard.update_alert_recording_metadata(raw_header, recording_state, recording_file_name)
        {
            return;
        }

        if let Err(err) = update_alert_files(&config.shared_state_dir, &guard).await {
            warn!(
                "Failed to update alert files with CAP recording metadata for {}: {}",
                raw_header, err
            );
        }

        guard.active_alerts.clone()
    };

    monitoring.broadcast_alerts(active_snapshot, None, None);
}

async fn process_cap_alert(
    config: &Config,
    app_state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
    client: &reqwest::Client,
    source_stream: &str,
    alert: CapAlert,
) {
    let event_code = normalize_event_code(&alert.event_code);

    let filters = {
        let guard = app_state.lock().await;
        guard.cloned_filters()
    };
    let action = filter::evaluate_action(filters.as_slice(), &event_code);
    if action == FilterAction::Ignore {
        debug!(
            "Skipping CAP alert {} ({}) due to filter action=ignore",
            alert.identifier, event_code
        );
        return;
    }

    let cap_relevant = is_cap_relevant(&alert.fips, &config.watched_fips);
    let should_log_cap_entry =
        filter::should_log_action(action) && (cap_relevant || config.should_log_all_alerts);
    if should_log_cap_entry {
        if let Err(err) = append_cap_log(config, &alert).await {
            warn!("Failed to append CAP log entry: {}", err);
        }
    }

    if !cap_relevant {
        debug!(
            "Skipping CAP alert {} ({}) because FIPS {:?} does not match watched set",
            alert.identifier, event_code, alert.fips
        );
        return;
    }

    let raw_header = build_cap_raw_header(
        &alert.originator_code,
        &event_code,
        &alert.fips,
        alert.sent,
        alert.expires,
        &alert.source_url,
    );
    let parsed_header = crate::e2t_ng::parse_header_json(raw_header.as_str())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok());
    let purge_time = determine_purge_time(alert.expires);
    let timezone = config.timezone.to_string();
    let eas_text = build_eas_text(&alert, timezone.as_str());
    let locations = if alert.areas.is_empty() {
        "Unknown".to_string()
    } else {
        alert.areas.join(", ")
    };

    let alert_data = EasAlertData {
        eas_text: eas_text.clone(),
        event_text: alert.event_text.clone(),
        event_code: event_code.clone(),
        fips: alert.fips.clone(),
        locations,
        originator: alert
            .sender_name
            .clone()
            .unwrap_or_else(|| alert.sender.clone()),
        description: Some(alert.simple_description.clone()),
        parsed_header,
    };

    let active_alert = ActiveAlert::new(alert_data, raw_header.clone(), purge_time)
        .with_source_stream_url(source_stream.to_string());

    let active_snapshot = {
        let mut guard = app_state.lock().await;
        let now = Utc::now();
        guard
            .active_alerts
            .retain(|existing| existing.expires_at > now && existing.raw_header != raw_header);
        guard.active_alerts.push(active_alert.clone());
        guard.cap_status.last_alert_received_at = Some(active_alert.received_at);
        guard.cap_status.last_alert_event_code = Some(event_code.clone());
        guard.cap_status.last_alert_source = Some(source_stream.to_string());
        guard.cap_status.alerts_processed = guard.cap_status.alerts_processed.saturating_add(1);
        guard.active_alerts.clone()
    };

    monitoring.broadcast_alerts(active_snapshot, Some(source_stream), Some(&event_code));

    let cap_recording_path =
        match fetch_cap_audio_recording(client, config, &alert, &raw_header, &event_code).await {
            Ok(path) => path,
            Err(err) => {
                warn!(
                    "Failed to process CAP audio for alert {} ({}): {}",
                    alert.identifier, event_code, err
                );
                None
            }
        };

    let recording_state = if cap_recording_path.is_some() {
        AlertRecordingState::Ready
    } else {
        AlertRecordingState::Missing
    };
    let recording_file_name = cap_recording_path
        .as_ref()
        .and_then(|path| recording_file_name_from_path(path));
    update_cap_alert_recording_metadata(
        config,
        app_state,
        monitoring,
        &raw_header,
        recording_state.clone(),
        recording_file_name.clone(),
    )
    .await;

    let mut alert_for_webhook = active_alert.clone();
    let _ = alert_for_webhook.update_recording_metadata(recording_state, recording_file_name);

    if cap_recording_path.is_none() {
        debug!(
            "CAP alert {} ({}) has no usable audio payload/recording",
            alert.identifier, event_code
        );
    }

    if filter::should_forward_action(action) {
        send_alert_webhook(
            source_stream,
            &alert_for_webhook,
            &eas_text,
            &raw_header,
            cap_recording_path.clone(),
        )
        .await;
    }

    if action == FilterAction::Relay && config.should_relay {
        info!("CAP alert for watched zone(s) received. Relaying...");
        if let Some(recording_path) = cap_recording_path {
            match RelayState::new(config.clone()).await {
                Ok(relay_state) => {
                    if let Err(err) = relay_state
                        .start_relay(
                            event_code.as_str(),
                            filters.as_slice(),
                            &recording_path,
                            Some(source_stream),
                            &raw_header,
                        )
                        .await
                    {
                        warn!("CAP relay failed for {}: {}", event_code, err);
                    }
                }
                Err(err) => warn!("Skipping CAP relay due to config error: {}", err),
            }
        } else {
            info!(
                "CAP alert {} matched relay action, but no CAP audio resource was available.",
                event_code
            );
        }
    }

    debug!(
        "CAP alert {} ({}) processing completed",
        alert.identifier, event_code
    );
}

fn parse_feed_alert_links(xml: &str) -> Result<Vec<String>> {
    let doc = match Document::parse(xml) {
        Ok(doc) => doc,
        Err(err) => {
            debug!(
                "CAP feed XML parse error: {} ({} bytes, snippet: {:?})",
                err,
                xml.len(),
                xml_snippet(xml, 220)
            );
            return Err(anyhow!("Invalid CAP feed XML: {}", err));
        }
    };
    let mut links = Vec::new();

    for entry in doc
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "entry")
    {
        let mut found = false;
        for link_node in entry
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "link")
        {
            if let Some(href) = link_node
                .attribute("href")
                .map(str::trim)
                .filter(|href| !href.is_empty())
            {
                links.push(href.to_string());
                found = true;
                break;
            }
        }

        if !found {
            if let Some(id) = entry
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == "id")
                .and_then(|node| node.text())
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                links.push(id.to_string());
            }
        }
    }

    links.sort();
    links.dedup();
    Ok(links)
}

fn parse_inline_alert_documents(
    xml: &str,
    endpoint_url: &str,
) -> Result<Option<Vec<(String, String)>>> {
    if !xml.contains("<alert") {
        return Ok(None);
    }

    let doc = match Document::parse(xml) {
        Ok(doc) => doc,
        Err(err) => {
            return Err(anyhow!("Invalid CAP alert collection XML: {}", err));
        }
    };

    let mut alerts = Vec::new();
    let mut seen_sources = HashSet::new();
    for (index, alert_node) in doc
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "alert")
        .enumerate()
    {
        let range = alert_node.range();
        if range.start >= range.end || range.end > xml.len() {
            continue;
        }

        let alert_xml = xml[range.start..range.end].trim();
        if alert_xml.is_empty() {
            continue;
        }

        let identifier = child_text(alert_node, "identifier")
            .unwrap_or_else(|| format!("embedded-alert-{}", index + 1));
        let source = format!("{endpoint_url}#{}", identifier.trim());
        if seen_sources.insert(source.clone()) {
            alerts.push((source, alert_xml.to_string()));
        }
    }

    if alerts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(alerts))
    }
}

fn simple_sanitize_description(description: &str) -> String {
    let mut return_value = description.trim().to_string();

    return_value = return_value.replace("\n", " ");
    return_value = return_value
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if let Some(nws_start) = return_value.find("The National Weather Service") {
        let prefix = &return_value[..nws_start];
        if !prefix.trim().is_empty()
            && prefix
                .lines()
                .all(|line| line.trim().is_empty() || is_nws_leading_code_line(line.trim()))
        {
            return_value = return_value[nws_start..].to_string();
        }
    }

    return_value.retain(|ch| ch != '*' && ch != '\r');

    return_value
}

fn parse_cap_alert(xml: &str, source_url: &str) -> Result<CapAlert> {
    let doc = match Document::parse(xml) {
        Ok(doc) => doc,
        Err(err) => {
            debug!(
                "CAP alert XML parse error for {}: {} ({} bytes, snippet: {:?})",
                source_url,
                err,
                xml.len(),
                xml_snippet(xml, 220)
            );
            return Err(anyhow!("Invalid CAP alert XML: {}", err));
        }
    };
    let root = doc.root_element();

    if root.tag_name().name() != "alert" {
        debug!(
            "CAP alert XML at {} has unexpected root <{}>",
            source_url,
            root.tag_name().name()
        );
        return Err(anyhow!("Expected <alert> root node"));
    }

    let identifier = child_text(root, "identifier").unwrap_or_else(|| source_url.to_string());
    let sender = child_text(root, "sender").unwrap_or_else(|| "Unknown sender".to_string());
    let mut sender_name = child_text(root, "senderName");

    let sent = child_text(root, "sent").as_deref().and_then(parse_cap_time);
    let msg_type = child_text(root, "msgType").unwrap_or_else(|| "Alert".to_string());

    if msg_type.eq_ignore_ascii_case("cancel") {
        debug!(
            "CAP alert {} is a cancellation message; skipping",
            source_url
        );
        return Err(anyhow!("CAP alert is a cancellation message"));
    }

    let scope = child_text(root, "scope").unwrap_or_else(|| "Public".to_string());

    let info_node = root
        .children()
        .find(|node| node.is_element() && node.tag_name().name() == "info")
        .ok_or_else(|| {
            debug!("CAP alert {} missing <info> section", source_url);
            anyhow!("CAP alert missing <info> section")
        })?;

    if sender_name.is_none() {
        sender_name = child_text(info_node, "senderName");
    }

    let same_event_code =
        extract_same_value(info_node, "eventCode").map(|value| normalize_event_code(&value));
    let (event_code, event_text) = match same_event_code {
        Some(code) => {
            let text = crate::webhook::determine_event_title(&code);
            (code, text)
        }
        None => {
            let text = child_text(info_node, "event").unwrap_or_else(|| "CAP Alert".to_string());
            let code = derive_event_code(&text);
            (code, text)
        }
    };
    let originator_code = extract_parameter_value(info_node, "EAS-ORG")
        .map(|value| normalize_originator_code(&value))
        .unwrap_or_else(|| "CIV".to_string());

    let urgency = child_text(info_node, "urgency");
    let severity = child_text(info_node, "severity");
    let certainty = child_text(info_node, "certainty");
    let instructions = child_text(info_node, "instruction");
    let cmam_long_text = extract_parameter_value(info_node, "CMAMlongtext")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let description_text = child_text(info_node, "description")
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let description_raw = cmam_long_text
        .or(description_text)
        .unwrap_or_else(|| "No CAP description provided.".to_string());
    let description = sanitize_cap_description(&description_raw);
    let expires = child_text(info_node, "expires")
        .as_deref()
        .and_then(parse_cap_time);
    let simple_description = simple_sanitize_description(&description_raw);

    let mut area_descs = Vec::new();
    let mut fips_codes = HashSet::new();
    let mut audio_uri = None;
    let mut audio_deref_uri = None;
    let mut audio_mime_type = None;

    for resource in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "resource")
    {
        let mime = child_text(resource, "mimeType");
        let uri = child_text(resource, "uri");
        let deref_uri = child_text(resource, "derefUri");
        if is_audio_resource(mime.as_deref(), uri.as_deref(), deref_uri.as_deref()) {
            audio_mime_type = mime;
            audio_uri = uri;
            audio_deref_uri = deref_uri;
            break;
        }
    }

    for area in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "area")
    {
        if let Some(area_desc) = child_text(area, "areaDesc") {
            area_descs.push(area_desc);
        }

        for geocode in area
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "geocode")
        {
            if let Some(same) = extract_same_from_container(geocode) {
                for fips in split_fips_codes(&same) {
                    fips_codes.insert(fips);
                }
            }
        }
    }

    let mut fips: Vec<String> = fips_codes.into_iter().collect();
    fips.sort();

    Ok(CapAlert {
        identifier,
        originator_code,
        sender,
        sender_name,
        sent,
        expires,
        msg_type,
        scope,
        event_text,
        event_code,
        urgency,
        severity,
        certainty,
        description,
        description_raw,
        simple_description,
        instructions,
        areas: area_descs,
        fips,
        audio_uri,
        audio_deref_uri,
        audio_mime_type,
        source_url: source_url.to_string(),
    })
}

fn sanitize_cap_description(description: &str) -> String {
    let mut working = description.trim();

    if let Some(nws_start) = working.find("The National Weather Service") {
        let prefix = &working[..nws_start];
        if !prefix.trim().is_empty()
            && prefix
                .lines()
                .all(|line| line.trim().is_empty() || is_nws_leading_code_line(line.trim()))
        {
            working = &working[nws_start..];
        }
    }

    let mut cleaned = String::with_capacity(working.len());
    for line in working.lines() {
        let mut line_buf = String::with_capacity(line.len());
        for ch in line.chars() {
            if ch != '*' && ch != '\r' {
                line_buf.push(ch);
            }
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        cleaned.push_str(trimmed);
    }

    if cleaned.is_empty() {
        return String::new();
    }

    let file_contents = match std::fs::read_to_string(CAP_TTS_REPLACEMENT_DICT_PATH) {
        Ok(contents) => contents,
        Err(err) => {
            warn!(
                "Failed to read CAP TTS replacement dictionary from {}: {}. No custom replacements will be applied.",
                CAP_TTS_REPLACEMENT_DICT_PATH, err
            );
            String::new()
        }
    };

    let replacements: HashMap<String, String> = match serde_json::from_str(&file_contents) {
        Ok(map) => map,
        Err(err) => {
            warn!(
                "Failed to parse CAP TTS replacement dictionary JSON from {}: {}. No custom replacements will be applied.",
                CAP_TTS_REPLACEMENT_DICT_PATH, err
            );
            HashMap::new()
        }
    };

    let mut replaced = cleaned.clone();

    for (target, replacement) in replacements {
        // Case-insensitive replacement: match the target regardless of case in the CAP text
        let re = match regex::RegexBuilder::new(&regex::escape(&target))
            .case_insensitive(true)
            .build()
        {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    "Failed to build regex for CAP TTS replacement target '{}': {}. Skipping.",
                    target, err
                );
                continue;
            }
        };
        replaced = re.replace_all(&replaced, replacement.as_str()).into_owned();
    }

    expand_cap_times_for_tts(&replaced)
}

fn is_nws_leading_code_line(line: &str) -> bool {
    let mut has_upper_alpha = false;
    for ch in line.chars() {
        if ch.is_ascii_lowercase() {
            return false;
        }
        if ch.is_ascii_uppercase() {
            has_upper_alpha = true;
            continue;
        }
        if ch.is_ascii_digit() || matches!(ch, ' ' | '-' | '_' | '/' | '.') {
            continue;
        }
        return false;
    }
    has_upper_alpha
}

fn expand_cap_times_for_tts(input: &str) -> String {
    let mut output = String::with_capacity(input.len() + 64);
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < input.len() {
        let byte = bytes[i];
        if byte.is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            if let Some((consumed, replacement)) = parse_spoken_time_at(&input[i..]) {
                output.push_str(&replacement);
                i += consumed;
                continue;
            }
        }

        let ch = input[i..].chars().next().unwrap_or_default();
        output.push(ch);
        i += ch.len_utf8();
    }
    output
}

fn parse_spoken_time_at(slice: &str) -> Option<(usize, String)> {
    let bytes = slice.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == 0 || idx > 4 {
        return None;
    }

    let digits = &slice[..idx];
    let mut cursor = idx;

    let ws_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor == ws_start || cursor + 2 > bytes.len() {
        return None;
    }

    let am_pm = if slice[cursor..cursor + 2].eq_ignore_ascii_case("AM") {
        "AM"
    } else if slice[cursor..cursor + 2].eq_ignore_ascii_case("PM") {
        "PM"
    } else {
        return None;
    };
    cursor += 2;

    if cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
        return None;
    }

    let ws_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor == ws_start {
        return None;
    }

    let tz_start = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
        cursor += 1;
    }
    if cursor == tz_start {
        return None;
    }

    let timezone = &slice[tz_start..cursor];
    let timezone_spoken = spoken_timezone_name(timezone)?;
    let (hour, minute) = parse_compact_time(digits)?;

    if cursor < bytes.len() && bytes[cursor].is_ascii_alphanumeric() {
        return None;
    }

    let expanded = format!("{hour}:{minute:02} {am_pm} {timezone_spoken}");
    Some((cursor, expanded))
}

fn parse_compact_time(value: &str) -> Option<(u8, u8)> {
    let parsed = value.parse::<u16>().ok()?;
    let (hour, minute) = match value.len() {
        1 | 2 => (parsed, 0),
        3 => (parsed / 100, parsed % 100),
        4 => (parsed / 100, parsed % 100),
        _ => return None,
    };
    if hour == 0 || hour > 12 || minute > 59 {
        return None;
    }
    Some((hour as u8, minute as u8))
}

fn spoken_timezone_name(tz: &str) -> Option<&'static str> {
    if tz.eq_ignore_ascii_case("EDT") {
        Some("Eastern Daylight Time")
    } else if tz.eq_ignore_ascii_case("EST") {
        Some("Eastern Standard Time")
    } else if tz.eq_ignore_ascii_case("CDT") {
        Some("Central Daylight Time")
    } else if tz.eq_ignore_ascii_case("CST") {
        Some("Central Standard Time")
    } else if tz.eq_ignore_ascii_case("MDT") {
        Some("Mountain Daylight Time")
    } else if tz.eq_ignore_ascii_case("MST") {
        Some("Mountain Standard Time")
    } else if tz.eq_ignore_ascii_case("PDT") {
        Some("Pacific Daylight Time")
    } else if tz.eq_ignore_ascii_case("PST") {
        Some("Pacific Standard Time")
    } else if tz.eq_ignore_ascii_case("AKDT") {
        Some("Alaska Daylight Time")
    } else if tz.eq_ignore_ascii_case("AKST") {
        Some("Alaska Standard Time")
    } else if tz.eq_ignore_ascii_case("HST") {
        Some("Hawaii Standard Time")
    } else if tz.eq_ignore_ascii_case("UTC") {
        Some("Coordinated Universal Time")
    } else if tz.eq_ignore_ascii_case("GMT") {
        Some("Greenwich Mean Time")
    } else {
        None
    }
}

async fn synthesize_cap_tts_audio(
    config: &Config,
    alert: &CapAlert,
    event_code: &str,
) -> Result<Option<PathBuf>> {
    info!(
        "Synthesizing CAP TTS audio for alert {} ({})",
        alert.identifier, event_code
    );
    let timezone = config.timezone.to_string();
    let alert_prefix_raw = build_eas_text(alert, timezone.as_str());

    let alert_prefix = if let Some((before_nws, after_nws)) =
        alert_prefix_raw.split_once("The National Weather Service in ")
    {
        if let Some((_, after_issue)) = after_nws.split_once("); has issued") {
            format!("{before_nws}The National Weather Service has issued{after_issue}")
        } else {
            alert_prefix_raw.clone()
        }
    } else {
        alert_prefix_raw.clone()
    };

    let alert_prefix = CAP_TTS_REPLACEMENT_DICT_PATH
        .parse::<PathBuf>()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|file_contents| {
            serde_json::from_str::<HashMap<String, String>>(&file_contents).ok()
        })
        .map(|replacements| {
            let mut replaced = alert_prefix.clone();
            for (target, replacement) in replacements {
                replaced = replaced.replace(&target, &replacement);
            }
            replaced
        })
        .unwrap_or_else(|| alert_prefix.clone());

    let description = alert.description.trim();

    let instructions = alert
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());

    if description.is_empty() {
        return Ok(None);
    }

    let tts_name = format!(
        "cap_tts_{}_{}_{}.wav",
        sanitize_filename_label(&alert.identifier),
        sanitize_filename_label(event_code),
        Utc::now().timestamp_millis()
    );
    let tts_path = config.recording_dir.join(tts_name);

    let tts_lock = cap_tts_synth_lock();
    let _tts_guard = match tts_lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            info!(
                "CAP TTS synthesis busy; queued alert {} ({})",
                alert.identifier, event_code
            );
            tts_lock.lock().await
        }
    };

    let status = Command::new(CAP_TTS_WINE_PATH)
        .arg(CAP_TTS_DUMPER_PATH)
        .arg(format!(
            "{} {} {}",
            alert_prefix,
            description,
            instructions.unwrap_or_default()
        ))
        .arg(&tts_path)
        .status()
        .await
        .context("Failed to execute CAP TTS command")?;

    if !status.success() {
        return Err(anyhow!(
            "CAP TTS command failed with status {:?}",
            status.code()
        ));
    }

    let metadata = fs::metadata(&tts_path).await?;
    if metadata.len() == 0 {
        let _ = fs::remove_file(&tts_path).await;
        return Ok(None);
    }

    info!(
        "CAP TTS audio synthesized. ({} bytes, alert ID {})",
        metadata.len(),
        alert.identifier
    );

    Ok(Some(tts_path))
}

fn cap_tts_synth_lock() -> &'static Mutex<()> {
    CAP_TTS_SYNTH_LOCK.get_or_init(|| Mutex::new(()))
}

fn child_text<'a, 'input>(node: Node<'a, 'input>, child_name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == child_name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn extract_same_value<'a, 'input>(node: Node<'a, 'input>, container_name: &str) -> Option<String> {
    for container in node
        .children()
        .filter(|child| child.is_element() && child.tag_name().name() == container_name)
    {
        if let Some(value) = extract_same_from_container(container) {
            return Some(value);
        }
    }
    None
}

fn extract_same_from_container<'a, 'input>(container: Node<'a, 'input>) -> Option<String> {
    let value_name = child_text(container, "valueName").unwrap_or_default();
    let value = child_text(container, "value").unwrap_or_default();
    if value_name.eq_ignore_ascii_case("SAME") && !value.is_empty() {
        Some(value)
    } else {
        None
    }
}

fn split_fips_codes(value: &str) -> Vec<String> {
    value
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

fn parse_cap_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|ts| ts.with_timezone(&Utc))
        .ok()
}

fn build_eas_text(alert: &CapAlert, timezone: &str) -> String {
    let mut header = build_cap_raw_header(
        &alert.originator_code,
        &alert.event_code,
        &alert.fips,
        alert.sent,
        alert.expires,
        &alert.source_url,
    );

    if !header.ends_with('-') {
        header.push('-');
    }

    let fallback_text = if alert.description.trim().is_empty() {
        alert.event_text.clone()
    } else {
        alert.description.clone()
    };

    let eas_text = crate::e2t_ng::E2T(&header, "", false, Some(timezone));
    if eas_text == "Invalid EAS header format" || eas_text.trim().is_empty() {
        warn!(
            "E2T-NG failed to generate EAS text for CAP header {}, using fallback text.",
            header
        );
        fallback_text
    } else {
        eas_text
    }
}

fn determine_purge_time(expires: Option<DateTime<Utc>>) -> Duration {
    let now = Utc::now();
    let default = Duration::from_secs(CAP_DEFAULT_PURGE_SECS);
    let Some(expires_at) = expires else {
        return default;
    };

    if expires_at <= now {
        return Duration::from_secs(60);
    }

    (expires_at - now).to_std().unwrap_or(default)
}

fn derive_event_code(event_text: &str) -> String {
    let alpha_only: String = event_text
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .take(3)
        .collect();
    if alpha_only.is_empty() {
        "CAP".to_string()
    } else {
        normalize_event_code(&alpha_only)
    }
}

fn normalize_event_code(event_code: &str) -> String {
    let mut normalized: String = event_code
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(3)
        .collect();
    normalized.make_ascii_uppercase();
    if normalized.is_empty() {
        "CAP".to_string()
    } else if normalized.len() < 3 {
        format!("{normalized:0<3}")
    } else {
        normalized
    }
}

fn build_cap_raw_header(
    originator_code: &str,
    event_code: &str,
    fips_list: &[String],
    sent: Option<DateTime<Utc>>,
    expires: Option<DateTime<Utc>>,
    source_hint: &str,
) -> String {
    let org = normalize_originator_code(originator_code);
    let code = normalize_event_code(event_code);
    let sent_utc = sent.unwrap_or_else(Utc::now);
    let issue_jjj_hhmm = sent_utc.format("%j%H%M").to_string();
    let exp = encode_expiration_from_cap(sent, expires);
    let source_marker = cap_header_source_marker(source_hint);

    let mut cleaned_fips: Vec<String> = fips_list
        .iter()
        .filter_map(|value| normalize_fips_code(value))
        .collect();
    cleaned_fips.sort();
    cleaned_fips.dedup();
    if cleaned_fips.is_empty() {
        cleaned_fips.push("099999".to_string());
    }

    format!(
        "ZCZC-{org}-{code}-{}+{exp}-{issue_jjj_hhmm}-{source_marker}-",
        cleaned_fips.join("-"),
    )
}

fn cap_header_source_marker(source_hint: &str) -> &'static str {
    if source_hint
        .get(..4)
        .map_or(false, |prefix| prefix.eq_ignore_ascii_case("WEA-"))
        || contains_ascii_ignore_case(source_hint, b"publicwea")
        || contains_ascii_ignore_case(source_hint, b"/wea/")
        || contains_ascii_ignore_case(source_hint, b"/wea#")
        || source_hint
            .rsplit_once('#')
            .map_or(false, |(_, fragment)| {
                fragment
                    .get(..4)
                    .map_or(false, |prefix| prefix.eq_ignore_ascii_case("WEA-"))
            })
    {
        CAP_HEADER_SOURCE_MARKER_WEA
    } else {
        CAP_HEADER_SOURCE_MARKER_CAP
    }
}

fn contains_ascii_ignore_case(haystack: &str, needle: &[u8]) -> bool {
    let haystack = haystack.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn is_cap_relevant(alert_fips: &[String], watched_fips: &HashSet<String>) -> bool {
    if watched_fips.is_empty() {
        return true;
    }
    if watched_fips.contains("000000") || watched_fips.contains("") {
        return true;
    }
    if alert_fips.iter().any(|fips| fips == "000000") {
        return true;
    }
    alert_fips.iter().any(|fips| watched_fips.contains(fips))
}

async fn append_cap_log(config: &Config, alert: &CapAlert) -> Result<()> {
    let header_string = build_cap_raw_header(
        &alert.originator_code,
        &alert.event_code,
        &alert.fips,
        alert.sent,
        alert.expires,
        &alert.source_url,
    );

    let timezone = config.timezone.to_string();
    let alert_desc = build_eas_text(alert, timezone.as_str());

    let received_at = Utc::now();
    let local_time = received_at.with_timezone(&config.timezone);
    let timestamp = local_time.format("%Y-%m-%d %l:%M:%S %p");

    let log_line = format!(
        "{}: {} (Received @ {})\n\n",
        header_string, alert_desc, timestamp
    );

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.dedicated_alert_log_file)
        .await?;
    file.write_all(log_line.as_bytes()).await?;
    Ok(())
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    match fetch_text_once(client, url, false).await {
        Ok(text) => Ok(text),
        Err(err) if is_incomplete_http_message(&err) => {
            debug!(
                "Retrying CAP fetch with `Connection: close` after incomplete HTTP response from {}",
                url
            );
            fetch_text_once(client, url, true)
                .await
                .with_context(|| format!("Retry failed for CAP URL {}", url))
        }
        Err(err) => Err(err),
    }
}

async fn fetch_text_once(client: &reqwest::Client, url: &str, force_close: bool) -> Result<String> {
    debug!(
        "Starting CAP HTTP GET {} (force_close={})",
        url, force_close
    );
    let mut request = client.get(url);
    if force_close {
        request = request.header(reqwest::header::CONNECTION, "close");
    }
    let response = request.send().await?;
    let status = response.status();
    let content_length = response.content_length();
    debug!(
        "CAP HTTP response received from {}: status={}, content_length={:?}",
        url, status, content_length
    );
    let response = response.error_for_status()?;
    let text = response.text().await?;
    debug!(
        "CAP HTTP body read complete for {} ({} bytes)",
        url,
        text.len()
    );
    Ok(text)
}

fn is_incomplete_http_message(err: &anyhow::Error) -> bool {
    if let Some(req_err) = err.downcast_ref::<reqwest::Error>() {
        if req_err.to_string().contains("IncompleteMessage") {
            return true;
        }
    }
    let mut source = err.source();
    while let Some(inner) = source {
        if inner.to_string().contains("IncompleteMessage") {
            return true;
        }
        source = inner.source();
    }
    false
}

fn is_http_status(err: &anyhow::Error, status: reqwest::StatusCode) -> bool {
    err.downcast_ref::<reqwest::Error>()
        .and_then(|req_err| req_err.status())
        .map(|code| code == status)
        .unwrap_or(false)
}

fn xml_snippet(xml: &str, max_chars: usize) -> &str {
    &xml[..min(xml.len(), max_chars)]
}

fn looks_like_alert_xml(xml: &str) -> bool {
    if let Ok(document) = Document::parse(xml) {
        return document.root_element().tag_name().name() == "alert";
    }
    xml.contains("<alert")
}

fn build_dedupe_key_components(
    originator: &str,
    event_code: &str,
    issuance: &str,
    fips: &[String],
) -> Option<String> {
    let originator = normalize_originator_code(originator);
    let event_code = normalize_event_code(event_code);
    let issuance: String = issuance
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .take(7)
        .collect();
    if issuance.len() != 7 {
        return None;
    }

    let mut cleaned_fips: Vec<String> = fips
        .iter()
        .filter_map(|value| normalize_fips_code(value))
        .collect();
    cleaned_fips.sort();
    cleaned_fips.dedup();
    if cleaned_fips.is_empty() {
        cleaned_fips.push("099999".to_string());
    }

    Some(format!(
        "org:{originator}|evt:{event_code}|iss:{issuance}|fips:{}",
        cleaned_fips.join(",")
    ))
}

fn build_dedupe_key(alert: &CapAlert) -> String {
    let issuance = alert
        .sent
        .map(|sent| sent.format("%j%H%M").to_string())
        .unwrap_or_else(|| "0000000".to_string());
    build_dedupe_key_components(
        &alert.originator_code,
        &alert.event_code,
        &issuance,
        &alert.fips,
    )
    .unwrap_or_else(|| format!("id:{}", alert.identifier))
}

fn build_dedupe_key_from_raw_header(raw_header: &str) -> Option<String> {
    let trimmed = raw_header.trim().trim_end_matches('-');
    let (prefix, _) = trimmed.rsplit_once('-')?;
    let body = prefix.strip_prefix("ZCZC-")?;
    let mut parts = body.splitn(3, '-');
    let originator = parts.next()?;
    let event_code = parts.next()?;
    let fips_duration_and_issuance = parts.next()?;
    let (fips_and_duration, issuance) = fips_duration_and_issuance.rsplit_once('-')?;
    let (fips_segment, _) = fips_and_duration.rsplit_once('+')?;
    let fips: Vec<String> = fips_segment
        .split('-')
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.to_string())
        .collect();
    build_dedupe_key_components(originator, event_code, issuance, &fips)
}

fn extract_parameter_value<'a, 'input>(
    info_node: Node<'a, 'input>,
    parameter_name: &str,
) -> Option<String> {
    for parameter in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "parameter")
    {
        let value_name = child_text(parameter, "valueName").unwrap_or_default();
        if !value_name.eq_ignore_ascii_case(parameter_name) {
            continue;
        }
        if let Some(value) = child_text(parameter, "value") {
            return Some(value);
        }
    }
    None
}

fn normalize_originator_code(value: &str) -> String {
    let mut cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(3)
        .collect();
    cleaned.make_ascii_uppercase();
    if cleaned.is_empty() {
        "CIV".to_string()
    } else if cleaned.len() < 3 {
        format!("{cleaned:X<3}")
    } else {
        cleaned
    }
}

fn normalize_fips_code(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if digits.len() >= 6 {
        Some(digits.chars().take(6).collect())
    } else {
        Some(format!("{digits:0>6}"))
    }
}

fn encode_expiration_from_cap(
    sent: Option<DateTime<Utc>>,
    expires: Option<DateTime<Utc>>,
) -> String {
    let default = "0030".to_string();
    let Some(expires_at) = expires else {
        return default;
    };

    let reference = sent.unwrap_or_else(Utc::now);
    if expires_at <= reference {
        return default;
    }

    let duration = expires_at - reference;
    let total_minutes = ((duration.num_seconds() + 59) / 60).max(1);
    let hours = (total_minutes / 60).min(99);
    let mins = total_minutes % 60;
    format!("{hours:02}{mins:02}")
}

fn is_audio_resource(mime: Option<&str>, uri: Option<&str>, deref_uri: Option<&str>) -> bool {
    if let Some(mime_value) = mime {
        let lower = mime_value.to_ascii_lowercase();
        if lower.starts_with("audio/") || lower.contains("audio") {
            return true;
        }
    }

    if let Some(uri_value) = uri {
        let lower = uri_value.to_ascii_lowercase();
        if [".mp3", ".wav", ".ogg", ".m4a", ".aac", ".flac"]
            .iter()
            .any(|ext| lower.contains(ext))
        {
            return true;
        }
    }

    deref_uri.is_some()
}

async fn fetch_cap_audio_recording(
    client: &reqwest::Client,
    config: &Config,
    alert: &CapAlert,
    raw_header: &str,
    event_code: &str,
) -> Result<Option<PathBuf>> {
    fs::create_dir_all(&config.recording_dir).await?;

    let cap_audio_path = if alert.audio_uri.is_none() && alert.audio_deref_uri.is_none() {
        match synthesize_cap_tts_audio(config, alert, event_code).await {
            Ok(Some(path)) => path,
            Ok(None) => return Ok(None),
            Err(err) => {
                warn!(
                    "Failed to synthesize CAP TTS fallback for alert {}: {}",
                    alert.identifier, err
                );
                return Ok(None);
            }
        }
    } else {
        let ext = audio_extension(alert.audio_mime_type.as_deref(), alert.audio_uri.as_deref());
        let download_name = format!(
            "cap_src_{}_{}.{}",
            sanitize_filename_label(&alert.identifier),
            sanitize_filename_label(event_code),
            ext
        );
        let download_path = config.recording_dir.join(download_name);

        let audio_bytes = if let Some(deref_uri) = &alert.audio_deref_uri {
            decode_deref_uri_audio(deref_uri)?
        } else if let Some(uri) = &alert.audio_uri {
            fetch_audio_bytes(client, uri).await?
        } else {
            return Ok(None);
        };

        if audio_bytes.is_empty() {
            return Ok(None);
        }
        if audio_bytes.len() > CAP_AUDIO_MAX_BYTES {
            return Err(anyhow!(
                "CAP audio payload is too large ({} bytes > {} bytes)",
                audio_bytes.len(),
                CAP_AUDIO_MAX_BYTES
            ));
        }

        fs::write(&download_path, audio_bytes).await?;
        download_path
    };

    let (output_path, should_remove_cap_audio_input) =
        match build_recording_with_same_header(config, raw_header, event_code, &cap_audio_path)
            .await
        {
            Ok(path) => (Some(path), true),
            Err(err) => {
                warn!(
                    "Failed to prepend SAME header to CAP audio, using raw CAP audio file: {}",
                    err
                );
                (Some(cap_audio_path.clone()), false)
            }
        };

    if should_remove_cap_audio_input {
        let _ = fs::remove_file(&cap_audio_path).await;
    }

    Ok(output_path)
}

async fn fetch_audio_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?;
    let response = response.error_for_status()?;
    let bytes = response.bytes().await?;
    Ok(bytes.to_vec())
}

fn decode_deref_uri_audio(deref_uri: &str) -> Result<Vec<u8>> {
    if let Some((meta, encoded)) = deref_uri.split_once(',') {
        if meta.to_ascii_lowercase().contains(";base64") {
            return base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .context("Invalid CAP derefUri base64 payload");
        }
    }

    base64::engine::general_purpose::STANDARD
        .decode(deref_uri.trim())
        .context("Invalid CAP derefUri payload")
}

async fn build_recording_with_same_header(
    config: &Config,
    raw_header: &str,
    event_code: &str,
    cap_audio_input_path: &PathBuf,
) -> Result<PathBuf> {
    let tmp_id = format!(
        "{}_{}",
        sanitize_filename_label(event_code),
        sanitize_filename_label(&Utc::now().timestamp_millis().to_string())
    );

    let header_path = config
        .recording_dir
        .join(format!("cap_header_{}.wav", tmp_id));
    let silence_path = config
        .recording_dir
        .join(format!("cap_silence_{}.wav", tmp_id));
    let attn_tone_path = config
        .recording_dir
        .join(format!("cap_attn_{}.wav", tmp_id));
    let nnnn_path = config
        .recording_dir
        .join(format!("cap_nnnn_{}.wav", tmp_id));

    let header_samples = header::generate_same_header_samples(
        raw_header,
        CAP_RECORDING_SAMPLE_RATE,
        CAP_HEADER_AMPLITUDE,
    )?;
    let silence_samples = header::generate_silence_for_duration(CAP_RECORDING_SAMPLE_RATE, 1.0);
    let attn_samples =
        header::generate_attention_tone(CAP_RECORDING_SAMPLE_RATE, CAP_HEADER_AMPLITUDE)?;
    let nnnn_samples = header::generate_same_header_samples(
        "NNNN",
        CAP_RECORDING_SAMPLE_RATE,
        CAP_HEADER_AMPLITUDE,
    )?;

    write_wav_i16(&header_path, CAP_RECORDING_SAMPLE_RATE, &header_samples).await?;
    write_wav_i16(&attn_tone_path, CAP_RECORDING_SAMPLE_RATE, &attn_samples).await?;
    write_wav_i16(&silence_path, CAP_RECORDING_SAMPLE_RATE, &silence_samples).await?;
    write_wav_i16(&nnnn_path, CAP_RECORDING_SAMPLE_RATE, &nnnn_samples).await?;

    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let output_name = format!(
        "EAS_Recording_{}_{}_{}.wav",
        timestamp,
        sanitize_filename_label(event_code),
        "IPAWSCAP"
    );
    let output_path = config.recording_dir.join(output_name);

    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(&header_path)
        .arg("-i")
        .arg(&attn_tone_path)
        .arg("-i")
        .arg(&silence_path)
        .arg("-i")
        .arg(cap_audio_input_path)
        .arg("-i")
        .arg(&silence_path)
        .arg("-i")
        .arg(&nnnn_path)
        .arg("-filter_complex")
        .arg("[0:a][1:a][2:a][3:a][4:a][5:a]concat=n=6:v=0:a=1[outa]")
        .arg("-map")
        .arg("[outa]")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(&output_path);

    let status = ffmpeg.status().await?;
    let _ = fs::remove_file(&header_path).await;
    let _ = fs::remove_file(&nnnn_path).await;
    let _ = fs::remove_file(&silence_path).await;
    let _ = fs::remove_file(&attn_tone_path).await;

    if !status.success() {
        return Err(anyhow!(
            "ffmpeg failed to build CAP recording with SAME header (status {:?})",
            status.code()
        ));
    }

    Ok(output_path)
}

async fn write_wav_i16(path: &PathBuf, sample_rate: u32, samples: &[i16]) -> Result<()> {
    let path = path.clone();
    let samples = samples.to_vec();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec)?;
        for sample in samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
        Ok(())
    })
    .await??;
    Ok(())
}

fn audio_extension(mime_type: Option<&str>, uri: Option<&str>) -> &'static str {
    if let Some(mime) = mime_type {
        let lower = mime.to_ascii_lowercase();
        if lower.contains("mp3") || lower.contains("mpeg") {
            return "mp3";
        }
        if lower.contains("wav") || lower.contains("wave") {
            return "wav";
        }
        if lower.contains("ogg") {
            return "ogg";
        }
        if lower.contains("aac") {
            return "aac";
        }
        if lower.contains("flac") {
            return "flac";
        }
        if lower.contains("mp4") || lower.contains("m4a") {
            return "m4a";
        }
    }

    if let Some(value) = uri {
        let lower = value.to_ascii_lowercase();
        for ext in ["mp3", "wav", "ogg", "aac", "flac", "m4a"] {
            if lower.contains(&format!(".{ext}")) {
                return ext;
            }
        }
    }

    "bin"
}

fn sanitize_filename_label(label: &str) -> String {
    let mut output = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            output.push(c.to_ascii_uppercase());
        } else if matches!(c, '-' | '_') {
            output.push(c);
        } else {
            output.push('_');
        }
    }

    let trimmed = output.trim_matches('_');
    if trimmed.is_empty() {
        "UNKNOWN".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone};
    use std::time::Duration;

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
    fn parse_feed_alert_links_collects_and_deduplicates() {
        let xml = include_str!("../tests/fixtures/cap_feed.xml");
        let links = parse_feed_alert_links(xml).expect("links");
        assert_eq!(
            links,
            vec![
                "https://alerts.example/a1",
                "https://alerts.example/a2",
                "https://alerts.example/id-only-1"
            ]
        );
    }

    #[test]
    fn parse_inline_alert_documents_reads_embedded_alerts() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ns1:alerts xmlns:ns1="http://gov.fema.ipaws.services/feed">
  <alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
    <identifier>WEA-1</identifier>
  </alert>
  <alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
    <identifier>WEA-2</identifier>
  </alert>
</ns1:alerts>"#;

        let alerts = parse_inline_alert_documents(xml, "https://example.test/wea")
            .expect("embedded parse")
            .expect("embedded alerts");
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].0, "https://example.test/wea#WEA-1");
        assert!(alerts[0].1.contains("<identifier>WEA-1</identifier>"));
        assert_eq!(alerts[1].0, "https://example.test/wea#WEA-2");
    }

    #[test]
    fn parse_cap_alert_valid_fixture_parses_core_fields() {
        let xml = include_str!("../tests/fixtures/cap_alert_valid.xml");
        let alert = parse_cap_alert(xml, "https://alerts.example/valid").expect("alert");
        assert_eq!(alert.identifier, "TEST-VALID-001");
        assert_eq!(alert.event_text, "Special Weather Statement");
        assert_eq!(alert.event_code, "SPE");
        assert_eq!(alert.originator_code, "CIV");
        assert_eq!(alert.fips, vec!["031055"]);
        assert!(alert.audio_uri.is_none());
    }

    #[test]
    fn parse_cap_alert_same_audio_fixture_uses_same_fields() {
        let xml = include_str!("../tests/fixtures/cap_alert_same_audio.xml");
        let alert = parse_cap_alert(xml, "https://alerts.example/same").expect("alert");
        assert_eq!(alert.identifier, "TEST-SAME-002");
        assert_eq!(alert.event_code, "TOR");
        assert_eq!(alert.originator_code, "WXR");
        assert_eq!(alert.fips, vec!["031055", "031153"]);
        assert_eq!(
            alert.audio_uri.as_deref(),
            Some("https://alerts.example/audio/test-alert.mp3")
        );
        assert_eq!(alert.audio_mime_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(alert.instructions.as_deref(), Some("Take shelter now."));
    }

    #[test]
    fn parse_cap_alert_prefers_cmam_long_text_for_description() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-WEA-001</identifier>
  <sender>sender.test</sender>
  <sent>2026-03-07T18:23:55-05:00</sent>
  <msgType>Alert</msgType>
  <scope>Public</scope>
  <info>
    <event>Local Area Emergency</event>
    <parameter>
      <valueName>CMAMlongtext</valueName>
      <value>Lee County: Lightning Alert: Seek shelter NOW</value>
    </parameter>
  </info>
</alert>"#;
        let alert = parse_cap_alert(xml, "https://alerts.example/wea").expect("alert");
        assert_eq!(
            alert.description_raw,
            "Lee County: Lightning Alert: Seek shelter NOW"
        );
        assert_eq!(
            alert.simple_description,
            "Lee County: Lightning Alert: Seek shelter NOW"
        );
    }

    #[test]
    fn parse_cap_alert_rejects_cancel_and_missing_info() {
        let cancel_xml = include_str!("../tests/fixtures/cap_alert_cancel.xml");
        assert!(parse_cap_alert(cancel_xml, "https://alerts.example/cancel").is_err());

        let missing_info_xml = include_str!("../tests/fixtures/cap_alert_missing_info.xml");
        assert!(parse_cap_alert(missing_info_xml, "https://alerts.example/missing").is_err());
    }

    #[test]
    fn normalize_helpers_work_for_event_originator_and_fips() {
        assert_eq!(normalize_event_code("to"), "TO0");
        assert_eq!(normalize_event_code("   !!!"), "CAP");
        assert_eq!(normalize_originator_code("wx"), "WXX");
        assert_eq!(normalize_originator_code(""), "CIV");
        assert_eq!(normalize_fips_code("31055"), Some("031055".to_string()));
        assert_eq!(normalize_fips_code("0310559"), Some("031055".to_string()));
        assert_eq!(normalize_fips_code("abc"), None);
    }

    #[test]
    fn parsed_identifier_from_url_prefers_fragment() {
        assert_eq!(
            parsed_identifier_from_url("https://example.test/PublicWEA/recent#2393260467162972"),
            "2393260467162972"
        );
        assert_eq!(
            parsed_identifier_from_url("https://alerts.example/id-only-1.xml"),
            "id-only-1"
        );
    }

    #[test]
    fn encode_expiration_and_header_building_are_stable() {
        let sent = Utc
            .with_ymd_and_hms(2026, 3, 6, 15, 0, 0)
            .single()
            .expect("sent");
        let expires = Utc
            .with_ymd_and_hms(2026, 3, 6, 16, 35, 0)
            .single()
            .expect("expires");
        assert_eq!(
            encode_expiration_from_cap(Some(sent), Some(expires)),
            "0135"
        );
        assert_eq!(encode_expiration_from_cap(Some(sent), Some(sent)), "0030");

        let header = build_cap_raw_header("wx", "to", &[], Some(sent), Some(expires), "id");
        assert!(header.starts_with("ZCZC-WXX-TO0-099999+0135-"));
        assert!(header.ends_with("-IPAWSCAP-"));
    }

    #[test]
    fn build_dedupe_key_ignores_fips_order_and_duration() {
        let xml = include_str!("../tests/fixtures/cap_alert_same_audio.xml");
        let first = parse_cap_alert(xml, "https://alerts.example/same").expect("first");
        let mut second = first.clone();
        second.identifier = "SECOND-ID".to_string();
        second.fips.reverse();
        second.expires = second.expires.map(|value| value + ChronoDuration::minutes(30));

        assert_eq!(build_dedupe_key(&first), build_dedupe_key(&second));
    }

    #[test]
    fn build_dedupe_key_from_raw_header_ignores_fips_order_and_duration() {
        let first = "ZCZC-EAS-RMT-031000-031055+0100-0011530-KETV    -";
        let second = "ZCZC-EAS-RMT-031055-031000+0030-0011530-KISO    -";
        let first_key = build_dedupe_key_from_raw_header(first).expect("first key");
        let second_key = build_dedupe_key_from_raw_header(second).expect("second key");
        assert_eq!(first_key, second_key);
    }

    #[test]
    fn active_alert_has_dedupe_key_matches_non_cap_sources_too() {
        let dedupe_key = build_dedupe_key_from_raw_header("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-")
            .expect("dedupe key");
        let eas_alert = ActiveAlert::new(
            sample_alert_data("TOR", &["031055"]),
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-".to_string(),
            Duration::from_secs(120),
        );

        assert!(active_alert_has_dedupe_key(&[eas_alert], dedupe_key.as_str()));
    }

    #[test]
    fn deref_uri_decode_supports_data_uri_and_raw_base64() {
        let data_uri = "data:audio/wav;base64,SGVsbG8=";
        let raw = "SGVsbG8=";
        assert_eq!(decode_deref_uri_audio(data_uri).expect("decode"), b"Hello");
        assert_eq!(decode_deref_uri_audio(raw).expect("decode"), b"Hello");
        assert!(decode_deref_uri_audio("not-base64").is_err());
    }

    #[test]
    fn audio_resource_and_extension_detection_work() {
        assert!(is_audio_resource(Some("audio/mpeg"), None, None));
        assert!(is_audio_resource(None, Some("https://x/y/test.wav"), None));
        assert!(is_audio_resource(None, None, Some("SGVsbG8=")));
        assert!(!is_audio_resource(
            Some("text/plain"),
            Some("https://x/y/test.txt"),
            None
        ));

        assert_eq!(audio_extension(Some("audio/mpeg"), None), "mp3");
        assert_eq!(audio_extension(None, Some("https://x/y/test.ogg")), "ogg");
        assert_eq!(
            audio_extension(Some("application/octet-stream"), None),
            "bin"
        );
    }

    #[test]
    fn sanitize_filename_and_time_helpers_work() {
        assert_eq!(
            sanitize_filename_label("tor/warning 2026"),
            "TOR_WARNING_2026"
        );
        assert_eq!(sanitize_filename_label("***"), "UNKNOWN");

        assert_eq!(parse_compact_time("9"), Some((9, 0)));
        assert_eq!(parse_compact_time("1234"), Some((12, 34)));
        assert_eq!(parse_compact_time("1360"), None);

        let expanded = expand_cap_times_for_tts("Until 930 AM CDT in effect.");
        assert!(expanded.contains("9:30 AM Central Daylight Time"));
    }

    #[test]
    fn cap_relevance_respects_watched_fips_and_wildcards() {
        let alert_fips = vec!["031055".to_string(), "031153".to_string()];
        let empty = HashSet::new();
        assert!(is_cap_relevant(&alert_fips, &empty));

        let mut watched = HashSet::new();
        watched.insert("031055".to_string());
        assert!(is_cap_relevant(&alert_fips, &watched));

        watched.clear();
        watched.insert("000000".to_string());
        assert!(is_cap_relevant(&alert_fips, &watched));

        watched.clear();
        watched.insert("999999".to_string());
        assert!(!is_cap_relevant(&alert_fips, &watched));
    }
}
