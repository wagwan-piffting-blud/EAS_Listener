use crate::alerts::update_alert_files;
use crate::config::Config;
use crate::db::DbHandle;
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
use std::collections::{HashMap, HashSet, VecDeque};
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
pub(crate) const CAP_HTTP_TIMEOUT_SECS: u64 = 10;
const CAP_DEFAULT_PURGE_SECS: u64 = 30 * 60;
pub(crate) const CAP_SEEN_DEFAULT_TTL_SECS: i64 = 6 * 60 * 60;
const CAP_FORBIDDEN_SKIP_TTL_SECS: i64 = 24 * 60 * 60;
const CAP_PARSE_ERROR_SKIP_TTL_SECS: i64 = 24 * 60 * 60;
const CAP_AUDIO_MAX_BYTES: usize = 25 * 1024 * 1024;
const CAP_RECORDING_SAMPLE_RATE: u32 = 48_000;
const CAP_HEADER_AMPLITUDE: f64 = 0.42;
const CAP_TTS_DEFAULT_CEPSTRAL_VOICE: &str = "Allison";
const CAP_TTS_CEPSTRAL_VOICES: &str = "Allison, David, Jean-Pierre, William";
const CAP_TTS_SUPPORTED_ENGINES: &str = "piper, espeak-ng, speechify, cepstral, loquendo";
// A canonical PCM WAV header is 44 bytes, so anything at or under that carries no samples.
const CAP_TTS_EMPTY_WAV_BYTES: u64 = 44;
// Speechify runs the voice inside an emulated win32 address space with a fixed 128 MB guest
// heap, and a long enough passage exhausts it. The process still exits 0 and leaves a
// header-only WAV, so there is nothing to detect until the file is inspected. How much text fits
// depends heavily on content -- plain prose has cleared 5,600 characters while a real alert has
// failed at 2,794 -- so no fixed size is safe. This is only the starting split; a passage that
// comes back empty is halved and retried until it fits.
const CAP_TTS_MAX_CHUNK_CHARS: usize = 1_500;
// Below this there is no point halving again: the failure is not about length.
const CAP_TTS_MIN_CHUNK_CHARS: usize = 200;
// Safety valve so a passage that always fails cannot spin forever. Sized to leave room for a
// genuinely long alert split into floor-sized pieces, not just for the failure case.
const CAP_TTS_MAX_SYNTH_ATTEMPTS: usize = 256;
const _: () = assert!(CAP_TTS_MIN_CHUNK_CHARS < CAP_TTS_MAX_CHUNK_CHARS);
// A real alert came back empty at 2,794 characters, so the first attempt starts below that.
const _: () = assert!(CAP_TTS_MAX_CHUNK_CHARS < 2_794);
const CAP_ACTIVE_ALERTS_FILE: &str = "active_alerts.json";
pub(crate) const DEFAULT_NO_DESCRIPTION: &str = "No CAP description provided.";
pub(crate) const CAP_HEADER_SOURCE_MARKER_CAP: &str = "IPAWSCAP";
pub(crate) const CAP_HEADER_SOURCE_MARKER_WEA: &str = "IPAWSWEA";
pub(crate) const CAP_HEADER_SOURCE_MARKER_NAAD: &str = "NAADSCAP";
/// The Canadian Alerting Attention Signal Alert Ready broadcasts open with. Compiled in, so a
/// standalone binary and the container image both have it without shipping `include/`.
const ALERT_READY_TONE_WAV: &[u8] = include_bytes!("../include/pelmorex.wav");
const URL_SPELL_MODE_ON: &str = "\\!rp70 \\!tsc";
const URL_SPELL_MODE_OFF: &str = "\\!rpr \\!ts0";
const URL_BARE_HOST_TLDS: &[&str] = &[
    "com", "org", "net", "gov", "edu", "mil", "info", "biz", "ca",
];
const URL_TWO_LABEL_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "gov.uk", "ac.uk", "me.uk", "net.uk", "com.au", "net.au", "org.au",
    "gov.au", "edu.au", "co.nz", "govt.nz", "org.nz", "co.jp", "or.jp", "ne.jp", "co.za", "org.za",
    "com.br", "com.mx", "gob.mx", "co.in", "gov.in",
];

static CAP_TTS_SYNTH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CapAlert {
    pub(crate) identifier: String,
    pub(crate) originator_code: String,
    pub(crate) sender: String,
    pub(crate) sender_name: Option<String>,
    pub(crate) sent: Option<DateTime<Utc>>,
    pub(crate) expires: Option<DateTime<Utc>>,
    pub(crate) msg_type: String,
    pub(crate) scope: String,
    pub(crate) event_text: String,
    pub(crate) event_code: String,
    pub(crate) urgency: Option<String>,
    pub(crate) severity: Option<String>,
    pub(crate) certainty: Option<String>,
    pub(crate) description: String,
    pub(crate) description_raw: String,
    pub(crate) instructions: Option<String>,
    pub(crate) simple_description: String,
    pub(crate) areas: Vec<String>,
    pub(crate) fips: Vec<String>,
    pub(crate) audio_uri: Option<String>,
    pub(crate) audio_deref_uri: Option<String>,
    pub(crate) audio_mime_type: Option<String>,
    pub(crate) source_url: String,
    /// CAP-CP (Canadian) alerts humanize against `include/same-ca.json` instead of `same-us.json`.
    pub(crate) canadian: bool,
}

fn spawn_cap_processor_task(
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    db: DbHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_cap_processor(config, app_state, monitoring, db).await {
            warn!("CAP processor task exited with error: {}", err);
        }
    })
}

/// Describes both CAP feeds, not just IPAWS: this supervisor always runs and sees every reload,
/// so it owns the status panel's configuration for CAP-CP too.
async fn sync_cap_runtime_config_status(app_state: &Arc<Mutex<AppState>>, config: &Config) {
    let mut guard = app_state.lock().await;
    guard.cap_status.enabled = config.process_cap_alerts || config.process_capcp_alerts;
    guard.cap_status.ipaws_enabled = config.process_cap_alerts;
    guard.cap_status.endpoint_count = config.cap_endpoints.len();
    guard.cap_status.endpoints = config.cap_endpoints.clone();
    guard.cap_status.capcp_enabled = config.process_capcp_alerts;
    guard.cap_status.capcp_endpoints = config.capcp_stream_endpoints.clone();
}

pub async fn run_cap_supervisor(
    initial_config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    mut reload_rx: broadcast::Receiver<Config>,
    db: DbHandle,
) -> Result<()> {
    let mut current_config = initial_config;
    sync_cap_runtime_config_status(&app_state, &current_config).await;
    let mut cap_task: Option<JoinHandle<()>> = if current_config.process_cap_alerts {
        Some(spawn_cap_processor_task(
            current_config.clone(),
            app_state.clone(),
            monitoring.clone(),
            db.clone(),
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
                        db.clone(),
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
    db: DbHandle,
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
                    backfill_persisted_cap_details(
                        &config,
                        &app_state,
                        &monitoring,
                        &dedupe_key,
                        &parsed,
                    )
                    .await;

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
                    &db,
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

pub(crate) async fn load_persisted_active_dedupe_keys(shared_state_dir: &Path) -> HashSet<String> {
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

pub(crate) async fn cap_alert_is_active(
    app_state: &Arc<Mutex<AppState>>,
    dedupe_key: &str,
) -> bool {
    let guard = app_state.lock().await;
    active_alert_has_dedupe_key(&guard.active_alerts, dedupe_key)
}

fn backfill_alert_cap_details(
    alerts: &mut [ActiveAlert],
    dedupe_key: &str,
    description: Option<&str>,
    instructions: Option<&str>,
) -> bool {
    let now = Utc::now();
    let mut changed = false;

    for alert in alerts.iter_mut() {
        if alert.expires_at <= now
            || build_dedupe_key_from_raw_header(&alert.raw_header).as_deref() != Some(dedupe_key)
        {
            continue;
        }

        if alert.data.description.is_none() && description.is_some() {
            alert.data.description = description.map(str::to_string);
            changed = true;
        }

        if alert.data.instructions.is_none() && instructions.is_some() {
            alert.data.instructions = instructions.map(str::to_string);
            changed = true;
        }
    }

    changed
}

pub(crate) async fn backfill_persisted_cap_details(
    config: &Config,
    app_state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
    dedupe_key: &str,
    parsed: &CapAlert,
) {
    let description = Some(parsed.simple_description.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let instructions = parsed
        .instructions
        .as_deref()
        .map(simple_sanitize_description)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    if description.is_none() && instructions.is_none() {
        return;
    }

    let active_snapshot = {
        let mut guard = app_state.lock().await;
        if !backfill_alert_cap_details(
            &mut guard.active_alerts,
            dedupe_key,
            description.as_deref(),
            instructions.as_deref(),
        ) {
            return;
        }

        if let Err(err) = update_alert_files(&config.shared_state_dir, &guard).await {
            warn!(
                "Failed to persist backfilled CAP details for dedupe key {}: {}",
                dedupe_key, err
            );
        }

        guard.active_alerts.clone()
    };

    info!(
        "Backfilled CAP description/instructions onto active alert {} (dedupe key={})",
        parsed.identifier, dedupe_key
    );
    monitoring.broadcast_alerts(active_snapshot, None, None);
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

/// Writes an alert to the dedicated CAP log and the alert database.
///
/// Split out of `process_cap_alert` so a caller that has decided an alert should be archived but
/// not made active -- a CAP-CP alert recovered from the NAAD archive after it expired, say -- can
/// record it without also relaying it.
pub(crate) async fn archive_cap_alert(
    config: &Config,
    db: &DbHandle,
    alert: &CapAlert,
    event_code: &str,
    source_stream: &str,
) {
    if let Err(err) = append_cap_log(config, alert).await {
        warn!("Failed to append CAP log entry: {}", err);
    }

    let received_at_iso = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let expires_at_iso = alert
        .expires
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let cap_duration = encode_expiration_from_cap(alert.sent, alert.expires);
    let cap_raw_header = build_cap_raw_header(
        &alert.originator_code,
        event_code,
        &alert.fips,
        alert.sent,
        alert.expires,
        &alert.source_url,
    );
    let cap_locations = if alert.areas.is_empty() {
        "Unknown".to_string()
    } else {
        alert.areas.join(", ")
    };
    let cap_originator_name = alert
        .sender_name
        .as_deref()
        .unwrap_or(alert.sender.as_str());
    let cap_eas_text = build_eas_text(
        alert,
        config.timezone.to_string().as_str(),
        &config.endec_mode,
    );

    match db
        .insert_cap_alert(
            &cap_raw_header,
            &cap_eas_text,
            event_code,
            &alert.event_text,
            &alert.originator_code,
            cap_originator_name,
            &alert.fips,
            &cap_locations,
            Some(alert.simple_description.as_str()),
            source_stream,
            alert.urgency.as_deref(),
            alert.severity.as_deref(),
            alert.certainty.as_deref(),
            alert.instructions.as_deref(),
            &alert.identifier,
            &alert.sender,
            Some(cap_duration.as_str()),
            &received_at_iso,
            expires_at_iso.as_deref(),
        )
        .await
    {
        Ok(id) => info!("CAP alert saved to database (id={})", id),
        Err(err) => warn!("Failed to save CAP alert to database: {}", err),
    }
}

pub(crate) async fn process_cap_alert(
    config: &Config,
    app_state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
    client: &reqwest::Client,
    source_stream: &str,
    alert: CapAlert,
    db: &DbHandle,
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
        archive_cap_alert(config, db, &alert, &event_code, source_stream).await;
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
    let eas_text = build_eas_text(&alert, timezone.as_str(), &config.endec_mode);
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
        instructions: alert
            .instructions
            .as_deref()
            .map(simple_sanitize_description)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
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

    // No source stream: a CAP feed is not an audio stream. Registering one made the dashboard
    // filter it back out by URL, which only ever knew the IPAWS endpoints -- and a CAP-CP source
    // carries the alert identifier, so each alert would have added a new phantom stream.
    monitoring.broadcast_alerts(active_snapshot, None, None);

    let framing = recording_framing(config, &alert);
    let cap_recording_path =
        match fetch_cap_audio_recording(client, config, &alert, &raw_header, &event_code, framing)
            .await
        {
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
    if let Some(ref name) = recording_file_name {
        db.update_recording_name(&raw_header, name).await;
    }
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
        // The raw header stays the alert's identity everywhere internally; it is only kept out of
        // what gets published when the alert went out as Alert Ready rather than as SAME.
        let published_header = (framing == RecordingFraming::Same).then_some(raw_header.as_str());
        send_alert_webhook(
            source_stream,
            &alert_for_webhook,
            &eas_text,
            published_header,
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

pub(crate) fn simple_sanitize_description(description: &str) -> String {
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

    let is_nws_wea = originator_code == "WXR"
        && cap_header_source_marker(source_url) == CAP_HEADER_SOURCE_MARKER_WEA;

    let normalized_description = if is_nws_wea {
        description_text
            .as_deref()
            .map(|raw| {
                crate::nws_bulletin::normalize_nws_bulletin(
                    raw,
                    &crate::nws_bulletin::NormalizeOptions::default(),
                )
            })
            .filter(|text| !text.is_empty())
    } else {
        None
    };

    let description_raw = normalized_description
        .or(cmam_long_text)
        .or(description_text)
        .unwrap_or_else(|| DEFAULT_NO_DESCRIPTION.to_string());
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
        canadian: false,
    })
}

fn normalize_urls_in_description(text: &str, use_spell_tags: bool) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut cursor = 0usize;

    for caps in url_regex().captures_iter(text) {
        let whole = caps.get(0).expect("capture group 0 always exists");

        if matches!(
            text[..whole.start()].chars().next_back(),
            Some('@') | Some('/') | Some('\\')
        ) {
            continue;
        }

        let has_scheme = caps.name("scheme").is_some();
        let host = caps.name("host").map(|m| m.as_str()).unwrap_or_default();
        let rest = caps.name("rest").map(|m| m.as_str()).unwrap_or_default();

        let kept = rest.trim_end_matches(|ch: char| {
            matches!(
                ch,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
            )
        });
        let trailing = &rest[kept.len()..];

        let spoken_host = match speak_url_host(host, has_scheme, use_spell_tags) {
            Some(spoken) => spoken,
            None => continue,
        };

        out.push_str(&text[cursor..whole.start()]);
        out.push_str(&spoken_host);
        out.push_str(&speak_url_path(kept));
        out.push_str(trailing);
        cursor = whole.end();
    }

    out.push_str(&text[cursor..]);
    collapse_inline_whitespace(&out)
}

/// Prepares text for TTS: URLs first, then hashtags.
///
/// Order matters. The URL pass consumes a trailing `#fragment` as part of the match and drops
/// it, so by the time hashtags are considered no `#` belonging to a URL is left to misread.
fn normalize_text_for_speech(text: &str, use_spell_tags: bool) -> String {
    let spoken = normalize_urls_in_description(text, use_spell_tags);
    normalize_hashtags_for_speech(&spoken, use_spell_tags)
}

/// Speaks `#QCStorm` as "hashtag" followed by the tag spelled out, matching how URLs are read.
fn normalize_hashtags_for_speech(text: &str, use_spell_tags: bool) -> String {
    if !text.contains('#') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + 32);
    let mut cursor = 0usize;

    for caps in hashtag_regex().captures_iter(text) {
        let whole = caps.get(0).expect("capture group 0 always exists");
        let tag = caps.name("tag").map(|m| m.as_str()).unwrap_or_default();
        if tag.is_empty() {
            continue;
        }

        out.push_str(&text[cursor..whole.start()]);
        out.push_str(caps.name("lead").map(|m| m.as_str()).unwrap_or_default());
        out.push_str("hashtag ");
        if use_spell_tags {
            out.push_str(URL_SPELL_MODE_ON);
            out.push(' ');
            out.push_str(tag);
            out.push(' ');
            out.push_str(URL_SPELL_MODE_OFF);
        } else {
            // Without control tags the engine still needs the letters separated to spell it.
            let mut letters = String::with_capacity(tag.len() * 2);
            for (index, ch) in tag.chars().enumerate() {
                if index > 0 {
                    letters.push(' ');
                }
                letters.extend(ch.to_lowercase());
            }
            out.push_str(&letters);
        }
        cursor = whole.end();
    }

    out.push_str(&text[cursor..]);
    collapse_inline_whitespace(&out)
}

fn hashtag_regex() -> &'static regex::Regex {
    // Preceded by start-of-text or whitespace so a "#" inside a word or a leftover URL fragment
    // is not treated as a tag. Tags are alphanumeric with underscores, as on every platform.
    // The regex crate has no lookbehind, so the preceding boundary is captured and written back.
    static HASHTAG_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?P<lead>^|\s)#(?P<tag>[A-Za-z0-9_]{1,140})")
            .expect("valid CAP hashtag regex")
    });
    &HASHTAG_RE
}

fn url_regex() -> &'static regex::Regex {
    static URL_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(
            r#"(?i)\b(?P<scheme>https?://)?(?P<host>(?:[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?\.)+[a-z]{2,24})(?::\d{1,5})?(?P<rest>[/?#][^\s<>"'\[\]{}]*)?"#,
        )
        .expect("valid CAP URL regex")
    });
    &URL_RE
}

