use crate::alerts::update_alert_files;
use crate::cap::{
    archive_cap_alert, backfill_persisted_cap_details, build_cap_raw_header, build_dedupe_key,
    cap_alert_is_active, child_text, extract_parameter_value, extract_same_from_container,
    is_audio_resource, load_persisted_active_dedupe_keys, normalize_event_code,
    normalize_originator_code, parse_cap_time, process_cap_alert, sanitize_cap_description,
    simple_sanitize_description, split_fips_codes, CAP_HTTP_TIMEOUT_SECS,
    CAP_SEEN_DEFAULT_TTL_SECS,
};
use crate::cap::{CapAlert, DEFAULT_NO_DESCRIPTION};
use crate::config::Config;
use crate::db::DbHandle;
use crate::monitoring::MonitoringHub;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use phf::phf_map;
use roxmltree::{Document, Node};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tracing::{debug, info, warn};

const NAAD_STREAM_ENDPOINTS: &[&str] = &[
    "streaming1.naad-adna.pelmorex.com:8080",
    "streaming2.naad-adna.pelmorex.com:8080",
];
const NAAD_ARCHIVE_HOSTS: &[&str] = &[
    "capcp1.naad-adna.pelmorex.com",
    "capcp2.naad-adna.pelmorex.com",
];
const NAAD_DEFAULT_LANGUAGES: &[&str] = &["en"];
const NAAD_HEARTBEAT_SENDER: &str = "NAADS-Heartbeat";
const NAAD_CONNECT_TIMEOUT_SECS: u64 = 20;
const NAAD_READ_TIMEOUT_SECS: u64 = 120;
const NAAD_RECONNECT_DELAY_SECS: u64 = 5;
const NAAD_RECONNECT_MAX_DELAY_SECS: u64 = 120;
const NAAD_PRODUCTIVE_CONNECTION_SECS: u64 = 60;
const NAAD_FAILURE_STREAK_WARN: u32 = 4;
const NAAD_STARTUP_STAGGER_MILLIS: u64 = 500;
const NAAD_READ_CHUNK_BYTES: usize = 16 * 1024;
const NAAD_BUFFER_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const NAAD_ARCHIVE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Both archive hosts are asked at once, so this bounds a slow one rather than adding up.
const NAAD_ARCHIVE_TIMEOUT_SECS: u64 = 5;
/// How long a handled NAAD document is remembered. Heartbeats list the last ten alerts until ten
/// newer ones push them out, which on a quiet day takes days.
const NAAD_DOCUMENT_SEEN_DAYS: i64 = 30;
/// Heartbeats waiting for the recovery worker. Both streams send each one, and whatever is dropped
/// here is listed again by the next heartbeat a minute later.
const NAAD_RECOVERY_QUEUE_DEPTH: usize = 4;
const CAPCP_PROFILE_CODE_PREFIX: &str = "profile:CAP-CP:";
const CAPCP_EVENT_VALUE_PREFIX: &str = "profile:CAP-CP:Event:";
const CAPCP_LOCATION_VALUE_PREFIX: &str = "profile:CAP-CP:Location:";
// Observed on live NAAD traffic:
//   layer:SOREM:1.0:Broadcast_Immediately   Yes/No
//   layer:SOREM:2.0:WirelessImmediate       Yes/No
// Note the differing layer versions and punctuation -- the two flags did not move in step.
// Matching is done on a substring of the valueName stripped of punctuation and case so a future
// version bump, or "_Immediate"/"Immediately" drift, cannot silently stop matching and start
// passing every alert.
const SOREM_BROADCAST_IMMEDIATE_KEY: &str = "broadcastimmediate";
const SOREM_WIRELESS_IMMEDIATE_KEY: &str = "wirelessimmediate";
const SOREM_BROADCAST_TEXT: &str = "layer:SOREM:1.0:Broadcast_Text";
const EC_NEWLY_ACTIVE_AREAS: &str = "layer:EC-MSC-SMC:1.1:Newly_Active_Areas";
const CAPCP_FALLBACK_EVENT_CODE: &str = "CEM";
const CAPCP_FALLBACK_ORIGINATOR_CODE: &str = "CIV";

#[derive(Debug, Deserialize)]
struct SameCaResource {
    #[serde(rename = "SAME")]
    same: HashMap<String, String>,
    #[serde(rename = "ORGS")]
    orgs: HashMap<String, String>,
    #[serde(rename = "EVENTS")]
    events: HashMap<String, String>,
}

static SAME_CA: Lazy<SameCaResource> = Lazy::new(|| {
    serde_json::from_str(include_str!("../include/same-ca.json")).expect("parse same-ca.json")
});

// Broadest-to-narrowest ordering of the CSV's scale column. Several census subdivisions share
// one CLC forecast zone, so the broadest row for a code is the one most likely to name the zone
// itself rather than a town inside it.
const CLC_SCALE_ORDER: &[&str] = &["N", "PT", "WB", "CD", "WBD", "CSD"];

// Names for the 104 CLC codes that GeoToCLC.csv references but same-ca.json has no entry for --
// mostly marine zones and northern subdivisions. Without this, E2T renders them as the raw
// "FIPS Code 094720" instead of a place name.
static CLC_NAMES: Lazy<HashMap<String, String>> = Lazy::new(|| {
    let mut best: HashMap<String, (usize, String)> = HashMap::new();

    for line in include_str!("../include/GeoToCLC.csv").lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 4 {
            continue;
        }

        let scale = fields[2].trim();
        let name = fields[3].trim();
        if name.is_empty() {
            continue;
        }

        let rank = CLC_SCALE_ORDER
            .iter()
            .position(|entry| *entry == scale)
            .unwrap_or(CLC_SCALE_ORDER.len());

        for code in fields[1].split('-').map(str::trim) {
            if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }

            match best.get(code) {
                Some((existing, _)) if *existing <= rank => {}
                _ => {
                    best.insert(code.to_string(), (rank, name.to_string()));
                }
            }
        }
    }

    best.into_iter()
        .map(|(code, (_, name))| (code, name))
        .collect()
});

pub(crate) fn clc_location_name(code: &str) -> Option<&'static str> {
    CLC_NAMES.get(code.trim()).map(String::as_str)
}

static GEO_TO_CLC: Lazy<HashMap<String, Vec<String>>> = Lazy::new(|| {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for line in include_str!("../include/GeoToCLC.csv").lines().skip(1) {
        let mut fields = line.split(',');
        let geocode = fields.next().unwrap_or_default().trim();
        let clc = fields.next().unwrap_or_default().trim();

        if geocode.is_empty() || !geocode.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }

        let codes: Vec<String> = clc
            .split('-')
            .map(str::trim)
            .filter(|code| code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()))
            .map(str::to_string)
            .collect();

        if codes.is_empty() {
            continue;
        }

        map.entry(geocode.to_string()).or_insert(codes);
    }

    map
});

static CAPCP_CATEGORY_TO_ORG: phf::Map<&'static str, &'static str> = phf_map! {
    "Met" => "WXR",
    "Admin" => "EAS",
    "Other" => "CIV",
};