fn speak_url_host(host: &str, has_scheme: bool, use_spell_tags: bool) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return None;
    }

    let tld = *labels.last()?;
    let starts_with_www = labels[0] == "www";
    if !has_scheme && !starts_with_www && !URL_BARE_HOST_TLDS.contains(&tld) {
        return None;
    }

    let suffix_labels = if labels.len() >= 3
        && URL_TWO_LABEL_SUFFIXES
            .contains(&format!("{}.{}", labels[labels.len() - 2], tld).as_str())
    {
        2
    } else {
        1
    };
    let suffix_start = labels.len().checked_sub(suffix_labels)?;

    let mut spoken = String::with_capacity(host.len() * 3);
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            spoken.push_str(" dot ");
        }
        // The final label is spoken as a word normally -- "dot com", "dot org" -- but a
        // two-letter country code is not a word, and Speechify reads ".ca" as "circa".
        let is_country_code_tld = index + 1 == labels.len()
            && label.len() == 2
            && label.bytes().all(|byte| byte.is_ascii_alphabetic());
        let spell = use_spell_tags
            && (is_country_code_tld || (index < suffix_start && !(index == 0 && starts_with_www)));
        if spell {
            spoken.push_str(URL_SPELL_MODE_ON);
            spoken.push(' ');
            spoken.push_str(label);
            spoken.push(' ');
            spoken.push_str(URL_SPELL_MODE_OFF);
        } else {
            spoken.push_str(label);
        }
    }

    Some(spoken)
}

fn speak_url_path(rest: &str) -> String {
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');

    let mut spoken = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        spoken.push_str(" slash ");
        spoken.push_str(
            &segment
                .replace('.', " dot ")
                .replace('-', " dash ")
                .replace('_', " underscore "),
        );
    }
    spoken
}

fn collapse_inline_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        match ch {
            ' ' | '\t' => pending_space = true,
            '\n' => {
                while out.ends_with(' ') {
                    out.pop();
                }
                pending_space = false;
                out.push('\n');
            }
            _ => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(ch);
            }
        }
    }
    if pending_space && !out.is_empty() {
        out.push(' ');
    }
    out
}