static CAPCP_EVENT_TO_SAME: phf::Map<&'static str, &'static str> = phf_map! {
    "911Service" => "TOE",
    "accident" => "CDW",
    "admin" => "ADR",
    "airQuality" => "SPS",
    "aircraftCras" => "LAE",
    "airportClose" => "ADR",
    "airspaceClos" => "ADR",
    "amber" => "CAE",
    "ambulance" => "LAE",
    "animalDang" => "CDW",
    "animalDiseas" => "CDW",
    "animalFeed" => "CEM",
    "animalHealth" => "CEM",
    "arcticOut" => "SVS",
    "avalanche" => "AVW",
    "aviation" => "LAE",
    "biological" => "BHW",
    "blizzard" => "BZW",
    "bloodSupply" => "LAE",
    "blowingSnow" => "WSW",
    "bridgeClose" => "LAE",
    "cable" => "ADR",
    "chemical" => "CHW",
    "civil" => "CEM",
    "civilEmerg" => "CEM",
    "civilEvent" => "CEM",
    "cold" => "SVS",
    "coldWave" => "SVS",
    "crime" => "CDW",
    "damBreach" => "DBW",
    "damOverflow" => "DBW",
    "dangerPerson" => "CDW",
    "diesel" => "LAE",
    "drinkingWate" => "CWW",
    "dustStorm" => "DSW",
    "earthquake" => "EQW",
    "electric" => "POS",
    "emergFacil" => "CEM",
    "emergSupport" => "CEM",
    "explosive" => "HMW",
    "facility" => "CEM",
    "fallObject" => "HMW",
    "fire" => "FRW",
    "flashFlood" => "FFW",
    "flashFreeze" => "FSW",
    "flood" => "FLW",
    "fog" => "SPS",
    "foodSupply" => "LAE",
    "forestFire" => "WFW",
    "freezeDrzl" => "WSW",
    "freezeRain" => "WSW",
    "freezngSpray" => "WSW",
    "frost" => "SPS",
    "galeWind" => "HWW",
    "gasoline" => "LAE",
    "geophyiscal" => "CEM",
    "hazmat" => "BHW",
    "health" => "BHW",
    "heat" => "SVS",
    "heatHumidity" => "SVS",
    "heatWave" => "SVS",
    "heatingOil" => "LAE",
    "highWater" => "SVS",
    "homeCrime" => "CEM",
    "hospital" => "LAE",
    "hurricFrcWnd" => "HUW",
    "hurricane" => "HUW",
    "ice" => "SPS",
    "icePressure" => "SPS",
    "iceberg" => "IBW",
    "industCrime" => "CEM",
    "industryFire" => "IFW",
    "infectious" => "DEW",
    "internet" => "ADR",
    "lahar" => "VOW",
    "landslide" => "LSW",
    "lavaFlow" => "VOW",
    "magnetStorm" => "CDW",
    "marine" => "SMW",
    "marineSecure" => "SMW",
    "meteor" => "CDW",
    "missingPer" => "MEP",
    "missingVPer" => "MEP",
    "naturalGas" => "LAE",
    "nautical" => "ADR",
    "notam" => "ADR",
    "other" => "CEM",
    "overflood" => "FLW",
    "plant" => "LAE",
    "plantInfect" => "LAE",
    "product" => "LAE",
    "publicServic" => "LAE",
    "pyroclaSurge" => "VOW",
    "pyroclasFlow" => "VOW",
    "radiological" => "RHW",
    "railway" => "LAE",
    "rainfall" => "SPS",
    "rdCondition" => "LAE",
    "reminder" => "CEM",
    "rescue" => "CEM",
    "retailCrime" => "CEM",
    "road" => "LAE",
    "roadClose" => "ADR",
    "roadDelay" => "ADR",
    "roadUsage" => "ADR",
    "rpdCloseLead" => "ADR",
    "satellite" => "ADR",
    "schoolBus" => "ADR",
    "schoolClose" => "ADR",
    "schoolLock" => "CDW",
    "sewer" => "LAE",
    "silver" => "CEM",
    "snowSquall" => "WSW",
    "snowfall" => "WSW",
    "spclIce" => "SPS",
    "spclMarine" => "SMW",
    "squall" => "SMW",
    "storm" => "SVS",
    "stormFrcWnd" => "SVS",
    "stormSurge" => "SSW",
    "strongWind" => "HWW",
    "telephone" => "LAE",
    "temperature" => "SPS",
    "terrorism" => "CDW",
    "testMessage" => "DMO",
    "thunderstorm" => "SVR",
    "tornado" => "TOR",
    "traffic" => "ADR",
    "train" => "ADR",
    "transit" => "ADR",
    "tropStorm" => "TRW",
    "tsunami" => "TSW",
    "urbanFire" => "FRW",
    "utility" => "ADR",
    "vehicleCrime" => "CEM",
    "volcanicAsh" => "VOW",
    "volcano" => "VOW",
    "volunteer" => "ADR",
    "waste" => "ADR",
    "water" => "ADR",
    "waterspout" => "SMW",
    "weather" => "SPS",
    "wildFire" => "FRW",
    "wind" => "HWW",
    "windchill" => "SPS",
    "winterStorm" => "WSW",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocumentOrigin {
    Live,
    Recovered,
}

impl DocumentOrigin {
    fn label(self) -> &'static str {
        match self {
            DocumentOrigin::Live => "live",
            DocumentOrigin::Recovered => "recovered",
        }
    }

    fn is_recovered(self) -> bool {
        matches!(self, DocumentOrigin::Recovered)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CapCpAlert {
    pub(crate) alert: CapAlert,
    pub(crate) geocodes: Vec<String>,
    pub(crate) broadcast_immediately: bool,
    pub(crate) wireless_immediate: bool,
    pub(crate) language: String,
    pub(crate) language_matched: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapCpFilterOutcome {
    pub(crate) language_matched: bool,
    pub(crate) geocode_matched: bool,
    pub(crate) broadcast_immediately: bool,
    pub(crate) wireless_immediate: bool,
    pub(crate) immediate_matched: bool,
    pub(crate) expired: bool,
}

impl CapCpFilterOutcome {
    pub(crate) fn passed(&self) -> bool {
        self.language_matched && self.geocode_matched && self.immediate_matched && !self.expired
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NaadReference {
    pub(crate) identifier: String,
    pub(crate) sent: String,
    pub(crate) date_path: String,
    pub(crate) file_name: String,
}

impl NaadReference {
    pub(crate) fn urls(&self) -> Vec<String> {
        NAAD_ARCHIVE_HOSTS
            .iter()
            .map(|host| format!("http://{host}/{}/{}", self.date_path, self.file_name))
            .collect()
    }
}

pub(crate) fn default_stream_endpoints() -> Vec<String> {
    NAAD_STREAM_ENDPOINTS
        .iter()
        .map(|entry| entry.to_string())
        .collect()
}

pub(crate) fn default_languages() -> Vec<String> {
    NAAD_DEFAULT_LANGUAGES
        .iter()
        .map(|entry| entry.to_string())
        .collect()
}

type RecoveryRequest = (Vec<NaadReference>, String);

struct CapCpContext {
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    db: DbHandle,
    client: reqwest::Client,
    /// Backed by the `cap_seen` table. It used to live only here, so every restart and every
    /// reload fetched and handled again each alert the heartbeats still listed.
    seen: Mutex<HashMap<String, DateTime<Utc>>>,
    persisted_active: Mutex<HashSet<String>>,
    recovery: mpsc::Sender<RecoveryRequest>,
}

fn naad_document_key(identifier: &str) -> String {
    format!("naad:{identifier}")
}

fn naad_document_seen_until() -> DateTime<Utc> {
    Utc::now() + ChronoDuration::days(NAAD_DOCUMENT_SEEN_DAYS)
}

async fn load_seen(db: &DbHandle) -> HashMap<String, DateTime<Utc>> {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    match db.load_cap_seen(&now).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(key, expires_at)| parse_cap_time(&expires_at).map(|at| (key, at)))
            .collect(),
        Err(err) => {
            warn!(
                "CAP-CP could not read what it handled before; alerts the heartbeats list may be handled again: {}",
                err
            );
            HashMap::new()
        }
    }
}

impl CapCpContext {
    /// True the first time `key` is marked, false while it is still remembered.
    async fn mark_seen(&self, key: String, until: DateTime<Utc>) -> bool {
        let now = Utc::now();
        let fresh = {
            let mut guard = self.seen.lock().await;
            guard.retain(|_, expires_at| *expires_at > now);
            guard.insert(key.clone(), until).is_none()
        };
        if fresh {
            let until = until.to_rfc3339_opts(SecondsFormat::Secs, true);
            if let Err(err) = self.db.record_cap_seen(&key, &until).await {
                warn!("CAP-CP could not remember {} across restarts: {}", key, err);
            }
        }
        fresh
    }

    async fn is_seen(&self, key: &str) -> bool {
        let now = Utc::now();
        let guard = self.seen.lock().await;
        guard
            .get(key)
            .map(|expires_at| *expires_at > now)
            .unwrap_or(false)
    }
}

pub async fn run_capcp_supervisor(
    initial_config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    mut reload_rx: broadcast::Receiver<Config>,
    db: DbHandle,
) -> Result<()> {
    let mut current_config = initial_config;
    let mut capcp_task: Option<JoinHandle<()>> = if current_config.process_capcp_alerts {
        Some(spawn_capcp_task(
            current_config.clone(),
            app_state.clone(),
            monitoring.clone(),
            db.clone(),
        ))
    } else {
        info!("CAP-CP processor disabled because PROCESS_CAPCP_ALERTS is false in your config.json file. No NAAD alerts will be processed or forwarded to webhooks.");
        None
    };

    loop {
        match reload_rx.recv().await {
            Ok(new_config) => {
                current_config = new_config;

                if let Some(task) = capcp_task.take() {
                    task.abort();
                    match task.await {
                        Ok(_) => {}
                        Err(err) if err.is_cancelled() => {}
                        Err(err) => warn!("CAP-CP processor task join error: {}", err),
                    }
                }

                if current_config.process_capcp_alerts {
                    info!("CAP-CP processor configuration reloaded; restarting NAAD listeners.");
                    capcp_task = Some(spawn_capcp_task(
                        current_config.clone(),
                        app_state.clone(),
                        monitoring.clone(),
                        db.clone(),
                    ));
                } else {
                    info!("CAP-CP processor disabled by reloaded configuration.");
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    "CAP-CP supervisor lagged on config updates (skipped {} message(s)); waiting for next update.",
                    skipped
                );
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    if let Some(task) = capcp_task.take() {
        task.abort();
        let _ = task.await;
    }

    Ok(())
}

fn spawn_capcp_task(
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    db: DbHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_capcp_processor(config, app_state, monitoring, db).await {
            warn!("CAP-CP processor task exited with error: {}", err);
        }
    })
}

async fn run_capcp_processor(
    config: Config,
    app_state: Arc<Mutex<AppState>>,
    monitoring: MonitoringHub,
    db: DbHandle,
) -> Result<()> {
    if config.capcp_stream_endpoints.is_empty() {
        warn!("CAP-CP processor enabled but CAPCP_STREAM_ENDPOINTS is empty; NAAD monitoring will not run.");
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(CAP_HTTP_TIMEOUT_SECS))
        .pool_max_idle_per_host(0)
        .build()
        .context("Failed to create CAP-CP HTTP client")?;

    let persisted = load_persisted_active_dedupe_keys(&config.shared_state_dir).await;
    let seen = load_seen(&db).await;
    let endpoints = config.capcp_stream_endpoints.clone();
    let (recovery, recovery_requests) = mpsc::channel(NAAD_RECOVERY_QUEUE_DEPTH);

    let context = Arc::new(CapCpContext {
        config,
        app_state,
        monitoring,
        db,
        client,
        seen: Mutex::new(seen),
        persisted_active: Mutex::new(persisted),
        recovery,
    });

    info!(
        "CAP-CP processor started with {} NAAD endpoint(s): {}.",
        endpoints.len(),
        endpoints.join(", ")
    );

    // Owned here, so the abort a reload sends this task takes the readers and the recovery worker
    // with it. Detached, they kept running beside their replacements after every reload.
    let mut tasks = JoinSet::new();
    tasks.spawn(run_capcp_recovery(Arc::clone(&context), recovery_requests));
    for (index, endpoint) in endpoints.into_iter().enumerate() {
        let context = Arc::clone(&context);
        let stagger = index as u64 * NAAD_STARTUP_STAGGER_MILLIS;
        tasks.spawn(async move { run_capcp_stream(context, endpoint, stagger).await });
    }

    while tasks.join_next().await.is_some() {}

    Ok(())
}

async fn run_capcp_stream(context: Arc<CapCpContext>, endpoint: String, stagger: u64) {
    let mut buffer: Vec<u8> = Vec::with_capacity(NAAD_READ_CHUNK_BYTES);
    let mut chunk = vec![0u8; NAAD_READ_CHUNK_BYTES];
    let mut failure_streak: u32 = 0;
    let mut warned = false;
    let mut ever_connected = false;

    if stagger > 0 {
        tokio::time::sleep(Duration::from_millis(stagger)).await;
    }

    loop {
        buffer.clear();

        let connect = timeout(
            Duration::from_secs(NAAD_CONNECT_TIMEOUT_SECS),
            TcpStream::connect(&endpoint),
        )
        .await;

        let mut stream = match connect {
            Ok(Ok(stream)) => stream,
            Ok(Err(err)) => {
                failure_streak = failure_streak.saturating_add(1);
                report_stream_failure(
                    &endpoint,
                    failure_streak,
                    &mut warned,
                    format_args!("connect failed: {err}"),
                );
                tokio::time::sleep(reconnect_delay(failure_streak)).await;
                continue;
            }
            Err(_) => {
                failure_streak = failure_streak.saturating_add(1);
                report_stream_failure(
                    &endpoint,
                    failure_streak,
                    &mut warned,
                    format_args!("connect timed out after {NAAD_CONNECT_TIMEOUT_SECS}s"),
                );
                tokio::time::sleep(reconnect_delay(failure_streak)).await;
                continue;
            }
        };

        if let Err(err) = stream.set_nodelay(true) {
            debug!("CAP-CP could not set TCP_NODELAY on {}: {}", endpoint, err);
        }

        if !ever_connected || warned {
            info!("CAP-CP connected to NAAD endpoint {}.", endpoint);
        } else {
            debug!("CAP-CP connected to NAAD endpoint {}.", endpoint);
        }
        ever_connected = true;

        let connected_at = Instant::now();
        let mut documents: u64 = 0;
        let mut closed_cleanly = false;

        loop {
            let read = match timeout(
                Duration::from_secs(NAAD_READ_TIMEOUT_SECS),
                stream.read(&mut chunk),
            )
            .await
            {
                Ok(Ok(0)) => {
                    closed_cleanly = true;
                    break;
                }
                Ok(Ok(read)) => read,
                Ok(Err(err)) => {
                    debug!("CAP-CP read error on NAAD {}: {}", endpoint, err);
                    break;
                }
                Err(_) => {
                    warn!(
                        "CAP-CP NAAD {} went silent for {}s (missed heartbeat); reconnecting.",
                        endpoint, NAAD_READ_TIMEOUT_SECS
                    );
                    break;
                }
            };

            buffer.extend_from_slice(&chunk[..read]);

            if buffer.len() > NAAD_BUFFER_LIMIT_BYTES {
                warn!(
                    "CAP-CP buffer for NAAD {} exceeded {} bytes without a complete alert; discarding.",
                    endpoint, NAAD_BUFFER_LIMIT_BYTES
                );
                buffer.clear();
                continue;
            }

            while let Some(document) = take_alert_document(&mut buffer) {
                documents = documents.saturating_add(1);
                handle_capcp_document(&context, document, &endpoint).await;
            }
        }

        let uptime = connected_at.elapsed();

        if connection_was_productive(uptime, documents) {
            if warned {
                info!(
                    "CAP-CP NAAD endpoint {} is healthy again after {} failed attempt(s).",
                    endpoint, failure_streak
                );
            }
            failure_streak = 0;
            warned = false;
            debug!(
                "CAP-CP NAAD endpoint {} closed after {:.1}s and {} document(s); reconnecting.",
                endpoint,
                uptime.as_secs_f64(),
                documents
            );
            tokio::time::sleep(reconnect_delay(1)).await;
            continue;
        }

        failure_streak = failure_streak.saturating_add(1);
        let detail = if closed_cleanly {
            "closed immediately with no data"
        } else {
            "dropped before delivering anything"
        };
        report_stream_failure(
            &endpoint,
            failure_streak,
            &mut warned,
            format_args!("{} after {:.0}ms", detail, uptime.as_secs_f64() * 1000.0),
        );
        tokio::time::sleep(reconnect_delay(failure_streak)).await;
    }
}

fn report_stream_failure(
    endpoint: &str,
    streak: u32,
    warned: &mut bool,
    detail: std::fmt::Arguments<'_>,
) {
    if streak >= NAAD_FAILURE_STREAK_WARN && !*warned {
        *warned = true;
        warn!(
            "CAP-CP NAAD endpoint {} has failed {} consecutive attempts ({}); backing off. The other endpoint carries the same traffic.",
            endpoint, streak, detail
        );
        return;
    }

    debug!("CAP-CP NAAD {} attempt {} {}.", endpoint, streak, detail);
}

fn connection_was_productive(uptime: Duration, documents: u64) -> bool {
    documents > 0 || uptime >= Duration::from_secs(NAAD_PRODUCTIVE_CONNECTION_SECS)
}

fn reconnect_delay(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(5);
    let seconds = (NAAD_RECONNECT_DELAY_SECS << shift).min(NAAD_RECONNECT_MAX_DELAY_SECS);
    let half_millis = seconds * 500;
    Duration::from_millis(half_millis + jitter_millis(half_millis))
}

fn jitter_millis(span_millis: u64) -> u64 {
    if span_millis == 0 {
        return 0;
    }

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_nanos()) % (span_millis + 1))
        .unwrap_or(0)
}

/// Runs on the stream's reader, so nothing in here may wait on the network or on processing: while
/// it did, the next live alert on that stream sat unread. With the other stream down, an archive
/// host timing out on a heartbeat's references held up alerts by tens of seconds.
async fn handle_capcp_document(context: &Arc<CapCpContext>, xml: String, endpoint: &str) {
    if is_heartbeat_document(&xml) {
        match parse_heartbeat_references(&xml) {
            Ok(references) => {
                debug!(
                    "CAP-CP heartbeat from {} references {} alert(s).",
                    endpoint,
                    references.len()
                );
                if context
                    .recovery
                    .try_send((references, endpoint.to_string()))
                    .is_err()
                {
                    debug!(
                        "CAP-CP recovery is still working through earlier heartbeats; the next one lists these references again."
                    );
                }
            }
            Err(err) => debug!(
                "CAP-CP heartbeat from {} had no usable references: {}",
                endpoint, err
            ),
        }
        return;
    }

    // Both streams carry every alert, so the second copy stops here.
    if let Some(identifier) = extract_identifier(&xml) {
        if !context
            .mark_seen(naad_document_key(&identifier), naad_document_seen_until())
            .await
        {
            debug!(
                "CAP-CP {} from {} was already handled; skipping.",
                identifier, endpoint
            );
            return;
        }
    }

    // Detached rather than owned by the processor, so a reload cannot cut an alert off halfway
    // through its narration or relay.
    let context = Arc::clone(context);
    let endpoint = endpoint.to_string();
    tokio::spawn(async move {
        process_capcp_alert_xml(&context, &xml, &endpoint, DocumentOrigin::Live).await;
    });
}

/// Fetches and handles the alerts heartbeats reference that this listener has not seen, which are
/// the ones it missed while disconnected.
async fn run_capcp_recovery(
    context: Arc<CapCpContext>,
    mut requests: mpsc::Receiver<RecoveryRequest>,
) {
    while let Some((references, endpoint)) = requests.recv().await {
        for reference in references {
            let key = naad_document_key(&reference.identifier);
            if context.is_seen(&key).await {
                continue;
            }

            let Some(archived) = fetch_archived_alert(&context, &reference).await else {
                continue;
            };

            // The live copy may have arrived while the archive was answering.
            if !context.mark_seen(key, naad_document_seen_until()).await {
                continue;
            }

            if is_heartbeat_document(&archived) {
                debug!(
                    "CAP-CP skipping archived heartbeat {}.",
                    reference.identifier
                );
                continue;
            }

            info!(
                "CAP-CP recovered {} from the NAAD archive; processing it.",
                reference.identifier
            );
            process_capcp_alert_xml(&context, &archived, &endpoint, DocumentOrigin::Recovered)
                .await;
        }
    }
}

async fn process_capcp_alert_xml(
    context: &Arc<CapCpContext>,
    xml: &str,
    endpoint: &str,
    origin: DocumentOrigin,
) {
    let source_url = capcp_source_url(endpoint, xml);

    let parsed = match parse_capcp_alert(xml, &source_url, &context.config.capcp_languages) {
        Ok(parsed) => parsed,
        Err(err) => {
            capcp_drop(
                origin,
                format_args!(
                    "CAP-CP {} alert from {} could not be parsed: {}",
                    origin.label(),
                    endpoint,
                    err
                ),
            );
            return;
        }
    };

    let outcome = evaluate_capcp_filter(
        &parsed,
        &context.config.capcp_geocode_filter,
        context.config.capcp_require_immediate,
    );

    if outcome.broadcast_immediately || outcome.wireless_immediate {
        info!(
            "CAP-CP alert {} is flagged immediate by SOREM (broadcast={}, wireless={}).",
            parsed.alert.identifier, outcome.broadcast_immediately, outcome.wireless_immediate
        );
    }

    if !is_capcp_document(xml) {
        debug!(
            "NAAD alert {} does not declare a {} code; parsing it as CAP-CP anyway.",
            parsed.alert.identifier, CAPCP_PROFILE_CODE_PREFIX
        );
    }

    let language = parsed.language;
    let alert = parsed.alert;
    let event_code = normalize_event_code(&alert.event_code);

    if !outcome.passed() {
        if origin.is_recovered()
            && outcome.expired
            && outcome.language_matched
            && outcome.geocode_matched
            && outcome.immediate_matched
        {
            info!(
                "CAP-CP recovered alert {} ({}) had already expired; archiving it without relaying.",
                alert.identifier, event_code
            );
            archive_cap_alert(
                &context.config,
                &context.db,
                &alert,
                &event_code,
                &source_url,
            )
            .await;
            return;
        }

        capcp_drop(
            origin,
            format_args!(
                "Skipping CAP-CP {} alert {} (language_matched={}, geocode_matched={}, immediate_matched={}, broadcast_immediately={}, wireless_immediate={}, expired={})",
                origin.label(),
                alert.identifier,
                outcome.language_matched,
                outcome.geocode_matched,
                outcome.immediate_matched,
                outcome.broadcast_immediately,
                outcome.wireless_immediate,
                outcome.expired
            ),
        );
        return;
    }

    let dedupe_key = build_dedupe_key(&alert);
    let seen_until = match alert.expires {
        Some(expires_at) if expires_at > Utc::now() => expires_at,
        _ => Utc::now() + ChronoDuration::seconds(CAP_SEEN_DEFAULT_TTL_SECS),
    };

    if !context.mark_seen(dedupe_key.clone(), seen_until).await {
        debug!(
            "Skipping CAP-CP {} alert {} because it is already seen (dedupe key={})",
            origin.label(),
            alert.identifier,
            dedupe_key
        );
        return;
    }

    let already_active = {
        let guard = context.persisted_active.lock().await;
        guard.contains(&dedupe_key)
    } || cap_alert_is_active(&context.app_state, &dedupe_key).await;

    if already_active {
        backfill_persisted_cap_details(
            &context.config,
            &context.app_state,
            &context.monitoring,
            &dedupe_key,
            &alert,
        )
        .await;

        info!(
            "CAP-CP {} alert {} is already active; backfilled details instead of re-relaying.",
            origin.label(),
            alert.identifier
        );
        return;
    }

    info!(
        "Processing CAP-CP {} alert {} ({}, lang={}) from {} as {}.",
        origin.label(),
        alert.identifier,
        event_code,
        language,
        endpoint,
        build_capcp_raw_header(&alert)
    );

    process_cap_alert(
        &context.config,
        &context.app_state,
        &context.monitoring,
        &context.client,
        &source_url,
        alert,
        &context.db,
    )
    .await;

    update_alert_files(
        &context.config.shared_state_dir,
        &*context.app_state.lock().await,
    )
    .await
    .ok();

    context.persisted_active.lock().await.insert(dedupe_key);
}

fn capcp_drop(origin: DocumentOrigin, message: std::fmt::Arguments<'_>) {
    if origin.is_recovered() {
        warn!("{}", message);
    } else {
        debug!("{}", message);
    }
}

/// Asks every archive host at once and takes the first answer, so a host that is down costs
/// nothing rather than a full timeout per alert. The rest are cancelled when this returns.
async fn fetch_archived_alert(
    context: &Arc<CapCpContext>,
    reference: &NaadReference,
) -> Option<String> {
    let mut requests = JoinSet::new();
    for url in reference.urls() {
        let client = context.client.clone();
        requests.spawn(async move { fetch_archive_url(&client, &url).await });
    }
    while let Some(outcome) = requests.join_next().await {
        if let Ok(Some(body)) = outcome {
            return Some(body);
        }
    }

    debug!(
        "CAP-CP could not recover {} from any archive host.",
        reference.file_name
    );
    None
}

async fn fetch_archive_url(client: &reqwest::Client, url: &str) -> Option<String> {
    let request = client
        .get(url)
        .timeout(Duration::from_secs(NAAD_ARCHIVE_TIMEOUT_SECS));
    match request.send().await {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(bytes) if bytes.len() <= NAAD_ARCHIVE_MAX_BYTES => {
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            Ok(bytes) => {
                warn!(
                    "CAP-CP archive response for {} was {} bytes, over the {} byte cap.",
                    url,
                    bytes.len(),
                    NAAD_ARCHIVE_MAX_BYTES
                );
                None
            }
            Err(err) => {
                debug!("CAP-CP archive read failed for {}: {}", url, err);
                None
            }
        },
        Ok(response) => {
            debug!(
                "CAP-CP archive returned HTTP {} for {}",
                response.status(),
                url
            );
            None
        }
        Err(err) => {
            debug!("CAP-CP archive request failed for {}: {}", url, err);
            None
        }
    }
}

fn capcp_source_url(endpoint: &str, xml: &str) -> String {
    match extract_identifier(xml) {
        Some(identifier) => format!("naad://{endpoint}#{identifier}"),
        None => format!("naad://{endpoint}"),
    }
}

fn extract_identifier(xml: &str) -> Option<String> {
    let doc = Document::parse(xml).ok()?;
    child_text(doc.root_element(), "identifier")
}

pub(crate) fn take_alert_document(buffer: &mut Vec<u8>) -> Option<String> {
    let end = find_alert_close(buffer)?;
    let raw: Vec<u8> = buffer.drain(..end).collect();
    let text = String::from_utf8_lossy(&raw).into_owned();

    let start = text
        .find("<?xml")
        .or_else(|| text.find('<'))
        .unwrap_or_default();
    let document = text[start..].trim().to_string();

    (!document.is_empty()).then_some(document)
}

fn find_alert_close(haystack: &[u8]) -> Option<usize> {
    let mut index = 0usize;

    while index + 2 <= haystack.len() {
        let offset = find_subslice(&haystack[index..], b"</")?;

        let start = index + offset;
        let mut cursor = start + 2;
        let name_start = cursor;

        while cursor < haystack.len() && is_xml_name_byte(haystack[cursor]) {
            cursor += 1;
        }

        let name = &haystack[name_start..cursor];
        let local = match name.iter().rposition(|byte| *byte == b':') {
            Some(position) => &name[position + 1..],
            None => name,
        };

        if local.eq_ignore_ascii_case(b"alert") {
            let mut tail = cursor;
            while tail < haystack.len() && haystack[tail].is_ascii_whitespace() {
                tail += 1;
            }
            if tail < haystack.len() && haystack[tail] == b'>' {
                return Some(tail + 1);
            }
        }

        index = start + 2;
    }

    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_xml_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
}

pub(crate) fn is_heartbeat_document(xml: &str) -> bool {
    let Ok(doc) = Document::parse(xml) else {
        return false;
    };
    let root = doc.root_element();
    if root.tag_name().name() != "alert" {
        return false;
    }

    child_text(root, "sender")
        .map(|sender| {
            sender
                .to_ascii_lowercase()
                .contains(&NAAD_HEARTBEAT_SENDER.to_ascii_lowercase())
        })
        .unwrap_or(false)
}

pub(crate) fn parse_heartbeat_references(xml: &str) -> Result<Vec<NaadReference>> {
    let doc = Document::parse(xml).map_err(|err| anyhow!("Invalid heartbeat XML: {}", err))?;
    let root = doc.root_element();
    let references =
        child_text(root, "references").ok_or_else(|| anyhow!("Heartbeat has no <references>"))?;

    Ok(parse_reference_list(&references))
}

pub(crate) fn parse_reference_list(references: &str) -> Vec<NaadReference> {
    references
        .split_whitespace()
        .filter_map(|entry| {
            let (_, rest) = entry.split_once(',')?;
            let (identifier, sent) = rest.split_once(',')?;
            let identifier = identifier.trim();
            let sent = sent.trim();
            if identifier.is_empty() || sent.is_empty() {
                return None;
            }

            let date_path = sent.split_once('T').map(|(date, _)| date).unwrap_or(sent);

            Some(NaadReference {
                identifier: identifier.to_string(),
                sent: sent.to_string(),
                date_path: date_path.to_string(),
                file_name: format!(
                    "{}I{}.xml",
                    naad_path_token(sent),
                    naad_path_token(identifier)
                ),
            })
        })
        .collect()
}

fn naad_path_token(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '-' | ':' => '_',
            '+' => 'p',
            other => other,
        })
        .collect()
}

pub(crate) fn parse_capcp_alert(
    xml: &str,
    source_url: &str,
    languages: &[String],
) -> Result<CapCpAlert> {
    let doc = Document::parse(xml).map_err(|err| anyhow!("Invalid CAP-CP alert XML: {}", err))?;
    let root = doc.root_element();

    if root.tag_name().name() != "alert" {
        return Err(anyhow!(
            "Expected <alert> root node, found <{}>",
            root.tag_name().name()
        ));
    }

    let msg_type = child_text(root, "msgType").unwrap_or_else(|| "Alert".to_string());
    if msg_type.eq_ignore_ascii_case("cancel") {
        return Err(anyhow!("CAP-CP alert is a cancellation message"));
    }

    let identifier = child_text(root, "identifier").unwrap_or_else(|| source_url.to_string());
    let sender = child_text(root, "sender").unwrap_or_else(|| "Unknown sender".to_string());
    let sent = child_text(root, "sent").as_deref().and_then(parse_cap_time);
    let scope = child_text(root, "scope").unwrap_or_else(|| "Public".to_string());

    let (info_node, language, language_matched) = select_info_node(root, languages)
        .ok_or_else(|| anyhow!("CAP-CP alert missing <info> section"))?;

    let sender_name =
        child_text(root, "senderName").or_else(|| child_text(info_node, "senderName"));

    let event_code = capcp_event_code(info_node);
    let event_text = capcp_event_title(&event_code)
        .or_else(|| child_text(info_node, "event"))
        .unwrap_or_else(|| "CAP-CP Alert".to_string());

    let originator_code = capcp_originator_code(info_node);
    let sender_name = sender_name.or_else(|| capcp_originator_name(&originator_code));

    let urgency = child_text(info_node, "urgency");
    let severity = child_text(info_node, "severity");
    let certainty = child_text(info_node, "certainty");
    let instructions = child_text(info_node, "instruction");
    let expires = child_text(info_node, "expires")
        .as_deref()
        .and_then(parse_cap_time);

    let mut areas = Vec::new();
    let mut geocodes = Vec::new();
    for area in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "area")
    {
        if let Some(area_desc) = child_text(area, "areaDesc") {
            areas.push(area_desc);
        }

        for geocode in area
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "geocode")
        {
            if let Some(value) = value_for_prefixed_name(geocode, CAPCP_LOCATION_VALUE_PREFIX) {
                geocodes.push(value);
            }
        }
    }

    let fips = capcp_location_codes(info_node, &geocodes);

    let description_raw = build_broadcast_text(info_node, &areas);
    let description = sanitize_cap_description(&description_raw);
    let simple_description = simple_sanitize_description(&description_raw);

    let (audio_mime_type, audio_uri, audio_deref_uri) = capcp_audio_resource(info_node);

    let alert = CapAlert {
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
        instructions,
        simple_description,
        areas,
        fips,
        audio_uri,
        audio_deref_uri,
        audio_mime_type,
        source_url: source_url.to_string(),
        canadian: true,
    };

    Ok(CapCpAlert {
        alert,
        geocodes,
        broadcast_immediately: capcp_broadcast_immediately(info_node),
        wireless_immediate: capcp_wireless_immediate(info_node),
        language,
        language_matched,
    })
}