pub(crate) fn sanitize_cap_description(description: &str) -> String {
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

    expand_cap_times_for_tts(&cleaned)
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

    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }

    if cursor + 2 > bytes.len() {
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

    let (hour, minute) = parse_compact_time(digits)?;

    let tz_probe = cursor;
    let mut tz_cursor = tz_probe;
    while tz_cursor < bytes.len() && bytes[tz_cursor].is_ascii_whitespace() {
        tz_cursor += 1;
    }

    if tz_cursor > tz_probe {
        let tz_start = tz_cursor;
        while tz_cursor < bytes.len() && bytes[tz_cursor].is_ascii_alphabetic() {
            tz_cursor += 1;
        }
        if tz_cursor > tz_start {
            let timezone = &slice[tz_start..tz_cursor];
            if let Some(tz_spoken) = spoken_timezone_name(timezone) {
                if tz_cursor >= bytes.len() || !bytes[tz_cursor].is_ascii_alphanumeric() {
                    let expanded = format!("{hour}:{minute:02} {am_pm} {tz_spoken}");
                    return Some((tz_cursor, expanded));
                }
            }
        }
    }

    let expanded = format!("{hour}:{minute:02} {am_pm}");
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

/// The pronunciation fixes in `cap_tts_replacement_config.json`: each key is replaced by its value,
/// as a whole token, in a single pass.
///
/// Keys used to be applied as raw substrings, one after another in `HashMap` order, so `"S "`
/// turned "HAS ISSUED" into "HASouth ISSUED" -- and the all-caps ENDEC modes made collisions like
/// that routine. Now a key only matches where its word characters don't run into more word
/// characters, the longest key wins where several start at the same place, and replaced text is
/// never matched again, so the order of the file cannot change the result.
struct TtsReplacements {
    pattern: regex::Regex,
    table: HashMap<String, String>,
}

/// The dictionary that ships with the listener. Without one, every engine mispronounces county
/// names, abbreviations and N-1-1 numbers, so it applies unless `TTS_BUILTIN_REPLACEMENTS` turns it
/// off rather than only when someone has created the file.
static BUILTIN_TTS_REPLACEMENTS: once_cell::sync::Lazy<HashMap<String, String>> =
    once_cell::sync::Lazy::new(|| {
        serde_json::from_str(include_str!("../cap_tts_replacement_config.example.json"))
            .expect("the built-in TTS dictionary is a JSON object of strings")
    });

impl TtsReplacements {
    /// The built-in dictionary with the one at `path` laid over it, the file winning where both
    /// have a key.
    fn load(path: &Path, builtin: bool) -> Option<Self> {
        let mut table = if builtin {
            BUILTIN_TTS_REPLACEMENTS.clone()
        } else {
            HashMap::new()
        };

        match std::fs::read_to_string(path) {
            // A typo used to disable every replacement without a word.
            Ok(contents) => match serde_json::from_str::<HashMap<String, String>>(&contents) {
                Ok(own) => table.extend(own),
                Err(err) => warn!(
                    "Ignoring TTS replacements in {}: it must be a JSON object of strings ({}).",
                    path.display(),
                    err
                ),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                "Could not read TTS replacements from {}: {}",
                path.display(),
                err
            ),
        }

        Self::new(table)
    }

    fn new(mut table: HashMap<String, String>) -> Option<Self> {
        table.retain(|key, _| !key.is_empty());
        if table.is_empty() {
            return None;
        }

        // Alternation takes the first branch that matches, so longer keys go first -- "SSW " has
        // to beat "S " at the same spot -- and ties sort alphabetically to keep the pattern stable.
        let mut keys: Vec<&String> = table.keys().collect();
        keys.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        let alternation = keys
            .iter()
            .map(|key| replacement_key_pattern(key))
            .collect::<Vec<_>>()
            .join("|");

        match regex::Regex::new(&alternation) {
            Ok(pattern) => Some(Self { pattern, table }),
            Err(err) => {
                warn!("Ignoring TTS replacements: {}", err);
                None
            }
        }
    }

    fn apply(&self, text: &str) -> String {
        // A closure rather than a replacement string, so `$` in a value is not a capture reference.
        self.pattern
            .replace_all(text, |caps: &regex::Captures| {
                let found = &caps[0];
                // An all-lowercase key matches in any case, so "HWY " arrives here as itself and
                // is looked up by the key that matched it.
                self.table
                    .get(found)
                    .or_else(|| self.table.get(&found.to_lowercase()))
                    .cloned()
                    .unwrap_or_else(|| found.to_string())
            })
            .into_owned()
    }

    /// Applies the dictionary to prose only, before `normalize_text_for_speech` runs. URLs and
    /// hashtags pass through exactly as written for that step to read out, found by the very same
    /// detectors so the two cannot disagree about what a URL is. Running first also means the
    /// dictionary never sees what that step emits, which for Speechify includes engine-specific
    /// control codes.
    fn apply_to_prose(&self, text: &str) -> String {
        let mut protected: Vec<(usize, usize)> = url_regex()
            .find_iter(text)
            .chain(hashtag_regex().find_iter(text))
            .map(|found| (found.start(), found.end()))
            .collect();
        protected.sort_unstable();

        let mut out = String::with_capacity(text.len() + 32);
        let mut cursor = 0usize;
        for (start, end) in protected {
            if end <= cursor {
                continue;
            }
            let start = start.max(cursor);
            out.push_str(&self.apply(&text[cursor..start]));
            out.push_str(&text[start..end]);
            cursor = end;
        }
        out.push_str(&self.apply(&text[cursor..]));
        out
    }
}

/// A key's letters and digits must not run into more letters and digits at either end, so `"S "`
/// cannot match the end of "HAS " nor `"EAS"` the middle of "AREAS". An end that is already a space
/// or punctuation delimits itself and is matched as written.
///
/// Case follows the key, the way smartcase search does: an all-lowercase key such as `"hwy "`
/// also catches "HWY " in all-caps text, while a key with capitals matches exactly -- so `"LA"`
/// leaves the French "la" alone and `"S "` cannot reach the "s " of "it's ".
fn replacement_key_pattern(key: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let lead = if key.starts_with(is_word) { r"\b" } else { "" };
    let trail = if key.ends_with(is_word) { r"\b" } else { "" };
    let escaped = regex::escape(key);
    if key.chars().any(char::is_uppercase) {
        format!("{lead}{escaped}{trail}")
    } else {
        format!("{lead}(?i:{escaped}){trail}")
    }
}

fn raw_header_sender(header: &str) -> &str {
    header
        .trim_end_matches('-')
        .rsplit('-')
        .next()
        .unwrap_or_default()
}

/// Drops the sender every ENDEC template ends its sentence with -- "(KWO35)", "Message from
/// KWO35." or a bare "KWO35" -- so `TTS_READ_CALLSIGN` silences the call sign whichever mode wrote
/// the text. Line by line, for `ENDEC_MODE=ALL`.
fn strip_sender_clause(text: &str, sender: &str) -> String {
    let sender = sender.trim();
    if sender.is_empty() {
        return text.to_string();
    }
    let pattern = format!(
        r"(?i)[ \t]*(?:[.,;:]?[ \t]*message from[ \t]+)?\(?{}\)?[ \t]*\.?[ \t]*$",
        regex::escape(sender)
    );
    let Ok(clause) = regex::Regex::new(&pattern) else {
        return text.to_string();
    };

    text.lines()
        .map(|line| {
            let stripped = clause.replace(line, "");
            if stripped.len() == line.len() {
                return line.to_string();
            }
            let stripped = stripped.trim_end();
            if stripped.is_empty() || stripped.ends_with(['.', '!', '?']) {
                stripped.to_string()
            } else {
                format!("{stripped}.")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let alert_prefix_raw = build_eas_text(alert, timezone.as_str(), &config.endec_mode);

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

    // The SAGE's own front end, when Loquendo is standing in for the SAGE's Loquendo.
    let alert_prefix =
        if config.tts_engine == "loquendo" && config.endec_mode.eq_ignore_ascii_case("SAGE") {
            crate::hellotts::rewrite(&alert_prefix)
        } else {
            alert_prefix
        };
    let alert_prefix = if config.tts_read_callsign {
        alert_prefix
    } else {
        let header = build_cap_raw_header(
            &alert.originator_code,
            &alert.event_code,
            &alert.fips,
            alert.sent,
            alert.expires,
            &alert.source_url,
        );
        strip_sender_clause(&alert_prefix, raw_header_sender(&header))
    };

    // One dictionary for everything that gets spoken, whatever the engine.
    let replacements = TtsReplacements::load(
        &crate::paths::cap_tts_replacement_dict(),
        config.tts_builtin_replacements,
    );
    let respell = |text: &str| match &replacements {
        Some(table) => table.apply_to_prose(text),
        None => text.to_string(),
    };
    let alert_prefix = respell(&alert_prefix);

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

    let deduped_instructions = instructions
        .map(|instr| deduplicate_instructions(description, instr))
        .filter(|s| !s.is_empty());

    let use_spell_tags = config.tts_engine == "speechify";
    let spoken_description = normalize_text_for_speech(&respell(description), use_spell_tags);
    let spoken_instructions = deduped_instructions
        .as_deref()
        .map(|instr| normalize_text_for_speech(&respell(instr), use_spell_tags))
        .unwrap_or_default();

    let tts_text = format!("{alert_prefix} {spoken_description} {spoken_instructions}");
    // Exactly what the engine is handed, so a replacement that misfires can be seen, not guessed.
    debug!(
        "CAP TTS text for alert {} ({}): {}",
        alert.identifier, event_code, tts_text
    );

    let Some((sample_rate, samples)) =
        synthesize_tts_samples(config, &tts_text, &alert.identifier, event_code).await?
    else {
        warn!(
            "CAP TTS produced no audio for alert {} ({}) from {} character(s) of text.",
            alert.identifier,
            event_code,
            tts_text.chars().count()
        );
        return Ok(None);
    };

    write_wav_i16(&tts_path, sample_rate, &samples).await?;

    let metadata = fs::metadata(&tts_path).await?;
    if metadata.len() <= CAP_TTS_EMPTY_WAV_BYTES {
        warn!(
            "CAP TTS produced no audio for alert {} ({}): {} byte(s) from {} character(s) of text.",
            alert.identifier,
            event_code,
            metadata.len(),
            tts_text.chars().count()
        );
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

/// Whether a failed engine run means the binary is missing or not executable, as opposed to an
/// engine that ran and choked on its input.
fn tts_engine_could_not_start(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            )
        })
    })
}

/// How each engine is named aloud in the test alert's narration.
fn tts_engine_spoken_name(engine: &str) -> &str {
    match engine {
        "piper" => "Piper",
        "espeak-ng" => "e Speak",
        "speechify" => "Speechify",
        "cepstral" => "Cepstral",
        "loquendo" => "Loquendo",
        other => other,
    }
}

/// The test alert's narration. Generic, but it names the engine, so a recording can be matched to
/// the configuration that produced it when several engines are being compared.
pub(crate) fn test_alert_tts_script(config: &Config) -> String {
    format!(
        "This is a test of the E A S Listener text to speech system, using the {} engine. \
         If you can hear this message, alerts that arrive without audio will be read aloud in \
         this voice. No action is required. This concludes this test of E A S Listener.",
        tts_engine_spoken_name(&config.tts_engine)
    )
}

/// Narrates the test alert through exactly the path CAP alerts take -- voice resolution, chunking
/// and the halve-and-retry recovery included -- so a pass here means CAP narration will work.
pub(crate) async fn synthesize_test_alert_narration(
    config: &Config,
) -> Result<Option<(u32, Vec<i16>)>> {
    let use_spell_tags = config.tts_engine == "speechify";
    let text = normalize_text_for_speech(&test_alert_tts_script(config), use_spell_tags);

    // Shared with CAP narration, so a test never runs an engine alongside a real alert's.
    let _guard = cap_tts_synth_lock().lock().await;
    synthesize_tts_samples(config, &text, "test-alert", "RWT").await
}

/// One synthesis attempt: either audio, or a reason it produced none.
enum TtsAttempt {
    Audio(u32, Vec<i16>),
    NoAudio(String),
}

/// Synthesizes `text`, halving and retrying any passage the engine cannot handle.
///
/// Speechify's guest heap ceiling moves with content, not just length, so a passage that comes
/// back as a header-only WAV is split and retried rather than written off. Returns `None` only
/// when nothing at all could be synthesized.
async fn synthesize_tts_samples(
    config: &Config,
    text: &str,
    alert_id: &str,
    event_code: &str,
) -> Result<Option<(u32, Vec<i16>)>> {
    // Checked once up front so a misconfigured engine fails outright instead of being retried
    // as though it were a length problem.
    if !matches!(
        config.tts_engine.as_str(),
        "piper" | "espeak-ng" | "speechify" | "cepstral" | "loquendo"
    ) {
        return Err(anyhow!(
            "Unknown TTS engine '{}'. Supported: {}",
            config.tts_engine,
            CAP_TTS_SUPPORTED_ENGINES
        ));
    }
    // A Speechify voice outside the default folder is the user's own, which no fetch can supply,
    // so it is reported before anything is downloaded.
    if config.tts_engine == "speechify" && config.spfy_voice_dir != crate::paths::spfy_voice_dir() {
        speechify_voice(config)?;
    }
    // Before the voice checks below, which would otherwise fail on a voice that is only missing
    // because it has not been fetched yet.
    ensure_tts_engine(config).await?;
    match config.tts_engine.as_str() {
        "speechify" => {
            speechify_voice(config)?;
        }
        "cepstral" => {
            cepstral_voice_dir(config)?;
        }
        _ => {}
    }

    let mut pending: VecDeque<String> = split_tts_text(text, CAP_TTS_MAX_CHUNK_CHARS)
        .into_iter()
        .collect();
    if pending.is_empty() {
        return Ok(None);
    }

    let mut combined: Vec<i16> = Vec::new();
    let mut sample_rate: Option<u32> = None;
    let mut synthesized = 0usize;
    let mut dropped = 0usize;
    let mut attempts = 0usize;
    // Lowered whenever a passage comes back empty, and applied to everything still queued. The
    // engine's ceiling is a property of the run, not of one passage, so rediscovering it chunk
    // by chunk would burn an attempt per chunk per halving.
    let mut size_limit = CAP_TTS_MAX_CHUNK_CHARS;

    while let Some(piece) = pending.pop_front() {
        attempts += 1;
        if attempts > CAP_TTS_MAX_SYNTH_ATTEMPTS {
            warn!(
                "CAP TTS for alert {} ({}) hit the {}-attempt ceiling with {} passage(s) still queued.",
                alert_id,
                event_code,
                CAP_TTS_MAX_SYNTH_ATTEMPTS,
                pending.len() + 1
            );
            dropped += pending.len() + 1;
            break;
        }

        let reason = match synthesize_tts_piece(config, &piece).await? {
            TtsAttempt::Audio(rate, samples) => {
                match sample_rate {
                    Some(existing) if existing != rate => {
                        return Err(anyhow!(
                            "TTS returned {} Hz for one passage but {} Hz for an earlier one",
                            rate,
                            existing
                        ));
                    }
                    Some(_) => {}
                    None => sample_rate = Some(rate),
                }
                combined.extend_from_slice(&samples);
                synthesized += 1;
                continue;
            }
            TtsAttempt::NoAudio(reason) => reason,
        };

        let chars = piece.chars().count();
        if chars <= CAP_TTS_MIN_CHUNK_CHARS {
            warn!(
                "CAP TTS could not synthesize a {}-character passage of alert {} ({}); omitting it. {}",
                chars, alert_id, event_code, reason
            );
            dropped += 1;
            continue;
        }

        let next_limit = chars.div_ceil(2).max(CAP_TTS_MIN_CHUNK_CHARS);
        if next_limit < size_limit {
            size_limit = next_limit;
            let queued: Vec<String> = pending.drain(..).collect();
            pending = queued
                .iter()
                .flat_map(|queued_piece| split_tts_text(queued_piece, size_limit))
                .collect();
        }

        let halves = split_tts_text(&piece, size_limit);
        if halves.len() < 2 {
            warn!(
                "CAP TTS could not split a {}-character passage of alert {} ({}) any further; omitting it. {}",
                chars, alert_id, event_code, reason
            );
            dropped += 1;
            continue;
        }

        info!(
            "CAP TTS retrying a {}-character passage of alert {} ({}) as {} pieces of at most {} characters. {}",
            chars,
            alert_id,
            event_code,
            halves.len(),
            size_limit,
            reason
        );
        for half in halves.into_iter().rev() {
            pending.push_front(half);
        }
    }

    if dropped > 0 {
        warn!(
            "CAP TTS omitted {} passage(s) of alert {} ({}); {} passage(s) were synthesized.",
            dropped, alert_id, event_code, synthesized
        );
    }

    if synthesized == 0 {
        return Ok(None);
    }

    let rate = sample_rate.ok_or_else(|| anyhow!("TTS produced audio with no sample rate"))?;
    Ok(Some((rate, combined)))
}

async fn synthesize_tts_piece(config: &Config, text: &str) -> Result<TtsAttempt> {
    let out_path = tempfile::Builder::new()
        .prefix("cap_tts_chunk_")
        .suffix(".wav")
        .tempfile()
        .context("Failed to create TTS output file")?
        .into_temp_path();
    // TempPath is AsRef for both Path and OsStr, so pin the one the calls below need.
    let out_path: &Path = out_path.as_ref();

    // A failure here is reported rather than propagated: for the engine that actually has a
    // ceiling the fix is to retry with less text, and the caller decides that. An engine that
    // could not be started at all is the exception -- less text will not start it either.
    let diagnostic = match run_tts_engine(config, text, out_path).await {
        Ok(stderr) => stderr,
        Err(err) if tts_engine_could_not_start(&err) => return Err(err),
        Err(err) => return Ok(TtsAttempt::NoAudio(err.to_string())),
    };

    let byte_len = match fs::metadata(out_path).await {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            return Ok(TtsAttempt::NoAudio(format!(
                "engine wrote no output file: {err}"
            )))
        }
    };

    if byte_len <= CAP_TTS_EMPTY_WAV_BYTES {
        let detail = if diagnostic.is_empty() {
            String::new()
        } else {
            format!(" Engine said: {diagnostic}")
        };
        return Ok(TtsAttempt::NoAudio(format!(
            "{} character(s) produced {} byte(s) of WAV.{}",
            text.chars().count(),
            byte_len,
            detail
        )));
    }

    match read_wav_i16(out_path).await {
        Ok((rate, samples)) => Ok(TtsAttempt::Audio(rate, samples)),
        Err(err) => Ok(TtsAttempt::NoAudio(format!("unreadable WAV: {err}"))),
    }
}

/// Resolves the configured Cepstral voice to a directory, checked before the engine is spawned so
/// a voice that was never fetched reports what to run instead of the engine's bare
/// "cannot open voice".
/// The three files `spfy_synth` needs to open one voice.
#[derive(Debug, PartialEq, Eq)]
struct SpeechifyVoice {
    vin: PathBuf,
    vdb: PathBuf,
    vcf: PathBuf,
}

/// Where a Speechify voice could live, given `TTS_MODEL`.
///
/// `SPFY_VOICE_DIR` historically pointed straight at one voice (`.../voices/tom`), but a bare
/// `TTS_MODEL` reads naturally as a voice sitting *inside* it. Both layouts are in the wild, so a
/// name is looked for in each and the first that actually holds a voice wins.
fn speechify_voice_dir_candidates(config: &Config) -> Vec<PathBuf> {
    let Some(model) = config
        .tts_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return vec![config.spfy_voice_dir.clone()];
    };

    let model_path = Path::new(model);
    if model_path.is_absolute() {
        return vec![model_path.to_path_buf()];
    }

    if model.contains(['/', '\\']) {
        // A relative path is anchored to the instance's directory, like config.json's other
        // paths, then to where the shared voices are, with the working directory kept last.
        let mut candidates = vec![
            crate::paths::in_app_root(model_path),
            crate::paths::assets_root().join(model_path),
            crate::paths::in_install_root(model_path),
            model_path.to_path_buf(),
        ];
        candidates.dedup();
        return candidates;
    }

    let mut candidates = vec![config.spfy_voice_dir.join(model)];
    if let Some(parent) = config.spfy_voice_dir.parent() {
        let sibling = parent.join(model);
        if !candidates.contains(&sibling) {
            candidates.push(sibling);
        }
    }
    candidates
}