fn select_info_node<'a, 'input>(
    root: Node<'a, 'input>,
    languages: &[String],
) -> Option<(Node<'a, 'input>, String, bool)> {
    let info_nodes: Vec<Node<'a, 'input>> = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "info")
        .collect();

    if info_nodes.is_empty() {
        return None;
    }

    for wanted in languages {
        for info in &info_nodes {
            let language = info_language(*info);
            if language == *wanted {
                return Some((*info, language, true));
            }
        }
    }

    let fallback = info_nodes[0];
    let language = info_language(fallback);
    Some((fallback, language, false))
}

fn info_language(info_node: Node<'_, '_>) -> String {
    let raw = child_text(info_node, "language").unwrap_or_else(|| "en-US".to_string());
    raw.trim()
        .to_ascii_lowercase()
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect::<String>()
        .chars()
        .take(2)
        .collect()
}

fn capcp_event_code(info_node: Node<'_, '_>) -> String {
    for container in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "eventCode")
    {
        if let Some(value) = extract_same_from_container(container) {
            let normalized = normalize_event_code(&value);
            if normalized != "CAP" {
                return normalized;
            }
        }
    }

    for container in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "eventCode")
    {
        if let Some(value) = value_for_prefixed_name(container, CAPCP_EVENT_VALUE_PREFIX) {
            if let Some(mapped) = CAPCP_EVENT_TO_SAME.get(value.trim()) {
                return normalize_event_code(mapped);
            }
        }
    }

    CAPCP_FALLBACK_EVENT_CODE.to_string()
}

fn capcp_originator_code(info_node: Node<'_, '_>) -> String {
    if let Some(value) = extract_parameter_value(info_node, "EAS-ORG") {
        return normalize_originator_code(&value);
    }

    child_text(info_node, "category")
        .and_then(|category| {
            CAPCP_CATEGORY_TO_ORG
                .get(category.trim())
                .map(|org| normalize_originator_code(org))
        })
        .unwrap_or_else(|| CAPCP_FALLBACK_ORIGINATOR_CODE.to_string())
}

fn capcp_location_codes(info_node: Node<'_, '_>, geocodes: &[String]) -> Vec<String> {
    if let Some(value) = extract_parameter_value(info_node, EC_NEWLY_ACTIVE_AREAS) {
        let codes = dedupe_codes(split_fips_codes(&value));
        if !codes.is_empty() {
            return codes;
        }
    }

    let mut mapped = Vec::new();
    for geocode in geocodes {
        match geocode_to_clc(geocode) {
            Some(codes) => mapped.extend(codes.iter().cloned()),
            None => mapped.extend(capcp_location_to_same(geocode)),
        }
    }

    let mapped = dedupe_codes(mapped);
    if !mapped.is_empty() {
        return mapped;
    }

    let mut same_codes = Vec::new();
    for area in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "area")
    {
        for geocode in area
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "geocode")
        {
            if let Some(value) = extract_same_from_container(geocode) {
                same_codes.extend(split_fips_codes(&value));
            }
        }
    }

    let same_codes = dedupe_codes(same_codes);
    if !same_codes.is_empty() {
        return same_codes;
    }

    let broadened = dedupe_codes(
        geocodes
            .iter()
            .filter_map(|geocode| geocode_to_clc_by_prefix(geocode))
            .flat_map(|codes| codes.iter().cloned())
            .collect(),
    );
    if !broadened.is_empty() {
        return broadened;
    }

    vec!["000000".to_string()]
}

pub(crate) fn geocode_to_clc(geocode: &str) -> Option<&'static [String]> {
    GEO_TO_CLC.get(geocode.trim()).map(Vec::as_slice)
}

pub(crate) fn geocode_to_clc_by_prefix(geocode: &str) -> Option<&'static [String]> {
    let trimmed = geocode.trim();
    [4usize, 2]
        .iter()
        .filter(|width| trimmed.len() > **width)
        .find_map(|width| geocode_to_clc(trimmed.get(..*width)?))
}

pub(crate) fn capcp_location_to_same(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|ch| ch.is_ascii_digit()).collect();
    let candidate = match digits.len() {
        5 => format!("0{digits}"),
        6 => digits,
        _ => return None,
    };

    SAME_CA
        .same
        .contains_key(&candidate[1..])
        .then_some(candidate)
}

fn dedupe_codes(codes: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    codes
        .into_iter()
        .filter(|code| !code.is_empty())
        .filter(|code| seen.insert(code.clone()))
        .collect()
}

pub(crate) fn capcp_event_title(event_code: &str) -> Option<String> {
    let key = event_code.trim().to_ascii_uppercase();
    let title = SAME_CA.events.get(key.as_str())?.trim();

    let without_article = title
        .strip_prefix("an ")
        .or_else(|| title.strip_prefix("a "))
        .or_else(|| title.strip_prefix("An "))
        .or_else(|| title.strip_prefix("A "))
        .unwrap_or(title)
        .trim();

    (!without_article.is_empty()).then(|| without_article.to_string())
}

pub(crate) fn capcp_originator_name(originator_code: &str) -> Option<String> {
    let key = originator_code.trim().to_ascii_uppercase();
    SAME_CA.orgs.get(key.as_str()).cloned()
}

fn capcp_broadcast_immediately(info_node: Node<'_, '_>) -> bool {
    capcp_yes_flag(info_node, SOREM_BROADCAST_IMMEDIATE_KEY)
}

fn capcp_wireless_immediate(info_node: Node<'_, '_>) -> bool {
    capcp_yes_flag(info_node, SOREM_WIRELESS_IMMEDIATE_KEY)
}

fn capcp_yes_flag(info_node: Node<'_, '_>, key: &str) -> bool {
    for parameter in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "parameter")
    {
        let Some(value_name) = child_text(parameter, "valueName") else {
            continue;
        };
        if !normalize_value_name(&value_name).contains(key) {
            continue;
        }
        if let Some(value) = child_text(parameter, "value") {
            return value.trim().to_ascii_lowercase().contains("yes");
        }
    }
    false
}

pub(crate) fn normalize_value_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn build_broadcast_text(info_node: Node<'_, '_>, areas: &[String]) -> String {
    if let Some(text) = extract_parameter_value(info_node, SOREM_BROADCAST_TEXT) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return collapse_broadcast_text(trimmed);
        }
    }

    let headline = child_text(info_node, "headline")
        .map(|value| format!("{}.", value.trim_end_matches('.')))
        .unwrap_or_default();
    let area_desc = if areas.is_empty() {
        String::new()
    } else {
        format!("{}.", areas.join(", "))
    };
    let description = child_text(info_node, "description").unwrap_or_default();
    let instruction = child_text(info_node, "instruction").unwrap_or_default();

    let combined = format!("{headline} {area_desc} {description} {instruction}");
    let collapsed = collapse_broadcast_text(&combined);

    if collapsed.is_empty() {
        DEFAULT_NO_DESCRIPTION.to_string()
    } else {
        collapsed
    }
}

fn collapse_broadcast_text(input: &str) -> String {
    let mut text = input.replace("###", " ").replace(['\r', '\n'], " ");
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    while text.contains(" .") {
        text = text.replace(" .", ".");
    }
    while text.contains("..") {
        text = text.replace("..", ".");
    }
    text.trim().to_string()
}

fn capcp_audio_resource(
    info_node: Node<'_, '_>,
) -> (Option<String>, Option<String>, Option<String>) {
    for resource in info_node
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "resource")
    {
        let mime = child_text(resource, "mimeType");
        let uri = child_text(resource, "uri");
        let deref_uri = child_text(resource, "derefUri");
        if is_audio_resource(mime.as_deref(), uri.as_deref(), deref_uri.as_deref()) {
            return (mime, uri, deref_uri);
        }
    }

    (None, None, None)
}

fn value_for_prefixed_name(container: Node<'_, '_>, prefix: &str) -> Option<String> {
    let value_name = child_text(container, "valueName")?;
    if !value_name
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        return None;
    }
    child_text(container, "value")
}

pub(crate) fn is_capcp_document(xml: &str) -> bool {
    let Ok(doc) = Document::parse(xml) else {
        return false;
    };

    doc.root_element()
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "code")
        .filter_map(|node| node.text())
        .any(|text| text.trim().starts_with(CAPCP_PROFILE_CODE_PREFIX))
}

pub(crate) fn evaluate_capcp_filter(
    parsed: &CapCpAlert,
    geocode_filter: &[String],
    require_immediate: bool,
) -> CapCpFilterOutcome {
    // Either flag is enough: an alert marked for immediate broadcast interruption and one marked
    // for immediate wireless delivery are both "send this now", and the profile does not always
    // set both.
    let immediate_matched =
        !require_immediate || parsed.broadcast_immediately || parsed.wireless_immediate;

    CapCpFilterOutcome {
        language_matched: parsed.language_matched,
        geocode_matched: geocode_filter_matches(&parsed.geocodes, geocode_filter),
        broadcast_immediately: parsed.broadcast_immediately,
        wireless_immediate: parsed.wireless_immediate,
        immediate_matched,
        expired: is_expired(parsed.alert.expires),
    }
}