/// Resolves `TTS_MODEL` to a Speechify voice, or explains exactly where it looked.
///
/// A voice directory holds three files named after the directory itself -- `crstom/crstom.vin`,
/// `crstom8.vdb`, `crstom.vcf` -- so the stem comes from the directory name rather than being
/// pinned to Tom. The `8`/`16` in the `.vdb` is its sample rate; Tom ships both and everything else
/// ships only 8, so 8 is preferred and 16 is accepted when it is all that is there.
fn speechify_voice(config: &Config) -> Result<SpeechifyVoice> {
    const VDB_RATES: &[&str] = &["8", "16"];

    let candidates = speechify_voice_dir_candidates(config);
    let mut tried = Vec::new();

    for dir in &candidates {
        let Some(stem) = dir.file_name().and_then(|name| name.to_str()) else {
            tried.push(format!("{} (not a voice directory)", dir.display()));
            continue;
        };

        if !dir.is_dir() {
            tried.push(format!("{} (no such directory)", dir.display()));
            continue;
        }

        let vin = dir.join(format!("{stem}.vin"));
        let vcf = dir.join(format!("{stem}.vcf"));
        let vdb = VDB_RATES
            .iter()
            .map(|rate| dir.join(format!("{stem}{rate}.vdb")))
            .find(|path| path.is_file());

        let mut missing = Vec::new();
        if !vin.is_file() {
            missing.push(format!("{stem}.vin"));
        }
        if vdb.is_none() {
            missing.push(format!("{stem}8.vdb"));
        }
        if !vcf.is_file() {
            missing.push(format!("{stem}.vcf"));
        }

        if let Some(vdb) = vdb {
            if missing.is_empty() {
                return Ok(SpeechifyVoice { vin, vdb, vcf });
            }
        }
        tried.push(format!(
            "{} (missing {})",
            dir.display(),
            missing.join(", ")
        ));
    }

    let requested = config
        .tts_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(|model| format!("TTS_MODEL '{model}'"))
        .unwrap_or_else(|| format!("SPFY_VOICE_DIR {}", config.spfy_voice_dir.display()));

    Err(anyhow!(
        "Could not resolve a Speechify voice from {}. A voice directory holds <name>.vin, \
         <name>8.vdb and <name>.vcf, all named after the directory itself. Looked in: {}",
        requested,
        tried.join("; ")
    ))
}

/// The machine's shared voice folders; a test sees none, so a checkout that has voices of its own
/// does not answer for it.
fn shared_cep6_roots() -> Vec<PathBuf> {
    if cfg!(test) {
        Vec::new()
    } else {
        crate::paths::cep6_voice_roots()
    }
}

fn cepstral_voice_dir(config: &Config) -> Result<PathBuf> {
    let voice = config
        .tts_model
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(CAP_TTS_DEFAULT_CEPSTRAL_VOICE);

    if voice.contains(['/', '\\']) || voice.contains("..") {
        return Err(anyhow!(
            "TTS_MODEL '{}' is not a Cepstral voice name. Use one of: {}",
            voice,
            CAP_TTS_CEPSTRAL_VOICES
        ));
    }

    // Where this listener fetches voices first, then wherever another instance has -- and the
    // state folder, where they were kept before they were shared.
    let mut roots = vec![config.cep6_voice_dir.clone()];
    roots.extend(shared_cep6_roots());
    roots.push(config.shared_state_dir.join("tts_voices").join("cep6"));
    if let Some(found) = roots
        .iter()
        .map(|root| root.join(voice))
        .find(|dir| dir.join("voice.idx").is_file())
    {
        return Ok(found);
    }
    Err(anyhow!(
        "Cepstral voice '{}' is not installed in {}. Run: tts_voices/cep6/fetch_voices.sh -d {} {}",
        voice,
        config.cep6_voice_dir.display(),
        config.cep6_voice_dir.display(),
        voice
    ))
}

/// Text as loqdave reads it: one Latin-1 byte per character. The punctuation NWS and CAP-CP text
/// is full of has an ASCII stand-in; anything else outside Latin-1 becomes a space, which the
/// engine treats as a pause rather than reading out a replacement character.
fn to_latin1(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{0000}'..='\u{00FF}' => bytes.push(ch as u8),
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => bytes.push(b'\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => bytes.push(b'"'),
            '\u{2010}'..='\u{2015}' | '\u{2212}' => bytes.push(b'-'),
            '\u{2026}' => bytes.extend_from_slice(b"..."),
            _ => bytes.push(b' '),
        }
    }
    bytes
}

/// loqdave writes a fixed Loquendo banner and an interactive prompt to stderr on every run, so a
/// real diagnostic arrives buried in nine lines of boilerplate. Keep only what is not boilerplate.
fn strip_loqdave_banner(stderr: &str) -> String {
    const BANNER_PREFIXES: [&str; 8] = [
        "Copyright (C)",
        "LoquendoTTS",
        "Multilingual Text-To-Speech",
        "Speaker =",
        "Speech Format =",
        "Audio destination library =",
        "End your sentence with",
        "Press Ctrl-D to exit.",
    ];

    stderr
        .lines()
        .map(|line| line.trim_start_matches('>').trim())
        .filter(|line| {
            !line.is_empty()
                && !BANNER_PREFIXES
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Fetches the configured engine, and the voice it needs, the first time they are wanted: a
/// standalone install ships none of them. An engine that cannot be had is reported the way one
/// that fails to launch is, so the caller does not retry it passage by passage.
pub(crate) async fn ensure_tts_engine(config: &Config) -> Result<()> {
    use crate::components::{self, CEP6, ESPEAK_NG, LOQDAVE, PIPER, SPFY_SYNTH};
    let unavailable = |why: String| {
        anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::NotFound, why)).context(
            format!("The {} TTS engine is not available", config.tts_engine),
        )
    };

    match config.tts_engine.as_str() {
        "piper" => {
            components::ensure(&PIPER).await.map_err(unavailable)?;
            let default_model = crate::paths::piper_default_model();
            if config.tts_model.is_none() && !default_model.is_file() {
                components::refetch(&PIPER, move || default_model.is_file())
                    .await
                    .map_err(unavailable)?;
            }
        }
        "espeak-ng" => {
            components::ensure(&ESPEAK_NG).await.map_err(unavailable)?;
        }
        "speechify" => {
            components::ensure(&SPFY_SYNTH).await.map_err(unavailable)?;
            // The Tom voice comes with the engine's download, into the default voice folder; a
            // voice anywhere else is the user's own to install.
            if speechify_voice(config).is_err()
                && config.spfy_voice_dir == crate::paths::spfy_voice_dir()
            {
                let config = config.clone();
                components::refetch(&SPFY_SYNTH, move || speechify_voice(&config).is_ok())
                    .await
                    .map_err(unavailable)?;
            }
        }
        "cepstral" => {
            components::ensure(&CEP6).await.map_err(unavailable)?;
            if cepstral_voice_dir(config).is_err() {
                let voice = config
                    .tts_model
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(CAP_TTS_DEFAULT_CEPSTRAL_VOICE);
                // A name that is not one of the four is left to cepstral_voice_dir to explain.
                if CAP_TTS_CEPSTRAL_VOICES
                    .split(", ")
                    .any(|known| known == voice)
                {
                    let installed = config.clone();
                    components::ensure_cepstral_voice(&config.cep6_voice_dir, voice, move || {
                        cepstral_voice_dir(&installed).is_ok()
                    })
                    .await
                    .map_err(unavailable)?;
                }
            }
        }
        "loquendo" => {
            components::ensure(&LOQDAVE).await.map_err(unavailable)?;
        }
        _ => {}
    }
    Ok(())
}

/// Started with the listener and after each reload, so the first alert does not wait on a
/// download of the engine it is about to use.
pub(crate) async fn prefetch_tts_engine(config: Config) {
    match ensure_tts_engine(&config).await {
        Ok(()) => {}
        Err(err) => warn!("{err:#}"),
    }
}

async fn run_tts_engine(config: &Config, text: &str, out_path: &Path) -> Result<String> {
    ensure_tts_engine(config).await?;

    // Engines that take the text as a command-line argument blow past the per-argument limit on
    // a long alert, so every engine reads from a file instead. The handle is closed but the path
    // is unlinked on drop, so it is cleaned up on every path out of this function.
    let input_path = {
        use std::io::Write as _;

        let mut input_file = tempfile::Builder::new()
            .prefix("cap_tts_")
            .suffix(".txt")
            .tempfile()
            .context("Failed to create TTS input file")?;
        // loqdave drives the engine with InputTextCoding=ansi, one byte per character; handed
        // UTF-8, every curly quote or accent would be read as two or three wrong letters.
        let bytes = if config.tts_engine == "loquendo" {
            std::borrow::Cow::Owned(to_latin1(text))
        } else {
            std::borrow::Cow::Borrowed(text.as_bytes())
        };
        input_file
            .write_all(&bytes)
            .context("Failed to write TTS input file")?;
        input_file
            .flush()
            .context("Failed to flush TTS input file")?;
        input_file.into_temp_path()
    };

    let mut diagnostic = String::new();
    let status = match config.tts_engine.as_str() {
        "piper" => {
            let default_model = crate::paths::piper_default_model();
            let model: &Path = config
                .tts_model
                .as_deref()
                .map_or(default_model.as_path(), Path::new);
            let mut child = Command::new(crate::components::binary(&crate::components::PIPER))
                .arg("--model")
                .arg(model)
                .arg("--output_file")
                .arg(out_path)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .context("Failed to spawn Piper TTS process")?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(text.as_bytes())
                    .await
                    .context("Failed to write text to Piper stdin")?;
                drop(stdin);
            }
            child
                .wait()
                .await
                .context("Failed to wait for Piper TTS process")?
        }
        "espeak-ng" => {
            let output = Command::new(crate::components::binary(&crate::components::ESPEAK_NG))
                .arg("-w")
                .arg(out_path)
                .arg("-f")
                .arg(input_path.as_os_str())
                .output()
                .await
                .context("Failed to execute espeak-ng TTS command")?;
            diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
            output.status
        }
        "speechify" => {
            let voice = speechify_voice(config)?;
            // With --file the "<text>" positional is omitted, leaving the three voice paths and
            // the output path.
            let output = Command::new(crate::components::binary(&crate::components::SPFY_SYNTH))
                .arg("--file")
                .arg(input_path.as_os_str())
                .arg(&voice.vin)
                .arg(&voice.vdb)
                .arg(&voice.vcf)
                .arg(out_path)
                .output()
                .await
                .context("Failed to execute Speechify TTS command")?;
            // spfy_synth exits 0 even when its guest heap runs dry, so stderr is the only
            // evidence of what went wrong and has to survive a successful exit.
            diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !output.status.success() {
                return Err(anyhow!(
                    "Speechify (spfy_synth) failed with status {:?}: {}",
                    output.status.code(),
                    diagnostic
                ));
            }
            output.status
        }
        "cepstral" => {
            let voice_dir = cepstral_voice_dir(config)?;
            let output = Command::new(crate::components::binary(&crate::components::CEP6))
                .arg(&voice_dir)
                .arg("file")
                .arg(input_path.as_os_str())
                .arg(out_path)
                .output()
                .await
                .context("Failed to execute Cepstral (cep6) TTS command")?;
            diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_string();
            output.status
        }
        "loquendo" => {
            let mut command = Command::new(crate::components::binary(&crate::components::LOQDAVE));
            // --data replaces the voice built into loqdave rather than adding to it, so it is
            // only passed when someone has pointed it somewhere on purpose.
            if !config.loq6_data_dir.as_os_str().is_empty() {
                command.arg("--data").arg(&config.loq6_data_dir);
            }
            let output = command
                .arg("--file")
                .arg(input_path.as_os_str())
                .arg("--out")
                .arg(out_path)
                .output()
                .await
                .context("Failed to execute Loquendo (loqdave) TTS command")?;
            diagnostic = strip_loqdave_banner(&String::from_utf8_lossy(&output.stderr));
            output.status
        }
        other => {
            return Err(anyhow!(
                "Unknown TTS engine '{}'. Supported: {}",
                other,
                CAP_TTS_SUPPORTED_ENGINES
            ));
        }
    };

    if !status.success() {
        return Err(anyhow!(
            "CAP TTS command failed with status {:?}: {}",
            status.code(),
            diagnostic
        ));
    }

    Ok(diagnostic)
}

/// Splits TTS text into pieces no longer than `max_chars`, preferring sentence boundaries so a
/// chunk never starts mid-thought. Falls back to word boundaries for a single long sentence, and
/// to character boundaries for a single long word.
fn split_tts_text(text: &str, max_chars: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || max_chars == 0 {
        return Vec::new();
    }
    if trimmed.chars().count() <= max_chars {
        return vec![trimmed.to_string()];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for sentence in split_sentences(trimmed) {
        for piece in split_oversized(&sentence, max_chars) {
            let would_be = if current.is_empty() {
                piece.chars().count()
            } else {
                current.chars().count() + 1 + piece.chars().count()
            };

            if !current.is_empty() && would_be > max_chars {
                chunks.push(std::mem::take(&mut current));
            }

            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(&piece);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') && chars.peek().is_none_or(|next| next.is_whitespace()) {
            let sentence = current.trim().to_string();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            current.clear();
        }
    }

    let tail = current.trim().to_string();
    if !tail.is_empty() {
        sentences.push(tail);
    }

    sentences
}

fn split_oversized(sentence: &str, max_chars: usize) -> Vec<String> {
    if sentence.chars().count() <= max_chars {
        return vec![sentence.to_string()];
    }

    let mut pieces = Vec::new();
    let mut current = String::new();

    for word in sentence.split_whitespace() {
        if word.chars().count() > max_chars {
            if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
            }
            for ch in word.chars() {
                if current.chars().count() == max_chars {
                    pieces.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
            continue;
        }

        let would_be = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };

        if !current.is_empty() && would_be > max_chars {
            pieces.push(std::mem::take(&mut current));
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    if !current.is_empty() {
        pieces.push(current);
    }

    pieces
}

async fn read_wav_i16(path: &Path) -> Result<(u32, Vec<i16>)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(u32, Vec<i16>)> {
        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        let samples = reader
            .samples::<i16>()
            .collect::<std::result::Result<Vec<i16>, _>>()?;
        Ok((spec.sample_rate, samples))
    })
    .await?
}

fn deduplicate_instructions(description: &str, instructions: &str) -> String {
    let desc_sentences: Vec<&str> = description
        .split('.')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let kept: Vec<&str> = instructions
        .split('.')
        .map(str::trim)
        .filter(|sentence| {
            if sentence.is_empty() {
                return false;
            }
            !desc_sentences
                .iter()
                .any(|d| d.eq_ignore_ascii_case(sentence))
        })
        .collect();

    if kept.is_empty() {
        return String::new();
    }

    let mut result = kept.join(". ");
    if instructions.trim_end().ends_with('.') {
        result.push('.');
    }
    result
}

fn cap_tts_synth_lock() -> &'static Mutex<()> {
    CAP_TTS_SYNTH_LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn child_text<'a, 'input>(node: Node<'a, 'input>, child_name: &str) -> Option<String> {
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

pub(crate) fn extract_same_from_container<'a, 'input>(
    container: Node<'a, 'input>,
) -> Option<String> {
    let value_name = child_text(container, "valueName").unwrap_or_default();
    let value = child_text(container, "value").unwrap_or_default();
    if value_name.eq_ignore_ascii_case("SAME") && !value.is_empty() {
        Some(value)
    } else {
        None
    }
}

pub(crate) fn split_fips_codes(value: &str) -> Vec<String> {
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

pub(crate) fn parse_cap_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|ts| ts.with_timezone(&Utc))
        .ok()
}

fn build_eas_text(alert: &CapAlert, timezone: &str, endec_mode: &str) -> String {
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

    let eas_text = crate::e2t_ng::E2T(&header, endec_mode, alert.canadian, Some(timezone));
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

pub(crate) fn normalize_event_code(event_code: &str) -> String {
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

pub(crate) fn build_cap_raw_header(
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

/// What surrounds a CAP alert's message audio in its recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordingFraming {
    /// SAME header, attention tone, the message, then NNNN.
    Same,
    /// The Alert Ready attention signal, once, then the message -- nothing after it. Also keeps
    /// the SAME header out of webhooks, since nothing about the alert went out as SAME.
    AlertReady,
    /// `EMIT_HEADER_TONES` is off: the message audio alone, with nothing around it. Like
    /// `AlertReady`, no header goes out, so none is published either.
    Bare,
}

fn recording_framing(config: &Config, alert: &CapAlert) -> RecordingFraming {
    if !config.emit_header_tones {
        return RecordingFraming::Bare;
    }

    let from_naad = cap_header_source_marker(&alert.source_url) == CAP_HEADER_SOURCE_MARKER_NAAD;
    if from_naad && config.capcp_use_alert_ready_tone {
        RecordingFraming::AlertReady
    } else {
        RecordingFraming::Same
    }
}

/// Whether an alert came from any CAP feed -- IPAWS, IPAWS WEA or NAAD -- judged by the sender ID
/// its synthesised header carries. The one place that knows the full set; checking for "IPAWS"
/// alone is how CAP-CP alerts used to fall through.
pub(crate) fn is_cap_raw_header(raw_header: &str) -> bool {
    cap_feed_of_raw_header(raw_header).is_some()
}

/// Which CAP feed a header was synthesised for, by its sender ID: `IPAWSCAP`, `IPAWSWEA` or
/// `NAADSCAP`. `None` is a header decoded off the air.
pub(crate) fn cap_feed_of_raw_header(raw_header: &str) -> Option<&'static str> {
    let (_, sender) = raw_header.trim().trim_end_matches('-').rsplit_once('-')?;
    [
        CAP_HEADER_SOURCE_MARKER_CAP,
        CAP_HEADER_SOURCE_MARKER_WEA,
        CAP_HEADER_SOURCE_MARKER_NAAD,
    ]
    .into_iter()
    .find(|marker| *marker == sender.trim())
}

fn cap_header_source_marker(source_hint: &str) -> &'static str {
    if contains_ascii_ignore_case(source_hint, b"naad")
        || contains_ascii_ignore_case(source_hint, b"pelmorex")
    {
        return CAP_HEADER_SOURCE_MARKER_NAAD;
    }

    if source_hint
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("WEA-"))
        || contains_ascii_ignore_case(source_hint, b"publicwea")
        || contains_ascii_ignore_case(source_hint, b"/wea/")
        || contains_ascii_ignore_case(source_hint, b"/wea#")
        || source_hint.rsplit_once('#').is_some_and(|(_, fragment)| {
            fragment
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("WEA-"))
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
    let alert_desc = build_eas_text(alert, timezone.as_str(), &config.endec_mode);

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

pub(crate) fn build_dedupe_key(alert: &CapAlert) -> String {
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

pub(crate) fn extract_parameter_value<'a, 'input>(
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

pub(crate) fn normalize_originator_code(value: &str) -> String {
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

pub(crate) fn normalize_fips_code(value: &str) -> Option<String> {
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

pub(crate) fn is_audio_resource(
    mime: Option<&str>,
    uri: Option<&str>,
    deref_uri: Option<&str>,
) -> bool {
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
    framing: RecordingFraming,
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

    // Named for the feed the alert came from -- IPAWS, IPAWS WEA or NAAD -- by the same function
    // that picked its header's sender ID, so the two can never disagree.
    let source_marker = cap_header_source_marker(&alert.source_url);
    let framed = match framing {
        RecordingFraming::Same => {
            build_recording_with_same_header(
                config,
                raw_header,
                event_code,
                source_marker,
                &cap_audio_path,
            )
            .await
        }
        RecordingFraming::AlertReady => {
            build_recording_with_alert_ready_tone(
                config,
                event_code,
                source_marker,
                &cap_audio_path,
            )
            .await
        }
        RecordingFraming::Bare => {
            // Still goes through the concat so the recording is named and encoded the same way
            // every other one is.
            concat_recording(config, event_code, source_marker, &[&cap_audio_path]).await
        }
    };
    let (output_path, should_remove_cap_audio_input) = match framed {
        Ok(path) => (Some(path), true),
        Err(err) => {
            warn!(
                "Failed to add {:?} framing to CAP audio, using the raw CAP audio file: {}",
                framing, err
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

fn recording_segment_id(event_code: &str) -> String {
    format!(
        "{}_{}",
        sanitize_filename_label(event_code),
        sanitize_filename_label(&Utc::now().timestamp_millis().to_string())
    )
}

/// The custom opening for an alert from the feed `source_marker` names: CAP-CP alerts have their
/// own, so a config running both NAAD and IPAWS can change one without the other.
fn header_audio_for(config: &Config, source_marker: &str) -> Option<PathBuf> {
    let custom = if source_marker == CAP_HEADER_SOURCE_MARKER_NAAD {
        config.capcp_header_audio_override()
    } else {
        config.header_audio_override()
    };
    custom.map(Path::to_path_buf)
}

/// SAME framing: header, attention tone, a second of silence, the message, silence, NNNN. With
/// custom header audio set (`header_audio_for`), that file replaces the header and attention
/// tone; the closing NNNN stays, since only the opening is replaced.
async fn build_recording_with_same_header(
    config: &Config,
    raw_header: &str,
    event_code: &str,
    source_marker: &str,
    cap_audio_input_path: &Path,
) -> Result<PathBuf> {
    let tmp_id = recording_segment_id(event_code);
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

    let custom_opening = header_audio_for(config, source_marker);

    let result = async {
        let silence_samples = header::generate_silence_for_duration(CAP_RECORDING_SAMPLE_RATE, 1.0);
        let nnnn_samples = header::generate_same_header_samples(
            "NNNN",
            CAP_RECORDING_SAMPLE_RATE,
            CAP_HEADER_AMPLITUDE,
        )?;

        let mut segments: Vec<&Path> = Vec::with_capacity(6);
        if let Some(custom) = &custom_opening {
            segments.push(custom);
        } else {
            let header_samples = header::generate_same_header_samples(
                raw_header,
                CAP_RECORDING_SAMPLE_RATE,
                CAP_HEADER_AMPLITUDE,
            )?;
            let attn_samples =
                header::generate_attention_tone(CAP_RECORDING_SAMPLE_RATE, CAP_HEADER_AMPLITUDE)?;
            write_wav_i16(&header_path, CAP_RECORDING_SAMPLE_RATE, &header_samples).await?;
            write_wav_i16(&attn_tone_path, CAP_RECORDING_SAMPLE_RATE, &attn_samples).await?;
            segments.push(&header_path);
            segments.push(&attn_tone_path);
        }

        write_wav_i16(&silence_path, CAP_RECORDING_SAMPLE_RATE, &silence_samples).await?;
        write_wav_i16(&nnnn_path, CAP_RECORDING_SAMPLE_RATE, &nnnn_samples).await?;
        segments.extend_from_slice(&[
            &silence_path,
            cap_audio_input_path,
            &silence_path,
            &nnnn_path,
        ]);

        concat_recording(config, event_code, source_marker, &segments).await
    }
    .await;

    for temp in [&header_path, &nnnn_path, &silence_path, &attn_tone_path] {
        let _ = fs::remove_file(temp).await;
    }
    result
}

/// Alert Ready framing: the attention signal exactly once, a second of silence, then the message.
/// Unlike SAME there is nothing after the message -- no closing tone, no NNNN.
/// Custom header audio (`header_audio_for`) replaces the attention signal when it is set.
async fn build_recording_with_alert_ready_tone(
    config: &Config,
    event_code: &str,
    source_marker: &str,
    cap_audio_input_path: &Path,
) -> Result<PathBuf> {
    let tmp_id = recording_segment_id(event_code);
    let tone_path = config
        .recording_dir
        .join(format!("cap_alert_ready_{}.wav", tmp_id));
    let silence_path = config
        .recording_dir
        .join(format!("cap_silence_{}.wav", tmp_id));

    let custom_opening = header_audio_for(config, source_marker);

    let result = async {
        let opening: &Path = match &custom_opening {
            Some(custom) => custom,
            None => {
                fs::write(&tone_path, ALERT_READY_TONE_WAV).await?;
                &tone_path
            }
        };
        let silence_samples = header::generate_silence_for_duration(CAP_RECORDING_SAMPLE_RATE, 1.0);
        write_wav_i16(&silence_path, CAP_RECORDING_SAMPLE_RATE, &silence_samples).await?;

        concat_recording(
            config,
            event_code,
            source_marker,
            &[opening, &silence_path, cap_audio_input_path],
        )
        .await
    }
    .await;

    for temp in [&tone_path, &silence_path] {
        let _ = fs::remove_file(temp).await;
    }
    result
}

fn concat_filter(inputs: usize) -> String {
    let labels: String = (0..inputs).map(|index| format!("[{index}:a]")).collect();
    format!("{labels}concat=n={inputs}:v=0:a=1[outa]")
}

/// Joins `segments` in order into one recording in the configured storage format. ffmpeg's concat
/// filter negotiates a common rate and layout, so the segments do not have to match.
async fn concat_recording(
    config: &Config,
    event_code: &str,
    source_marker: &str,
    segments: &[&Path],
) -> Result<PathBuf> {
    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let storage_saver = config.storage_saver_mode;
    let saver_format = config.storage_saver_ext;
    let extension = if storage_saver {
        saver_format.extension()
    } else {
        "wav"
    };
    let output_name = format!(
        "EAS_Recording_{}_{}_{}.{}",
        timestamp,
        sanitize_filename_label(event_code),
        source_marker,
        extension
    );
    let output_path = config.recording_dir.join(output_name);
    let ffmpeg_output_path = if storage_saver {
        let mut partial = output_path.as_os_str().to_owned();
        partial.push(".partial");
        PathBuf::from(partial)
    } else {
        output_path.clone()
    };

    let mut ffmpeg = Command::new(crate::components::ffmpeg());
    ffmpeg
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y");
    for segment in segments {
        ffmpeg.arg("-i").arg(segment);
    }
    ffmpeg
        .arg("-filter_complex")
        .arg(concat_filter(segments.len()))
        .arg("-map")
        .arg("[outa]");

    if storage_saver {
        ffmpeg.args(saver_format.ffmpeg_codec_args());
    } else {
        ffmpeg.arg("-c:a").arg("pcm_s16le");
    }
    ffmpeg.arg(&ffmpeg_output_path);

    let status = ffmpeg.status().await?;
    if !status.success() {
        if storage_saver {
            let _ = fs::remove_file(&ffmpeg_output_path).await;
        }
        return Err(anyhow!(
            "ffmpeg failed to build the CAP recording (status {:?})",
            status.code()
        ));
    }

    if storage_saver {
        fs::rename(&ffmpeg_output_path, &output_path)
            .await
            .with_context(|| format!("Failed to finalize CAP recording at {:?}", output_path))?;
    }

    Ok(output_path)
}

async fn write_wav_i16(path: &Path, sample_rate: u32, samples: &[i16]) -> Result<()> {
    let path = path.to_path_buf();
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

    fn config_with_speechify_voice(voice_dir: &Path, model: Option<&str>) -> Config {
        let mut config = Config::safe_internal_defaults();
        config.tts_engine = "speechify".to_string();
        config.tts_model = model.map(|name| name.to_string());
        config.spfy_voice_dir = voice_dir.to_path_buf();
        config
    }

    fn install_speechify_voice(parent: &Path, name: &str, vdb_rate: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).expect("voice dir");
        std::fs::write(dir.join(format!("{name}.vin")), b"vin").expect("vin");
        std::fs::write(dir.join(format!("{name}{vdb_rate}.vdb")), b"vdb").expect("vdb");
        std::fs::write(dir.join(format!("{name}.vcf")), b"vcf").expect("vcf");
        dir
    }

    #[test]
    fn speechify_voice_defaults_to_the_configured_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let tom = install_speechify_voice(root.path(), "tom", "8");

        // No TTS_MODEL: SPFY_VOICE_DIR points straight at the voice, as it always has.
        let config = config_with_speechify_voice(&tom, None);
        let voice = speechify_voice(&config).expect("voice resolves");
        assert_eq!(voice.vin, tom.join("tom.vin"));
        assert_eq!(voice.vdb, tom.join("tom8.vdb"));
        assert_eq!(voice.vcf, tom.join("tom.vcf"));
    }

    #[test]
    fn speechify_voice_selects_a_named_voice_beside_or_inside_the_configured_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let tom = install_speechify_voice(root.path(), "tom", "8");
        let crstom = install_speechify_voice(root.path(), "crstom", "8");

        // SPFY_VOICE_DIR points at one voice, TTS_MODEL names a sibling.
        let config = config_with_speechify_voice(&tom, Some("crstom"));
        let voice = speechify_voice(&config).expect("sibling voice resolves");
        assert_eq!(voice.vin, crstom.join("crstom.vin"));
        assert_eq!(voice.vdb, crstom.join("crstom8.vdb"));
        assert_eq!(voice.vcf, crstom.join("crstom.vcf"));

        // SPFY_VOICE_DIR points at the voices folder, TTS_MODEL names one inside it.
        let config = config_with_speechify_voice(root.path(), Some("crstom"));
        let voice = speechify_voice(&config).expect("nested voice resolves");
        assert_eq!(voice.vin, crstom.join("crstom.vin"));
    }

    #[test]
    fn speechify_voice_accepts_an_absolute_path_and_falls_back_to_a_16k_vdb() {
        let root = tempfile::tempdir().expect("temp dir");
        let crsmara = install_speechify_voice(root.path(), "crsmara", "16");

        let config = config_with_speechify_voice(root.path(), Some(&crsmara.display().to_string()));
        let voice = speechify_voice(&config).expect("absolute path resolves");
        assert_eq!(voice.vdb, crsmara.join("crsmara16.vdb"));
    }

    #[test]
    fn speechify_voice_prefers_the_8k_vdb_when_both_are_present() {
        let root = tempfile::tempdir().expect("temp dir");
        let tom = install_speechify_voice(root.path(), "tom", "8");
        std::fs::write(tom.join("tom16.vdb"), b"vdb").expect("16k vdb");

        let config = config_with_speechify_voice(&tom, None);
        assert_eq!(
            speechify_voice(&config).expect("voice resolves").vdb,
            tom.join("tom8.vdb")
        );
    }

    fn replacements(entries: &[(&str, &str)]) -> TtsReplacements {
        TtsReplacements::new(
            entries
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )
        .expect("replacements")
    }

    /// The report that found this: TFT's all-caps prefix read as "A CIVIL AUTHORITY HASOUTH
    /// ISSUED...". These are the entries from the live dictionary that collided with it.
    #[test]
    fn replacements_leave_words_that_merely_contain_a_key_alone() {
        let table = replacements(&[
            ("S ", "South "),
            ("E ", "East "),
            ("N ", "North "),
            ("W ", "West "),
            ("S.", "South "),
            ("E.", "East "),
            ("N.", "North "),
            ("EAS", "E A S"),
            ("LEA", "Law Enforcement Agency"),
            ("EM", "Emergency Management"),
            ("LA", "Los Angeles"),
            ("IPAWSWEA", "i paws w e a."),
        ]);

        let prefix = "A CIVIL AUTHORITY HAS ISSUED A LOCAL AREA EMERGENCY FOR THE FOLLOWING \
                      COUNTIES/AREAS: COBB, GA; DOUGLAS, GA; FULTON, GA; AT 9:26 PM ON SEP 18, \
                      2026 EFFECTIVE UNTIL 9:26 PM SEP 19, 2026. PLEASE STAND BY. MESSAGE FROM \
                      IPAWSWEA.";
        let expected = prefix.replace("IPAWSWEA.", "i paws w e a..");
        assert_eq!(table.apply(prefix), expected);

        // The tokens those entries exist for still expand.
        assert_eq!(
            table.apply("5 MI S OF ATLANTA, CALL LEA OR EM. EAS"),
            "5 MI South OF ATLANTA, CALL Law Enforcement Agency OR Emergency Management. E A S"
        );
    }

    #[test]
    fn replacements_prefer_the_longest_key_and_never_rematch_their_own_output() {
        let table = replacements(&[
            ("S ", "South "),
            ("SW ", "Southwest "),
            ("SSW ", "South-Southwest "),
            ("EAS", "E A S"),
            ("E ", "East "),
            ("A ", "Alpha "),
        ]);
        assert_eq!(
            table.apply("10 MI SSW OF TOWN, 5 MI SW OF CITY, 2 MI S "),
            "10 MI South-Southwest OF TOWN, 5 MI Southwest OF CITY, 2 MI South "
        );
        // "E A S" is the output of one entry and the input of two others; it must stay put.
        assert_eq!(table.apply("THE EAS TEST"), "THE E A S TEST");
    }

    #[test]
    fn replacement_values_are_inserted_literally() {
        let table = replacements(&[
            ("Pottawattamie", r"\![.1pa.0tx.0wa.0tu.0mi]"),
            ("Mt.", "Mount"),
            ("fee", "$1 fee"),
        ]);
        assert_eq!(
            table.apply("Pottawattamie County near Mt. Hood, not Amt. fee"),
            r"\![.1pa.0tx.0wa.0tu.0mi] County near Mount Hood, not Amt. $1 fee"
        );
    }

    #[test]
    fn replacements_reach_prose_but_never_urls_or_hashtags() {
        // "ca", "#" and "S " all appear inside the URL and the hashtag; only prose may change.
        let table = replacements(&[
            ("ca", "c a"),
            ("#", "hashtag "),
            ("S ", "South "),
            ("info ", "information "),
            ("Mngt", "Management"),
        ]);
        let text = "More info from Emergency Mngt at weather.gc.ca/S or #QCStorm, 5 MI S of town.";
        let prose = table.apply_to_prose(text);
        assert_eq!(
            prose,
            "More information from Emergency Management at weather.gc.ca/S or #QCStorm, 5 MI South of town."
        );

        // Spelling-out and its control codes stay Speechify's alone; every other engine gets
        // plain words from the same dictionary.
        let plain = normalize_text_for_speech(&prose, false);
        assert!(!plain.contains(r"\!"), "{plain}");
        assert!(
            plain.contains("information from Emergency Management"),
            "{plain}"
        );
        assert!(plain.contains("5 MI South of town"), "{plain}");
    }

    /// Descriptions used to get the dictionary case-insensitively, which let "LA" rewrite every
    /// French "la" and "S " reach into "it's". Case now follows the key.
    #[test]
    fn replacement_case_follows_the_key() {
        let table = replacements(&[
            ("hwy ", "highway "),
            ("Hwy ", "Highway "),
            ("LA", "Los Angeles"),
            ("S ", "South "),
            ("US ", "U S Highway "),
        ]);
        assert_eq!(
            table.apply("HWY 75 and Hwy 6 and hwy 2"),
            "highway 75 and Highway 6 and highway 2"
        );
        assert_eq!(
            table.apply("la tornade près de LA, it's moving S at us "),
            "la tornade près de Los Angeles, it's moving South at us "
        );
        assert_eq!(table.apply("CLOSED US 75"), "CLOSED U S Highway 75");
    }

    #[test]
    fn a_broken_replacement_file_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(TtsReplacements::load(&dir.path().join("missing.json"), false).is_none());

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, r#"{ "S ": "South ", }"#).expect("write");
        assert!(TtsReplacements::load(&broken, false).is_none());

        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").expect("write");
        assert!(TtsReplacements::load(&empty, false).is_none());
    }

    #[test]
    fn every_cap_feed_is_recognised_by_its_header_and_nothing_else_is() {
        for header in [
            "ZCZC-WXR-TOR-031055+0030-1231645-IPAWSCAP-",
            "ZCZC-WXR-TOR-031055+0030-1231645-IPAWSWEA-",
            "ZCZC-WXR-TOR-043100-046100+0030-2612008-NAADSCAP-",
            // Whatever build_cap_raw_header emits must round-trip.
            &build_cap_raw_header(
                "WXR",
                "TOR",
                &["043100".to_string()],
                None,
                None,
                "naad://streaming1.naad-adna.pelmorex.com:8080#TEST",
            ),
        ] {
            assert!(is_cap_raw_header(header), "{header}");
        }

        for header in [
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35   -",
            "ZCZC-EAS-RWT-000000+0015-2610534-EASLSTNR-",
            // A sender that merely contains a marker is not one.
            "ZCZC-WXR-TOR-031055+0030-1231645-XIPAWSCAP-",
            "NNNN",
            "",
        ] {
            assert!(!is_cap_raw_header(header), "{header}");
        }
    }

    /// A CAP-CP-only setup used to hide the panel entirely: "enabled" meant IPAWS polling.
    #[tokio::test]
    async fn cap_status_describes_both_feeds() {
        let app_state = Arc::new(Mutex::new(AppState::new(Vec::new())));
        let mut config = Config::safe_internal_defaults();
        config.process_cap_alerts = false;
        config.process_capcp_alerts = true;
        config.capcp_stream_endpoints = vec!["streaming1.naad-adna.pelmorex.com:8080".to_string()];

        sync_cap_runtime_config_status(&app_state, &config).await;
        {
            let status = &app_state.lock().await.cap_status;
            assert!(status.enabled);
            assert!(!status.ipaws_enabled);
            assert!(status.capcp_enabled);
            assert_eq!(
                status.capcp_endpoints,
                vec!["streaming1.naad-adna.pelmorex.com:8080"]
            );
        }

        config.process_capcp_alerts = false;
        sync_cap_runtime_config_status(&app_state, &config).await;
        assert!(!app_state.lock().await.cap_status.enabled);
    }

    /// Recording names and header sender IDs both come from this, so it is what decides whether a
    /// CAP-CP recording is labelled NAAD. It used to be labelled IPAWS: the recording builder
    /// re-derived the marker from the header and only knew about WEA.
    #[test]
    fn source_marker_follows_the_feed_the_alert_came_from() {
        for (source, expected) in [
            (
                "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/feed",
                CAP_HEADER_SOURCE_MARKER_CAP,
            ),
            (
                "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/eas/recent/2019-12-31T11:59:59Z",
                CAP_HEADER_SOURCE_MARKER_CAP,
            ),
            (
                "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/PublicWEA/recent/2012-08-21T11:40:43Z",
                CAP_HEADER_SOURCE_MARKER_WEA,
            ),
            (
                "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
                CAP_HEADER_SOURCE_MARKER_NAAD,
            ),
            (
                "http://capcp1.naad-adna.pelmorex.com/2026-09-18/2026_09_18T12_00_00Z/TEST.xml",
                CAP_HEADER_SOURCE_MARKER_NAAD,
            ),
        ] {
            assert_eq!(cap_header_source_marker(source), expected, "{source}");
        }
    }

    #[test]
    fn alert_ready_framing_needs_both_the_option_and_a_naad_source() {
        let mut alert = parse_cap_alert(
            include_str!("../tests/fixtures/cap_alert_valid.xml"),
            "https://alerts.example/valid",
        )
        .expect("alert");
        let mut config = Config::safe_internal_defaults();

        // Live NAAD documents and ones recovered from the archive both count as CAP-CP.
        for naad_source in [
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
            "http://capcp1.naad-adna.pelmorex.com/2026-09-18/2026_09_18T12_00_00Z/TEST.xml",
        ] {
            alert.source_url = naad_source.to_string();
            config.capcp_use_alert_ready_tone = false;
            assert_eq!(recording_framing(&config, &alert), RecordingFraming::Same);
            config.capcp_use_alert_ready_tone = true;
            assert_eq!(
                recording_framing(&config, &alert),
                RecordingFraming::AlertReady,
                "{naad_source}"
            );
        }

        // The option is CAP-CP's alone: an IPAWS alert keeps its SAME framing regardless.
        alert.source_url = "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/feed".to_string();
        assert_eq!(recording_framing(&config, &alert), RecordingFraming::Same);
    }

    #[test]
    fn header_tones_switched_off_outrank_every_other_framing() {
        let mut alert = parse_cap_alert(
            include_str!("../tests/fixtures/cap_alert_valid.xml"),
            "https://alerts.example/valid",
        )
        .expect("alert");
        let mut config = Config::safe_internal_defaults();
        config.emit_header_tones = false;
        config.capcp_use_alert_ready_tone = true;

        for source in [
            "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/feed",
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
        ] {
            alert.source_url = source.to_string();
            assert_eq!(
                recording_framing(&config, &alert),
                RecordingFraming::Bare,
                "{source}"
            );
        }

        // Nothing opens the recording, so custom header audio is not consulted either.
        config.custom_header_audio = std::env::current_exe().expect("a real file");
        assert_eq!(config.header_audio_override(), None);
    }

    #[test]
    fn capcp_header_audio_opens_naad_recordings_only() {
        let mut config = Config::safe_internal_defaults();
        let canadian = std::env::current_exe().expect("a real file");
        config.capcp_custom_header_audio = canadian.clone();

        assert_eq!(
            header_audio_for(&config, CAP_HEADER_SOURCE_MARKER_NAAD),
            Some(canadian)
        );
        for marker in [CAP_HEADER_SOURCE_MARKER_CAP, CAP_HEADER_SOURCE_MARKER_WEA] {
            assert_eq!(header_audio_for(&config, marker), None, "{marker}");
        }
    }

    #[test]
    fn the_compiled_in_alert_ready_tone_is_the_real_signal() {
        let reader = hound::WavReader::new(std::io::Cursor::new(ALERT_READY_TONE_WAV))
            .expect("pelmorex.wav parses as a WAV");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.bits_per_sample, 16);

        let seconds = reader.duration() as f64 / spec.sample_rate as f64;
        assert!((7.5..=8.5).contains(&seconds), "{seconds}s");
    }

    #[test]
    fn concat_filter_labels_every_segment() {
        // Byte-for-byte what the SAME framing used to hard-code.
        assert_eq!(
            concat_filter(6),
            "[0:a][1:a][2:a][3:a][4:a][5:a]concat=n=6:v=0:a=1[outa]"
        );
        assert_eq!(concat_filter(3), "[0:a][1:a][2:a]concat=n=3:v=0:a=1[outa]");
    }

    #[test]
    fn test_alert_script_names_the_engine_it_is_testing() {
        for (engine, spoken) in [
            ("piper", "Piper"),
            ("espeak-ng", "e Speak"),
            ("speechify", "Speechify"),
            ("cepstral", "Cepstral"),
            ("loquendo", "Loquendo"),
        ] {
            let mut config = Config::safe_internal_defaults();
            config.tts_engine = engine.to_string();
            let script = test_alert_tts_script(&config);
            assert!(
                script.contains(&format!("using the {spoken} engine")),
                "{engine}: {script}"
            );
            // Short enough to be one passage, so a pass is not an artefact of the chunking.
            assert!(script.chars().count() < CAP_TTS_MAX_CHUNK_CHARS, "{script}");
        }
    }

    #[test]
    fn only_an_engine_that_cannot_start_skips_the_retry_path() {
        let missing = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("Failed to execute Loquendo (loqdave) TTS command");
        assert!(tts_engine_could_not_start(&missing));

        let denied =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(tts_engine_could_not_start(&denied));

        // An engine that ran and failed may well succeed on less text.
        let exited = anyhow!("Speechify (spfy_synth) failed with status Some(1): heap OOM");
        assert!(!tts_engine_could_not_start(&exited));
    }

    #[tokio::test]
    async fn a_missing_voice_fails_before_any_engine_runs() {
        let root = tempfile::tempdir().expect("temp dir");
        let config = config_with_speechify_voice(&root.path().join("tom"), Some("crstom"));

        let err = synthesize_tts_samples(&config, "Hello there.", "test", "RWT")
            .await
            .expect_err("no voice is installed");
        assert!(
            err.to_string()
                .contains("Could not resolve a Speechify voice"),
            "{err}"
        );
    }

    #[test]
    fn speechify_voice_error_names_every_place_it_looked() {
        let root = tempfile::tempdir().expect("temp dir");
        let tom = install_speechify_voice(root.path(), "tom", "8");
        // A directory that exists but whose files carry the wrong stem is the likely mistake.
        let jill = root.path().join("jill");
        std::fs::create_dir_all(&jill).expect("voice dir");
        std::fs::write(jill.join("tom.vin"), b"vin").expect("misnamed vin");

        let config = config_with_speechify_voice(&tom, Some("jill"));
        let err = speechify_voice(&config)
            .expect_err("voice is not installed")
            .to_string();
        assert!(err.contains("TTS_MODEL 'jill'"), "{err}");
        assert!(err.contains("jill.vin"), "{err}");
        assert!(err.contains("jill8.vdb"), "{err}");
        assert!(err.contains(&jill.display().to_string()), "{err}");
        // Both the nested and the sibling candidate are reported.
        assert!(
            err.contains(&tom.join("jill").display().to_string()),
            "{err}"
        );
    }

    fn config_with_cepstral_voice(voice_root: &Path, model: Option<&str>) -> Config {
        let mut config = Config::safe_internal_defaults();
        config.tts_engine = "cepstral".to_string();
        config.tts_model = model.map(|name| name.to_string());
        config.cep6_voice_dir = voice_root.to_path_buf();
        config.shared_state_dir = voice_root.join("state");
        config
    }

    #[test]
    fn a_cepstral_voice_left_in_the_old_state_folder_is_still_found() {
        let root = tempfile::tempdir().expect("temp dir");
        let config = config_with_cepstral_voice(&root.path().join("shared"), Some("David"));
        let old = config
            .shared_state_dir
            .join("tts_voices")
            .join("cep6")
            .join("David");
        std::fs::create_dir_all(&old).expect("voice dir");
        std::fs::write(old.join("voice.idx"), b"index").expect("voice index");
        assert_eq!(cepstral_voice_dir(&config).expect("voice resolves"), old);
    }

    #[test]
    fn loqdave_banner_is_stripped_but_real_errors_survive() {
        let stderr = concat!(
            "Copyright (C) 2006 - Loquendo SpA.\n",
            "LoquendoTTS (LTTS v.6.6 Build 20071113) - Sep  5 2013 16:14:42\n",
            "Multilingual Text-To-Speech Synthesis System.\n",
            "Speaker = Dave (American English male voice)\n",
            "Speech Format = 16KHz loqmsx, Audio Format = 16KHz l\n",
            "Audio destination library = LoqAudioFile\n",
            "End your sentence with one of the following punctuation marks: \".;:!?\"\n",
            "Press Ctrl-D to exit.\n",
            "> out of guest memory\n",
            "loqdave: the engine driver returned 0xe006000a\n",
        );

        assert_eq!(
            strip_loqdave_banner(stderr),
            "out of guest memory; loqdave: the engine driver returned 0xe006000a"
        );
    }

    #[test]
    fn loqdave_input_is_one_latin1_byte_per_character() {
        assert_eq!(to_latin1("TORNADO WARNING"), b"TORNADO WARNING");
        // French CAP-CP text survives as Latin-1 rather than as two UTF-8 bytes per accent.
        assert_eq!(to_latin1("Qu\u{e9}bec"), b"Qu\xe9bec");
        assert_eq!(
            to_latin1("Take cover \u{2014} it\u{2019}s \u{201c}now\u{201d}\u{2026}"),
            b"Take cover - it's \"now\"..."
        );
        assert_eq!(to_latin1("\u{26a0} Alert"), b"  Alert");
    }

    #[test]
    fn loqdave_banner_alone_leaves_nothing() {
        let stderr = "Copyright (C) 2006 - Loquendo SpA.\nPress Ctrl-D to exit.\n> ";
        assert!(strip_loqdave_banner(stderr).is_empty());
    }

    #[test]
    fn cepstral_voice_dir_defaults_to_allison_and_needs_the_voice_installed() {
        let root = tempfile::tempdir().expect("temp dir");
        let config = config_with_cepstral_voice(root.path(), None);

        let missing = cepstral_voice_dir(&config).expect_err("voice is not installed");
        assert!(missing.to_string().contains("Allison"));
        assert!(missing.to_string().contains("fetch_voices.sh"));

        let installed = root.path().join("Allison");
        std::fs::create_dir_all(&installed).expect("voice dir");
        std::fs::write(installed.join("voice.idx"), b"index").expect("voice index");

        assert_eq!(
            cepstral_voice_dir(&config).expect("voice resolves"),
            installed
        );
    }

    #[test]
    fn cepstral_voice_dir_rejects_a_path_instead_of_a_voice_name() {
        let root = tempfile::tempdir().expect("temp dir");

        for model in ["../../etc", "sub/Allison", ".."] {
            let config = config_with_cepstral_voice(root.path(), Some(model));
            let err = cepstral_voice_dir(&config).expect_err("path is not a voice name");
            assert!(
                err.to_string().contains("is not a Cepstral voice name"),
                "unexpected error for {model}: {err}"
            );
        }
    }

    fn sample_alert_data(event_code: &str, fips: &[&str]) -> EasAlertData {
        EasAlertData {
            eas_text: "sample text".to_string(),
            event_text: "Sample Event".to_string(),
            event_code: event_code.to_string(),
            fips: fips.iter().map(|value| value.to_string()).collect(),
            locations: "Sample Location".to_string(),
            originator: "WXR".to_string(),
            description: None,
            instructions: None,
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
        second.expires = second
            .expires
            .map(|value| value + ChronoDuration::minutes(30));

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
        let dedupe_key =
            build_dedupe_key_from_raw_header("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-")
                .expect("dedupe key");
        let eas_alert = ActiveAlert::new(
            sample_alert_data("TOR", &["031055"]),
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-".to_string(),
            Duration::from_secs(120),
        );

        assert!(active_alert_has_dedupe_key(
            &[eas_alert],
            dedupe_key.as_str()
        ));
    }

    #[test]
    fn backfill_fills_only_missing_cap_details_on_matching_alerts() {
        let raw_header = "ZCZC-CIV-FRW-030039+2355-2421717-IPAWSCAP-";
        let dedupe_key = build_dedupe_key_from_raw_header(raw_header).expect("dedupe key");

        let mut restored = ActiveAlert::new(
            sample_alert_data("FRW", &["030039"]),
            raw_header.to_string(),
            Duration::from_secs(3600),
        );
        restored.data.description = Some("Already present.".to_string());

        let other = ActiveAlert::new(
            sample_alert_data("TOR", &["031055"]),
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-".to_string(),
            Duration::from_secs(3600),
        );

        let mut alerts = vec![restored, other];
        assert!(backfill_alert_cap_details(
            &mut alerts,
            dedupe_key.as_str(),
            Some("Replacement description."),
            Some("Residents should remain prepared to evacuate."),
        ));

        assert_eq!(
            alerts[0].data.description.as_deref(),
            Some("Already present.")
        );
        assert_eq!(
            alerts[0].data.instructions.as_deref(),
            Some("Residents should remain prepared to evacuate.")
        );
        assert!(alerts[1].data.instructions.is_none());

        assert!(!backfill_alert_cap_details(
            &mut alerts,
            dedupe_key.as_str(),
            Some("Replacement description."),
            Some("Residents should remain prepared to evacuate."),
        ));
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

    fn spelled(label: &str) -> String {
        format!("{URL_SPELL_MODE_ON} {label} {URL_SPELL_MODE_OFF}")
    }

    #[test]
    fn normalize_urls_spells_bare_domain() {
        assert_eq!(
            normalize_urls_in_description("See https://TxDOTAlerts.com for current alerts.", true),
            format!("See {} dot com for current alerts.", spelled("txdotalerts"))
        );
    }

    #[test]
    fn normalize_urls_keeps_www_prefix_spoken() {
        assert_eq!(
            normalize_urls_in_description(
                "Visit our website at www.example.com for more information.",
                true
            ),
            format!(
                "Visit our website at www dot {} dot com for more information.",
                spelled("example")
            )
        );
    }

    #[test]
    fn normalize_urls_drops_query_and_fragment_and_speaks_path() {
        assert_eq!(
            normalize_urls_in_description(
                "Details at https://alerts.weather.gov/cap/us.php?x=1#top now.",
                true
            ),
            format!(
                "Details at {} dot {} dot gov slash cap slash us dot php now.",
                spelled("alerts"),
                spelled("weather")
            )
        );
    }

    #[test]
    fn normalize_urls_handles_multiple_urls_and_two_label_suffix() {
        assert_eq!(
            normalize_urls_in_description(
                "Check ready.gov and http://www.example.co.uk/help today.",
                true
            ),
            format!(
                "Check {} dot gov and www dot {} dot co dot {} slash help today.",
                spelled("ready"),
                spelled("example"),
                spelled("uk")
            )
        );
    }

    #[test]
    fn normalize_urls_spells_every_subdomain_but_not_www() {
        assert_eq!(
            normalize_urls_in_description("Go to https://www.alerts.nws.example.com now.", true),
            format!(
                "Go to www dot {} dot {} dot {} dot com now.",
                spelled("alerts"),
                spelled("nws"),
                spelled("example")
            )
        );
    }

    #[test]
    fn normalize_urls_without_spell_tags_for_other_engines() {
        assert_eq!(
            normalize_urls_in_description("See https://TxDOTAlerts.com for current alerts.", false),
            "See txdotalerts dot com for current alerts."
        );
    }

    #[test]
    fn normalize_urls_leaves_prose_and_emails_alone() {
        let prose = "Move to shelter.Motorists should use caution. Winds of 1.5 inches. Email info@example.com now.";
        assert_eq!(normalize_urls_in_description(prose, true), prose);
    }

    /// Mirrors the halving loop in `synthesize_tts_samples` against a fake engine that refuses
    /// anything over `ceiling` characters, which is how Speechify's guest heap behaves.
    fn simulate_adaptive_split(text: &str, ceiling: usize) -> (Vec<String>, Vec<String>) {
        let mut pending: std::collections::VecDeque<String> =
            split_tts_text(text, CAP_TTS_MAX_CHUNK_CHARS)
                .into_iter()
                .collect();
        let mut synthesized = Vec::new();
        let mut dropped = Vec::new();
        let mut attempts = 0usize;
        let mut size_limit = CAP_TTS_MAX_CHUNK_CHARS;

        while let Some(piece) = pending.pop_front() {
            attempts += 1;
            assert!(
                attempts <= CAP_TTS_MAX_SYNTH_ATTEMPTS,
                "halving failed to converge"
            );

            let chars = piece.chars().count();
            if chars <= ceiling {
                synthesized.push(piece);
                continue;
            }
            if chars <= CAP_TTS_MIN_CHUNK_CHARS {
                dropped.push(piece);
                continue;
            }

            let next_limit = chars.div_ceil(2).max(CAP_TTS_MIN_CHUNK_CHARS);
            if next_limit < size_limit {
                size_limit = next_limit;
                let queued: Vec<String> = pending.drain(..).collect();
                pending = queued
                    .iter()
                    .flat_map(|queued_piece| split_tts_text(queued_piece, size_limit))
                    .collect();
            }

            let halves = split_tts_text(&piece, size_limit);
            if halves.len() < 2 {
                dropped.push(piece);
                continue;
            }
            for half in halves.into_iter().rev() {
                pending.push_front(half);
            }
        }

        (synthesized, dropped)
    }

    #[test]
    fn adaptive_split_converges_below_a_content_dependent_ceiling() {
        let sentence = "A special weather statement is in effect for Colville Lake. ";
        let text = sentence.repeat(120);

        // The reported failure: 2,794 characters came back empty, well under the old 3,000
        // starting chunk size. A ceiling that tight must still converge.
        for ceiling in [2_794usize, 1_200, 700, 400, 250] {
            let (synthesized, dropped) = simulate_adaptive_split(&text, ceiling);

            assert!(dropped.is_empty(), "ceiling {ceiling} dropped passages");
            assert!(
                !synthesized.is_empty(),
                "ceiling {ceiling} produced nothing"
            );
            for piece in &synthesized {
                assert!(
                    piece.chars().count() <= ceiling,
                    "ceiling {ceiling} kept a {}-character passage",
                    piece.chars().count()
                );
            }

            let original: Vec<&str> = text.split_whitespace().collect();
            let rejoined = synthesized.join(" ");
            let round_tripped: Vec<&str> = rejoined.split_whitespace().collect();
            assert_eq!(original, round_tripped, "ceiling {ceiling} lost words");
        }
    }

    #[test]
    fn adaptive_split_gives_up_below_the_floor_instead_of_spinning() {
        // An engine that fails on everything cannot be satisfied by halving, so the loop has to
        // terminate at the floor rather than recurse forever.
        let text = "Short sentence here. ".repeat(40);
        let (synthesized, dropped) = simulate_adaptive_split(&text, 0);

        assert!(synthesized.is_empty());
        assert!(!dropped.is_empty());
        for piece in &dropped {
            assert!(piece.chars().count() <= CAP_TTS_MIN_CHUNK_CHARS);
        }
    }

    #[test]
    fn hashtags_are_spoken_and_spelled_like_urls() {
        let spoken = normalize_hashtags_for_speech("Follow #QCStorm for updates.", true);
        assert_eq!(
            spoken,
            format!("Follow hashtag {URL_SPELL_MODE_ON} QCStorm {URL_SPELL_MODE_OFF} for updates.")
        );
    }

    #[test]
    fn hashtags_spell_out_letters_without_control_tags() {
        assert_eq!(
            normalize_hashtags_for_speech("Follow #QCStorm now.", false),
            "Follow hashtag q c s t o r m now."
        );
    }

    #[test]
    fn hashtags_at_the_start_and_in_sequence_are_handled() {
        assert_eq!(
            normalize_hashtags_for_speech("#ONStorm and #QCStorm", false),
            "hashtag o n s t o r m and hashtag q c s t o r m"
        );
    }

    #[test]
    fn a_bare_hash_is_left_alone() {
        // Not a tag: no word after it, or attached to a preceding token.
        assert_eq!(
            normalize_hashtags_for_speech("Call # now", false),
            "Call # now"
        );
        assert_eq!(
            normalize_hashtags_for_speech("item#4 shipped", false),
            "item#4 shipped"
        );
        assert_eq!(
            normalize_hashtags_for_speech("no hashes here", false),
            "no hashes here"
        );
    }

    #[test]
    fn url_fragments_are_not_mistaken_for_hashtags() {
        // The URL pass runs first and drops the fragment, so nothing is left for the hashtag
        // pass to misread.
        let spoken = normalize_text_for_speech(
            "See https://weather.gc.ca/warnings/index_e.html#current for details.",
            false,
        );
        assert!(
            !spoken.contains("hashtag"),
            "a URL fragment was read as a hashtag: {spoken}"
        );
        assert!(!spoken.contains('#'), "an unspoken # survived: {spoken}");
    }

    #[test]
    fn two_letter_country_code_tlds_are_spelled_not_spoken() {
        // ".ca" spoken as a word comes out "circa", so it has to be spelled.
        let spoken = normalize_urls_in_description("Visit weather.gc.ca today.", true);
        let tail = format!("dot {URL_SPELL_MODE_ON} ca {URL_SPELL_MODE_OFF}");
        assert!(
            spoken.contains(&tail),
            "the .ca TLD was not spelled: {spoken}"
        );
    }

    #[test]
    fn multi_letter_tlds_are_still_spoken_as_words() {
        let spoken = normalize_urls_in_description("See https://example.com now.", true);
        assert!(spoken.contains("dot com"), "{spoken}");
        assert!(
            !spoken.contains(&format!("{URL_SPELL_MODE_ON} com")),
            "the .com TLD should not be spelled: {spoken}"
        );
    }

    #[test]
    fn the_call_sign_comes_out_of_every_endec_mode_s_sentence() {
        let header = "ZCZC-WXR-TOR-031055+0030-2621515-KWO35-";
        assert_eq!(raw_header_sender(header), "KWO35");

        for mode in crate::e2t_ng::known_endec_modes() {
            if mode == "ALL" {
                continue;
            }
            let text = crate::e2t_ng::E2T(header, &mode, false, Some("America/Chicago"));
            let stripped = strip_sender_clause(&text, "KWO35");
            assert!(
                !stripped.to_ascii_uppercase().contains("KWO35"),
                "{mode}: {stripped}"
            );
            assert!(
                !stripped.to_ascii_lowercase().contains("message from"),
                "{mode}: {stripped}"
            );
            if text.contains("KWO35") {
                assert!(stripped.ends_with('.'), "{mode}: {stripped}");
            } else {
                assert_eq!(stripped, text, "{mode} has no sender to strip");
            }
        }

        assert_eq!(
            strip_sender_clause("Nothing to see here", "KWO35"),
            "Nothing to see here"
        );
    }

    #[test]
    fn the_built_in_dictionary_applies_and_a_local_file_overrides_it() {
        assert!(BUILTIN_TTS_REPLACEMENTS.len() > 100);

        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("cap_tts_replacement_config.json");
        let builtin = TtsReplacements::load(&missing, true).expect("built-in dictionary");
        assert_eq!(builtin.apply("Call 911 now"), "Call nine one one now");
        assert!(TtsReplacements::load(&missing, false).is_none());

        std::fs::write(&missing, r#"{ "911": "nine eleven" }"#).expect("write");
        let merged = TtsReplacements::load(&missing, true).expect("merged dictionary");
        assert_eq!(
            merged.apply("Call 911 in Pottawattamie"),
            "Call nine eleven in Pot-a-wat-a-mee"
        );
    }

    #[test]
    fn normalize_text_for_speech_handles_urls_and_hashtags_together() {
        let spoken = normalize_text_for_speech("Updates at weather.gc.ca and on #QCStorm.", false);
        assert!(spoken.contains("hashtag q c s t o r m"), "{spoken}");
        assert!(spoken.contains("dot ca"), "{spoken}");
    }

    #[tokio::test]
    async fn wav_round_trip_preserves_samples_and_rate() {
        // The chunk joiner reads each engine WAV back and rewrites one file, so these two have
        // to agree. Speechify emits 8 kHz mono 16-bit.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("round_trip.wav");
        let samples: Vec<i16> = (0..2_000).map(|n| ((n % 512) as i16) - 256).collect();

        write_wav_i16(&path, 8_000, &samples).await.expect("write");
        let (rate, read_back) = read_wav_i16(&path).await.expect("read");

        assert_eq!(rate, 8_000);
        assert_eq!(read_back, samples);
    }

    #[tokio::test]
    async fn wav_join_concatenates_chunks_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first: Vec<i16> = (0..500).map(|n| n as i16).collect();
        let second: Vec<i16> = (500..900).map(|n| n as i16).collect();

        let first_path = dir.path().join("a.wav");
        let second_path = dir.path().join("b.wav");
        write_wav_i16(&first_path, 8_000, &first)
            .await
            .expect("write a");
        write_wav_i16(&second_path, 8_000, &second)
            .await
            .expect("write b");

        let mut combined = Vec::new();
        for path in [&first_path, &second_path] {
            let (rate, samples) = read_wav_i16(path).await.expect("read chunk");
            assert_eq!(rate, 8_000);
            combined.extend_from_slice(&samples);
        }

        let joined_path = dir.path().join("joined.wav");
        write_wav_i16(&joined_path, 8_000, &combined)
            .await
            .expect("write joined");
        let (_, joined) = read_wav_i16(&joined_path).await.expect("read joined");

        assert_eq!(joined.len(), first.len() + second.len());
        assert_eq!(&joined[..first.len()], &first[..]);
        assert_eq!(&joined[first.len()..], &second[..]);
    }

    #[test]
    fn split_tts_text_leaves_short_text_alone() {
        let text = "A tornado warning is in effect. Take shelter now.";
        assert_eq!(split_tts_text(text, 3_000), vec![text.to_string()]);
        assert!(split_tts_text("   ", 3_000).is_empty());
        assert!(split_tts_text("anything", 0).is_empty());
    }

    #[test]
    fn split_tts_text_breaks_on_sentence_boundaries() {
        let sentence = "A special weather statement is in effect for Colville Lake. ";
        let text = sentence.repeat(40);
        let chunks = split_tts_text(&text, 300);

        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= 300,
                "chunk of {} chars exceeded the limit",
                chunk.chars().count()
            );
            // Sentence-aligned: a chunk starts a sentence and ends one.
            assert!(
                chunk.starts_with('A'),
                "chunk started mid-sentence: {chunk:?}"
            );
            assert!(chunk.ends_with('.'), "chunk ended mid-sentence: {chunk:?}");
        }
    }

    #[test]
    fn split_tts_text_preserves_every_word() {
        let sentence = "Environment Canada has issued a warning for the region. ";
        let text = sentence.repeat(60);
        let chunks = split_tts_text(&text, 250);

        let rejoined = chunks.join(" ");
        let original: Vec<&str> = text.split_whitespace().collect();
        let round_tripped: Vec<&str> = rejoined.split_whitespace().collect();
        assert_eq!(original, round_tripped, "chunking lost or reordered words");
    }

    #[test]
    fn split_tts_text_falls_back_to_word_then_character_boundaries() {
        // One sentence longer than the limit has to break on words.
        let long_sentence = format!("{} end.", "word ".repeat(200));
        for chunk in split_tts_text(&long_sentence, 100) {
            assert!(chunk.chars().count() <= 100);
        }

        // A single unbroken token longer than the limit has to break on characters.
        let giant_word = "x".repeat(500);
        let chunks = split_tts_text(&giant_word, 100);
        assert_eq!(chunks.len(), 5);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 100);
        }
        assert_eq!(chunks.concat(), giant_word);
    }

    #[test]
    fn split_tts_text_never_splits_inside_a_character() {
        // Multi-byte characters must survive the character-boundary fallback intact.
        let text = "é".repeat(250);
        let chunks = split_tts_text(&text, 100);
        assert_eq!(chunks.concat(), text);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 100);
            assert!(chunk.chars().all(|ch| ch == 'é'));
        }
    }

    #[test]
    fn split_tts_text_stays_under_the_speechify_heap_ceiling() {
        let sentence = "A special weather statement is in effect for Colville Lake. ";
        let text = sentence.repeat(400);
        for chunk in split_tts_text(&text, CAP_TTS_MAX_CHUNK_CHARS) {
            assert!(chunk.chars().count() <= CAP_TTS_MAX_CHUNK_CHARS);
        }
    }

    #[test]
    fn deduplicate_instructions_removes_repeated_sentences() {
        let description = "National Weather Service: TORNADO WARNING in this area until 7:15 PM EDT. Take shelter now in a basement or an interior room on the lowest floor of a sturdy building. If you are outdoors, in a mobile home, or in a vehicle, move to the closest substantial shelter and protect yourself from flying debris. Check media.";
        let instructions = "TAKE COVER NOW! Move to a basement or an interior room on the lowest floor of a sturdy building. Avoid windows. If you are outdoors, in a mobile home, or in a vehicle, move to the closest substantial shelter and protect yourself from flying debris.";
        let result = deduplicate_instructions(description, instructions);
        assert_eq!(
            result,
            "TAKE COVER NOW! Move to a basement or an interior room on the lowest floor of a sturdy building. Avoid windows."
        );
    }

    #[test]
    fn deduplicate_instructions_keeps_all_when_no_overlap() {
        let description = "A tornado warning has been issued.";
        let instructions = "Seek shelter immediately. Stay away from windows.";
        let result = deduplicate_instructions(description, instructions);
        assert_eq!(result, "Seek shelter immediately. Stay away from windows.");
    }

    #[test]
    fn deduplicate_instructions_empty_when_fully_duplicated() {
        let description = "Take cover now. Move to shelter.";
        let instructions = "Take cover now. Move to shelter.";
        let result = deduplicate_instructions(description, instructions);
        assert_eq!(result, "");
    }
}