pub(crate) fn geocode_filter_matches(geocodes: &[String], filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }

    if filter.iter().any(|entry| entry == "*") {
        return true;
    }

    if geocodes.is_empty() {
        return true;
    }

    geocodes.iter().any(|code| {
        filter.iter().any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => code.starts_with(prefix),
            None => entry == code,
        })
    })
}

pub(crate) fn is_expired(expires: Option<DateTime<Utc>>) -> bool {
    expires
        .map(|expires_at| expires_at <= Utc::now())
        .unwrap_or(false)
}

pub(crate) fn build_capcp_raw_header(alert: &CapAlert) -> String {
    build_cap_raw_header(
        &alert.originator_code,
        &alert.event_code,
        &alert.fips,
        alert.sent,
        alert.expires,
        &alert.source_url,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn valid_alert_xml() -> &'static str {
        include_str!("../tests/fixtures/capcp_alert_valid.xml")
    }

    fn heartbeat_xml() -> &'static str {
        include_str!("../tests/fixtures/capcp_heartbeat.xml")
    }

    #[test]
    fn reconnect_delay_backs_off_and_stays_capped() {
        let mut previous = Duration::ZERO;

        for streak in 1..=10u32 {
            let delay = reconnect_delay(streak);

            let shift = streak.saturating_sub(1).min(5);
            let window = (NAAD_RECONNECT_DELAY_SECS << shift).min(NAAD_RECONNECT_MAX_DELAY_SECS);

            assert!(
                delay >= Duration::from_millis(window * 500),
                "streak {streak} delayed {delay:?}, under half its {window}s window"
            );
            assert!(
                delay <= Duration::from_secs(window),
                "streak {streak} delayed {delay:?}, over its {window}s window"
            );
            assert!(
                delay <= Duration::from_secs(NAAD_RECONNECT_MAX_DELAY_SECS),
                "streak {streak} exceeded the backoff cap"
            );

            if streak > 1 && window > NAAD_RECONNECT_DELAY_SECS {
                assert!(
                    delay >= previous / 2,
                    "streak {streak} did not grow relative to the previous window"
                );
            }
            previous = delay;
        }
    }

    #[test]
    fn reconnect_delay_recovers_immediately_after_a_healthy_run() {
        let delay = reconnect_delay(1);
        assert!(delay >= Duration::from_millis(NAAD_RECONNECT_DELAY_SECS * 500));
        assert!(delay <= Duration::from_secs(NAAD_RECONNECT_DELAY_SECS));
    }

    #[test]
    fn connection_health_counts_data_or_uptime() {
        assert!(!connection_was_productive(Duration::from_millis(150), 0));
        assert!(connection_was_productive(Duration::from_millis(150), 1));
        assert!(connection_was_productive(
            Duration::from_secs(NAAD_PRODUCTIVE_CONNECTION_SECS),
            0
        ));
        assert!(!connection_was_productive(
            Duration::from_secs(NAAD_PRODUCTIVE_CONNECTION_SECS - 1),
            0
        ));
    }

    #[test]
    fn jitter_stays_inside_its_span() {
        assert_eq!(jitter_millis(0), 0);
        for span in [1u64, 250, 2_500, 60_000] {
            assert!(jitter_millis(span) <= span);
        }
    }

    #[test]
    fn default_endpoints_and_languages_match_naad() {
        assert_eq!(
            default_stream_endpoints(),
            vec![
                "streaming1.naad-adna.pelmorex.com:8080".to_string(),
                "streaming2.naad-adna.pelmorex.com:8080".to_string()
            ]
        );
        assert_eq!(default_languages(), vec!["en".to_string()]);
    }

    #[test]
    fn take_alert_document_splits_back_to_back_documents() {
        let mut buffer = b"<alert><identifier>one</identifier></alert><alert><identifier>two</identifier></alert>".to_vec();

        let first = take_alert_document(&mut buffer).expect("first document");
        assert_eq!(first, "<alert><identifier>one</identifier></alert>");

        let second = take_alert_document(&mut buffer).expect("second document");
        assert_eq!(second, "<alert><identifier>two</identifier></alert>");

        assert!(take_alert_document(&mut buffer).is_none());
        assert!(buffer.is_empty());
    }

    #[test]
    fn take_alert_document_waits_for_a_complete_document() {
        let mut buffer = b"<alert><identifier>partial</identifier>".to_vec();
        assert!(take_alert_document(&mut buffer).is_none());

        buffer.extend_from_slice(b"</alert>");
        let document = take_alert_document(&mut buffer).expect("completed document");
        assert!(document.ends_with("</alert>"));
    }

    #[test]
    fn take_alert_document_handles_namespaced_and_declared_documents() {
        let mut buffer =
            b"<?xml version=\"1.0\"?>\n<cap:alert><cap:identifier>ns</cap:identifier></cap:alert >"
                .to_vec();
        let document = take_alert_document(&mut buffer).expect("namespaced document");
        assert!(document.starts_with("<?xml"));
        assert!(document.ends_with("</cap:alert >"));
    }

    #[test]
    fn take_alert_document_ignores_other_closing_tags() {
        let mut buffer = b"<alert><info></info><alerting></alerting></alert>".to_vec();
        let document = take_alert_document(&mut buffer).expect("document");
        assert_eq!(
            document,
            "<alert><info></info><alerting></alerting></alert>"
        );
    }

    #[test]
    fn archived_heartbeats_are_rejected_before_the_alert_pipeline() {
        let parsed = parse_capcp_alert(heartbeat_xml(), "naad://test", &languages(&["en"]))
            .expect("heartbeats do parse as alerts");
        assert_eq!(parsed.alert.event_code, "CEM");
        assert_eq!(parsed.alert.fips, vec!["000000".to_string()]);

        assert!(is_heartbeat_document(heartbeat_xml()));
    }

    #[test]
    fn a_live_heartbeat_never_reaches_the_alert_path() {
        assert!(is_heartbeat_document(heartbeat_xml()));
        assert!(!is_heartbeat_document(valid_alert_xml()));
    }

    #[test]
    fn document_origin_labels_are_distinct() {
        assert_eq!(DocumentOrigin::Live.label(), "live");
        assert_eq!(DocumentOrigin::Recovered.label(), "recovered");
        assert!(DocumentOrigin::Recovered.is_recovered());
        assert!(!DocumentOrigin::Live.is_recovered());
    }

    #[test]
    fn expired_recovered_alerts_are_archivable_but_not_relayable() {
        let xml =
            valid_alert_xml().replace("2035-09-09T18:00:00-00:00", "2020-01-01T00:00:00-00:00");
        let parsed =
            parse_capcp_alert(&xml, "naad://test", &languages(&["en"])).expect("parsed alert");

        let outcome = evaluate_capcp_filter(&parsed, &[], false);
        assert!(!outcome.passed());
        assert!(outcome.expired);
        assert!(outcome.language_matched);
        assert!(outcome.geocode_matched);
    }

    #[test]
    fn a_filtered_out_recovered_alert_is_not_archived() {
        let parsed = parse_capcp_alert(valid_alert_xml(), "naad://test", &languages(&["en"]))
            .expect("parsed alert");

        let outcome = evaluate_capcp_filter(&parsed, &["24*".to_string()], false);
        assert!(!outcome.passed());
        assert!(!outcome.geocode_matched);
        assert!(!outcome.expired);
    }

    #[test]
    fn heartbeat_documents_are_detected_by_sender() {
        assert!(is_heartbeat_document(heartbeat_xml()));
        assert!(!is_heartbeat_document(valid_alert_xml()));
    }

    #[test]
    fn parse_heartbeat_references_builds_archive_coordinates() {
        let references = parse_heartbeat_references(heartbeat_xml()).expect("references");
        assert_eq!(references.len(), 2);

        let first = &references[0];
        assert_eq!(first.identifier, "urn:oid:2.49.0.1.124.1234567.2026");
        assert_eq!(first.sent, "2026-09-09T14:05:00-00:00");
        assert_eq!(first.date_path, "2026-09-09");
        assert_eq!(
            first.file_name,
            "2026_09_09T14_05_00_00_00Iurn_oid_2.49.0.1.124.1234567.2026.xml"
        );
        assert_eq!(
            first.urls(),
            vec![
                "http://capcp1.naad-adna.pelmorex.com/2026-09-09/2026_09_09T14_05_00_00_00Iurn_oid_2.49.0.1.124.1234567.2026.xml".to_string(),
                "http://capcp2.naad-adna.pelmorex.com/2026-09-09/2026_09_09T14_05_00_00_00Iurn_oid_2.49.0.1.124.1234567.2026.xml".to_string(),
            ]
        );
    }

    #[test]
    fn parse_reference_list_substitutes_plus_offsets() {
        let references =
            parse_reference_list("sender@example.ca,urn:oid:1.2+3,2026-09-09T14:05:00+00:00");
        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].file_name,
            "2026_09_09T14_05_00p00_00Iurn_oid_1.2p3.xml"
        );
        assert_eq!(references[0].date_path, "2026-09-09");
    }

    #[test]
    fn parse_reference_list_skips_malformed_entries() {
        let references = parse_reference_list("no-commas-here another,onlyone");
        assert!(references.is_empty());
    }

    #[test]
    fn parse_capcp_alert_extracts_core_fields() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
            &languages(&["en"]),
        )
        .expect("parsed alert");

        assert_eq!(parsed.alert.identifier, "TEST-CAPCP-001");
        assert_eq!(parsed.alert.event_code, "TOR");
        assert_eq!(parsed.alert.event_text, "Tornado Warning");
        assert_eq!(parsed.alert.originator_code, "WXR");
        assert!(parsed.alert.canadian);
        assert_eq!(parsed.language, "en");
        assert!(parsed.language_matched);
        assert!(parsed.broadcast_immediately);
        assert_eq!(parsed.geocodes, vec!["3520005".to_string()]);
        assert_eq!(
            parsed.alert.fips,
            vec!["043100".to_string(), "046100".to_string()]
        );
        assert_eq!(
            parsed.alert.audio_uri.as_deref(),
            Some("https://alerts.example.ca/audio/capcp.mp3")
        );
        assert_eq!(parsed.alert.audio_mime_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(
            parsed.alert.instructions.as_deref(),
            Some("Take shelter immediately.")
        );
    }

    #[test]
    fn parse_capcp_alert_prefers_broadcast_text() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://test#TEST-CAPCP-001",
            &languages(&["en"]),
        )
        .expect("parsed alert");

        assert_eq!(
            parsed.alert.description_raw,
            "A tornado warning is in effect for southern Ontario. Take shelter immediately."
        );
    }

    #[test]
    fn parse_capcp_alert_selects_the_configured_language() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://test#TEST-CAPCP-001",
            &languages(&["fr"]),
        )
        .expect("parsed alert");

        assert_eq!(parsed.language, "fr");
        assert!(parsed.language_matched);
        assert!(parsed.alert.description_raw.contains("tornade"));
    }

    #[test]
    fn parse_capcp_alert_flags_unmatched_languages() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://test#TEST-CAPCP-001",
            &languages(&["de"]),
        )
        .expect("parsed alert");

        assert!(!parsed.language_matched);
        assert!(!evaluate_capcp_filter(&parsed, &[], false).passed());
    }

    #[test]
    fn parse_capcp_alert_rejects_cancellations() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-CANCEL</identifier>
  <sender>test@example.ca</sender>
  <msgType>Cancel</msgType>
  <info><language>en-CA</language><event>Tornado</event></info>
</alert>"#;

        assert!(parse_capcp_alert(xml, "naad://test", &languages(&["en"])).is_err());
    }

    #[test]
    fn parse_capcp_alert_maps_capcp_event_vocabulary() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-EVENT-MAP</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Blizzard</event>
    <eventCode>
      <valueName>profile:CAP-CP:Event:0.4</valueName>
      <value>blizzard</value>
    </eventCode>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(parsed.alert.event_code, "BZW");
        assert_eq!(parsed.alert.originator_code, "WXR");
    }

    #[test]
    fn parse_capcp_alert_falls_back_to_cem_for_unknown_events() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-UNKNOWN-EVENT</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Other</category>
    <event>Something New</event>
    <eventCode>
      <valueName>profile:CAP-CP:Event:0.4</valueName>
      <value>somethingBrandNew</value>
    </eventCode>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(parsed.alert.event_code, "CEM");
        assert_eq!(parsed.alert.originator_code, "CIV");
        assert_eq!(parsed.alert.fips, vec!["000000".to_string()]);
    }

    #[test]
    fn parse_capcp_alert_honours_explicit_same_event_codes() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-SAME-EVENT</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Blizzard</event>
    <eventCode>
      <valueName>SAME</valueName>
      <value>WSW</value>
    </eventCode>
    <eventCode>
      <valueName>profile:CAP-CP:Event:0.4</valueName>
      <value>blizzard</value>
    </eventCode>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(parsed.alert.event_code, "WSW");
    }

    #[test]
    fn parse_capcp_alert_falls_back_to_same_geocodes() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-SAME-GEOCODE</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Blizzard</event>
    <area>
      <areaDesc>Somewhere</areaDesc>
      <geocode><valueName>SAME</valueName><value>048100,048200</value></geocode>
    </area>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(
            parsed.alert.fips,
            vec!["048100".to_string(), "048200".to_string()]
        );
    }

    #[test]
    fn capcp_location_geocodes_resolve_against_canadian_same_data() {
        assert_eq!(capcp_location_to_same("001110").as_deref(), Some("001110"));
        assert_eq!(capcp_location_to_same("01110").as_deref(), Some("001110"));
        assert!(capcp_location_to_same("3520005").is_none());
        assert!(capcp_location_to_same("099999").is_none());
        assert!(capcp_location_to_same("not-a-code").is_none());
    }

    #[test]
    fn clc_names_cover_codes_missing_from_same_ca_json() {
        // Both appeared as raw "FIPS Code ..." in generated EAS text before this table existed.
        assert_eq!(clc_location_name("094720"), Some("Colville Lake"));
        assert_eq!(clc_location_name("094300"), Some("Region 4"));
        // Broadest scale wins, so a marine zone is named as the zone, not a town beside it.
        assert_eq!(clc_location_name("001100"), Some("Pacific Waters"));
        assert_eq!(clc_location_name("002200"), Some("Great Lakes"));
        assert!(clc_location_name("999999").is_none());
    }

    #[test]
    fn every_clc_code_in_the_geocode_table_can_be_named() {
        let same_ca = &SAME_CA.same;
        let mut unnamed = Vec::new();

        for codes in GEO_TO_CLC.values() {
            for code in codes {
                let in_same_ca = same_ca.contains_key(&code[1..]);
                if !in_same_ca && clc_location_name(code).is_none() {
                    unnamed.push(code.clone());
                }
            }
        }

        assert!(
            unnamed.is_empty(),
            "these CLC codes would render as raw FIPS codes: {unnamed:?}"
        );
    }

    #[test]
    fn geocode_to_clc_translates_sgc_codes() {
        assert_eq!(
            geocode_to_clc("3520005"),
            Some(["043100".to_string()].as_slice())
        );
        assert_eq!(
            geocode_to_clc("3520"),
            Some(["043100".to_string()].as_slice())
        );
        assert_eq!(
            geocode_to_clc("35"),
            Some(["040000".to_string()].as_slice())
        );
        assert_eq!(
            geocode_to_clc("00111"),
            Some(["001110".to_string()].as_slice())
        );
        assert_eq!(geocode_to_clc("0"), Some(["000000".to_string()].as_slice()));
        assert!(geocode_to_clc("9999999").is_none());
        assert!(geocode_to_clc("043100").is_none());
    }

    #[test]
    fn geocode_to_clc_splits_multi_zone_entries() {
        assert_eq!(
            geocode_to_clc("5901"),
            Some(
                [
                    "084300".to_string(),
                    "084400".to_string(),
                    "084500".to_string(),
                    "085600".to_string()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn geocode_to_clc_keeps_the_first_row_for_duplicated_geocodes() {
        let codes = geocode_to_clc("4812004").expect("duplicated geocode");
        assert_eq!(
            codes,
            [
                "075141".to_string(),
                "075142".to_string(),
                "075143".to_string(),
                "075144".to_string(),
                "075145".to_string(),
                "075146".to_string()
            ]
        );
        assert!(!codes.contains(&"070000".to_string()));
    }

    #[test]
    fn geocode_to_clc_by_prefix_widens_unmapped_codes() {
        assert_eq!(
            geocode_to_clc_by_prefix("3520999"),
            Some(["043100".to_string()].as_slice())
        );
        assert!(geocode_to_clc_by_prefix("35").is_none());
    }

    #[test]
    fn geo_to_clc_table_is_well_formed() {
        assert!(
            GEO_TO_CLC.len() > 5_000,
            "expected the full CAP-CP geocode table, got {} entries",
            GEO_TO_CLC.len()
        );

        for (geocode, codes) in GEO_TO_CLC.iter() {
            assert!(
                geocode.bytes().all(|byte| byte.is_ascii_digit()),
                "non-numeric geocode key: {geocode}"
            );
            assert!(!codes.is_empty(), "{geocode} mapped to no CLC codes");
            for code in codes {
                assert_eq!(
                    code.len(),
                    6,
                    "{geocode} mapped to malformed CLC code {code}"
                );
                assert!(
                    code.bytes().all(|byte| byte.is_ascii_digit()),
                    "{geocode} mapped to non-numeric CLC code {code}"
                );
            }
        }
    }

    #[test]
    fn capcp_alerts_without_newly_active_areas_use_the_geocode_table() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-GEOCODE-TABLE</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <code>profile:CAP-CP:0.4</code>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Tornado</event>
    <eventCode>
      <valueName>profile:CAP-CP:Event:0.4</valueName>
      <value>tornado</value>
    </eventCode>
    <area>
      <areaDesc>City of Toronto</areaDesc>
      <geocode>
        <valueName>profile:CAP-CP:Location:0.3</valueName>
        <value>3520005</value>
      </geocode>
    </area>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(parsed.alert.fips, vec!["043100".to_string()]);
    }

    #[test]
    fn capcp_titles_come_from_canadian_same_data() {
        assert_eq!(capcp_event_title("TOR").as_deref(), Some("Tornado Warning"));
        assert_eq!(
            capcp_originator_name("WXR").as_deref(),
            Some("Environment Canada")
        );
        assert!(capcp_event_title("ZZZ").is_none());
    }

    #[test]
    fn geocode_filter_matches_exact_and_wildcard_prefixes() {
        let geocodes = vec!["3520005".to_string()];

        assert!(geocode_filter_matches(&geocodes, &[]));
        assert!(geocode_filter_matches(&geocodes, &["3520005".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["35*".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["352*".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["3520*".to_string()]));
        assert!(!geocode_filter_matches(&geocodes, &["24*".to_string()]));
        assert!(!geocode_filter_matches(&geocodes, &["35".to_string()]));
    }

    #[test]
    fn geocode_filter_passes_alerts_without_capcp_geocodes() {
        assert!(geocode_filter_matches(&[], &["35*".to_string()]));
    }

    #[test]
    fn geocode_filter_treats_a_bare_star_as_everything() {
        let star = vec!["*".to_string()];

        assert!(geocode_filter_matches(&["3520005".to_string()], &star));
        assert!(geocode_filter_matches(&["0".to_string()], &star));
        assert!(geocode_filter_matches(&[], &star));
        assert!(geocode_filter_matches(
            &["9999999".to_string()],
            &["24*".to_string(), "*".to_string()]
        ));
    }

    #[test]
    fn geocode_filter_accepts_prefix_wildcards_of_any_width() {
        let geocodes = vec!["3520005".to_string()];

        assert!(geocode_filter_matches(&geocodes, &["3*".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["35200*".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["352000*".to_string()]));
        assert!(geocode_filter_matches(&geocodes, &["3520005*".to_string()]));
        assert!(!geocode_filter_matches(&geocodes, &["36*".to_string()]));
    }

    fn immediate_alert_xml(broadcast: &str, wireless: &str) -> String {
        format!(
            r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-IMMEDIATE</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <code>profile:CAP-CP:0.4</code>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Tornado</event>
    <expires>2035-01-01T00:00:00-00:00</expires>
    <eventCode>
      <valueName>profile:CAP-CP:Event:0.4</valueName>
      <value>tornado</value>
    </eventCode>
    <parameter>
      <valueName>layer:SOREM:1.0:Broadcast_Immediately</valueName>
      <value>{broadcast}</value>
    </parameter>
    <parameter>
      <valueName>layer:SOREM:2.0:WirelessImmediate</valueName>
      <value>{wireless}</value>
    </parameter>
  </info>
</alert>"#
        )
    }

    fn immediate_outcome(broadcast: &str, wireless: &str, require: bool) -> CapCpFilterOutcome {
        let xml = immediate_alert_xml(broadcast, wireless);
        let parsed =
            parse_capcp_alert(&xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        evaluate_capcp_filter(&parsed, &[], require)
    }

    #[test]
    fn either_immediate_flag_admits_an_alert() {
        for (broadcast, wireless) in [("Yes", "No"), ("No", "Yes"), ("Yes", "Yes")] {
            let outcome = immediate_outcome(broadcast, wireless, true);
            assert!(
                outcome.immediate_matched && outcome.passed(),
                "broadcast={broadcast} wireless={wireless} should have passed"
            );
        }
    }

    #[test]
    fn neither_immediate_flag_rejects_an_alert() {
        let outcome = immediate_outcome("No", "No", true);
        assert!(!outcome.immediate_matched);
        assert!(!outcome.passed());
        // The other gates are unaffected, so the log names the real reason.
        assert!(outcome.language_matched);
        assert!(outcome.geocode_matched);
        assert!(!outcome.expired);
    }

    #[test]
    fn a_missing_immediate_parameter_counts_as_no() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-NO-PARAMS</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Tornado</event>
    <expires>2035-01-01T00:00:00-00:00</expires>
  </info>
</alert>"#;
        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert!(!parsed.broadcast_immediately);
        assert!(!parsed.wireless_immediate);
        assert!(!evaluate_capcp_filter(&parsed, &[], true).passed());
        // With the requirement off it goes through as before.
        assert!(evaluate_capcp_filter(&parsed, &[], false).passed());
    }

    #[test]
    fn immediate_value_names_are_matched_despite_spelling_drift() {
        // The profile is inconsistent about punctuation and the "-ly" suffix, so matching is
        // done on a normalized substring.
        for name in [
            // The name observed on live NAAD traffic.
            "layer:SOREM:2.0:WirelessImmediate",
            // Tolerated variants, so a version bump or spelling change cannot turn the gate off.
            "layer:SOREM:1.0:WirelessImmediate",
            "layer:SOREM:3.0:Wireless_Immediate",
            "layer:SOREM:2.0:Wireless-Immediately",
            "LAYER:sorem:2.0:wirelessimmediate",
        ] {
            let xml = format!(
                r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-NAME-DRIFT</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Tornado</event>
    <expires>2035-01-01T00:00:00-00:00</expires>
    <parameter><valueName>{name}</valueName><value>Yes</value></parameter>
  </info>
</alert>"#
            );
            let parsed =
                parse_capcp_alert(&xml, "naad://test", &languages(&["en"])).expect("parsed alert");
            assert!(
                parsed.wireless_immediate,
                "{name} was not recognized as the wireless flag"
            );
        }
    }

    #[test]
    fn the_observed_sorem_value_names_match_their_keys() {
        // Exactly as they appear on live NAAD traffic, including the differing layer versions.
        let broadcast = normalize_value_name("layer:SOREM:1.0:Broadcast_Immediately");
        let wireless = normalize_value_name("layer:SOREM:2.0:WirelessImmediate");

        assert_eq!(broadcast, "layersorem10broadcastimmediately");
        assert_eq!(wireless, "layersorem20wirelessimmediate");
        assert!(broadcast.contains(SOREM_BROADCAST_IMMEDIATE_KEY));
        assert!(wireless.contains(SOREM_WIRELESS_IMMEDIATE_KEY));

        // A neighbouring EC flag with a similar name must not be picked up by either key.
        let intrusive = normalize_value_name("layer:EC-MSC-SMC:1.0:Broadcast_Intrusive");
        assert!(!intrusive.contains(SOREM_BROADCAST_IMMEDIATE_KEY));
        assert!(!intrusive.contains(SOREM_WIRELESS_IMMEDIATE_KEY));
    }

    #[test]
    fn observed_no_values_are_read_as_not_immediate() {
        // Every flag on the sampled traffic read "No" with a capital N.
        let outcome = immediate_outcome("No", "No", true);
        assert!(!outcome.broadcast_immediately);
        assert!(!outcome.wireless_immediate);
        assert!(!outcome.immediate_matched);
    }

    #[test]
    fn evaluate_capcp_filter_reports_each_gate() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://test#TEST-CAPCP-001",
            &languages(&["en"]),
        )
        .expect("parsed alert");

        let matched = evaluate_capcp_filter(&parsed, &["35*".to_string()], false);
        assert!(matched.language_matched);
        assert!(matched.geocode_matched);
        assert!(matched.broadcast_immediately);
        assert!(!matched.expired);
        assert!(matched.passed());

        let filtered_out = evaluate_capcp_filter(&parsed, &["24*".to_string()], false);
        assert!(!filtered_out.geocode_matched);
        assert!(!filtered_out.passed());
    }

    #[test]
    fn expired_alerts_do_not_pass_the_filter() {
        let xml =
            valid_alert_xml().replace("2035-09-09T18:00:00-00:00", "2020-01-01T00:00:00-00:00");
        let parsed =
            parse_capcp_alert(&xml, "naad://test", &languages(&["en"])).expect("parsed alert");

        let outcome = evaluate_capcp_filter(&parsed, &[], false);
        assert!(outcome.expired);
        assert!(!outcome.passed());
    }

    #[test]
    fn capcp_documents_are_identified_by_profile_code() {
        assert!(is_capcp_document(valid_alert_xml()));
        assert!(!is_capcp_document(
            r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2"><code>IPAWSv1.0</code></alert>"#
        ));
    }

    #[test]
    fn capcp_headers_carry_the_naad_source_marker() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
            &languages(&["en"]),
        )
        .expect("parsed alert");

        let header = build_capcp_raw_header(&parsed.alert);
        assert!(header.starts_with("ZCZC-WXR-TOR-043100-046100+"));
        assert!(header.ends_with("-NAADSCAP-"));
    }

    #[test]
    fn capcp_headers_humanize_against_canadian_same_data() {
        let parsed = parse_capcp_alert(
            valid_alert_xml(),
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001",
            &languages(&["en"]),
        )
        .expect("parsed alert");

        // Pinned rather than read from the ambient config: this is about Canadian SAME data
        // resolving to Canadian names, and the wording asserted below is the default mode's.
        let header = build_capcp_raw_header(&parsed.alert);
        let humanized = crate::e2t_ng::E2T(&header, "default", true, Some("UTC"));

        assert_ne!(humanized, "Invalid EAS header format");
        assert!(
            humanized.contains("City of Toronto"),
            "expected Canadian location name, got: {humanized}"
        );
        assert!(
            humanized.contains("City of Hamilton"),
            "expected Canadian location name, got: {humanized}"
        );
        assert!(
            humanized.contains("Environment Canada"),
            "expected Canadian originator name, got: {humanized}"
        );
        assert!(
            humanized.contains("Tornado Warning"),
            "expected Canadian event name, got: {humanized}"
        );
    }

    #[test]
    fn capcp_source_url_embeds_the_identifier() {
        assert_eq!(
            capcp_source_url("streaming1.naad-adna.pelmorex.com:8080", valid_alert_xml()),
            "naad://streaming1.naad-adna.pelmorex.com:8080#TEST-CAPCP-001"
        );
        assert_eq!(
            capcp_source_url("streaming2.naad-adna.pelmorex.com:8080", "not xml"),
            "naad://streaming2.naad-adna.pelmorex.com:8080"
        );
    }

    #[test]
    fn collapse_broadcast_text_cleans_reference_artifacts() {
        assert_eq!(
            collapse_broadcast_text("Headline. ### Body   text.. More\ntext"),
            "Headline. Body text. More text"
        );
    }

    #[test]
    fn build_broadcast_text_stitches_cap_fields_without_broadcast_text() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>TEST-STITCH</identifier>
  <sender>test@example.ca</sender>
  <msgType>Alert</msgType>
  <info>
    <language>en-CA</language>
    <category>Met</category>
    <event>Blizzard</event>
    <headline>Blizzard warning</headline>
    <description>Heavy snow expected.</description>
    <instruction>Avoid travel.</instruction>
    <area><areaDesc>Winnipeg</areaDesc></area>
  </info>
</alert>"#;

        let parsed =
            parse_capcp_alert(xml, "naad://test", &languages(&["en"])).expect("parsed alert");
        assert_eq!(
            parsed.alert.description_raw,
            "Blizzard warning. Winnipeg. Heavy snow expected. Avoid travel."
        );
    }

    #[test]
    fn event_map_targets_are_valid_same_codes() {
        for (capcp_event, same_code) in CAPCP_EVENT_TO_SAME.entries() {
            assert_eq!(
                same_code.len(),
                3,
                "{capcp_event} maps to a non-3-character SAME code"
            );
            assert_eq!(
                normalize_event_code(same_code),
                *same_code,
                "{capcp_event} maps to a SAME code that does not normalize cleanly"
            );
        }
    }
}
