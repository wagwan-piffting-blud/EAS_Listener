use crate::filter::{self, FilterRule};
use anyhow::{anyhow, Context, Result};
use chrono_tz::Tz;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub apprise_config_path: String,
    pub should_relay_icecast: bool,
    pub icecast_relay: String,
    pub dasdec_url: String,
    pub should_relay_dasdec: bool,
    pub icecast_intro: PathBuf,
    pub icecast_outro: PathBuf,
    pub should_relay: bool,
    pub should_log_all_alerts: bool,
    pub icecast_stream_urls: Vec<String>,
    pub shared_state_dir: PathBuf,
    pub alert_log_file: String,
    pub dedicated_alert_log_file: PathBuf,
    pub timezone: Tz,
    pub watched_fips: HashSet<String>,
    pub recording_dir: PathBuf,
    pub monitoring_bind_addr: SocketAddr,
    pub monitoring_max_log_entries: usize,
    pub monitoring_activity_window_secs: u64,
    pub use_reverse_proxy: bool,
    pub monitoring_bind_port: u16,
    pub ws_reverse_proxy_url: String,
    pub dashboard_username: String,
    pub dashboard_password: String,
    pub eas_relay_name: String,
    pub reverse_proxy_url: String,
    pub local_deeplink_host: String,
    pub web_server_port: String,
    pub filters: Vec<FilterRule>,
    pub log_level: String,
}

impl Config {
    pub fn from_config_json(config_file: &str) -> Result<Self> {
        let config_data = std::fs::read_to_string(config_file)
            .with_context(|| format!("Failed to read config file: {}", config_file))?;
        let config_json: serde_json::Value = serde_json::from_str(&config_data)
            .with_context(|| format!("Failed to parse config file: {}", config_file))?;

        let shared_dir: PathBuf = config_json
            .get("SHARED_STATE_DIR")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("SHARED_STATE_DIR must be set in your config.json file"))?;

        let log_filename = config_json
            .get("DEDICATED_ALERT_LOG_FILE")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("DEDICATED_ALERT_LOG_FILE must be set in your config.json file")
            })?;

        let should_log_all_alerts = config_json
            .get("SHOULD_LOG_ALL_ALERTS")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                anyhow!(
                    "SHOULD_LOG_ALL_ALERTS must be either true or false in your config.json file"
                )
            })?;

        let should_relay = config_json
            .get("SHOULD_RELAY")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                anyhow!("SHOULD_RELAY must be either true or false in your config.json file")
            })?;

        let should_relay_icecast = config_json
            .get("SHOULD_RELAY_ICECAST")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                anyhow!(
                    "SHOULD_RELAY_ICECAST must be either true or false in your config.json file"
                )
            })?;

        let icecast_relay = config_json
            .get("ICECAST_RELAY")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut icecast_intro: PathBuf = PathBuf::new();
        let mut icecast_outro: PathBuf = PathBuf::new();

        let should_relay_dasdec = config_json
            .get("SHOULD_RELAY_DASDEC")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                anyhow!("SHOULD_RELAY_DASDEC must be either true or false in your config.json file")
            })?;

        let dasdec_url = config_json
            .get("DASDEC_URL")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if should_relay && should_relay_icecast {
            if icecast_relay.is_empty() {
                return Err(anyhow!(
                    "ICECAST_RELAY must be set if SHOULD_RELAY and SHOULD_RELAY_ICECAST are true"
                ));
            }

            icecast_intro = config_json
                .get("ICECAST_INTRO")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| "".into());

            icecast_outro = config_json
                .get("ICECAST_OUTRO")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| "".into());
        }

        let tz_str = config_json
            .get("TZ")
            .and_then(|v| v.as_str())
            .unwrap_or("UTC");
        let timezone = tz_str.parse().unwrap_or(Tz::UTC);

        let watched_fips: HashSet<String> = config_json
            .get("WATCHED_FIPS")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        let recording_dir = shared_dir.join(
            config_json
                .get("RECORDING_DIR")
                .and_then(|v| v.as_str())
                .unwrap_or("recordings"),
        );

        let icecast_stream_urls: Vec<String> = config_json
            .get("ICECAST_STREAM_URL_ARRAY")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                anyhow!("ICECAST_STREAM_URL_ARRAY must be set in your config.json file")
            })?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();

        if icecast_stream_urls.is_empty() {
            return Err(anyhow!(
                "ICECAST_STREAM_URL_ARRAY must contain at least one stream URL"
            ));
        }

        let monitoring_bind_addr: SocketAddr = config_json
            .get("MONITORING_BIND_ADDR")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("MONITORING_BIND_ADDR must be set in your config.json file"))?
            .parse()
            .with_context(|| "MONITORING_BIND_ADDR must be a valid socket address")?;

        let monitoring_max_log_entries = config_json
            .get("MONITORING_MAX_LOGS")
            .and_then(|v| v.as_u64())
            .unwrap_or(500) as usize;

        let monitoring_activity_window_secs = config_json
            .get("MONITORING_ACTIVITY_WINDOW_SECS")
            .and_then(|v| v.as_u64())
            .unwrap_or(45)
            .max(1);

        let use_reverse_proxy: bool = config_json
            .get("USE_REVERSE_PROXY")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                anyhow!("USE_REVERSE_PROXY must be either true or false in your config.json file")
            })?;

        let alert_log_file = config_json
            .get("ALERT_LOG_FILE")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("ALERT_LOG_FILE must be set in your config.json file"))?
            .to_string();

        let apprise_config_path = config_json
            .get("APPRISE_CONFIG_PATH")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("APPRISE_CONFIG_PATH must be set in your config.json file"))?
            .to_string();

        let monitoring_bind_port = config_json
            .get("MONITORING_BIND_PORT")
            .and_then(|v| v.as_u64())
            .unwrap_or(8080) as u16;

        let ws_reverse_proxy_url = config_json
            .get("WS_REVERSE_PROXY_URL")
            .and_then(|v| v.as_str())
            .unwrap_or("localhost")
            .to_string();

        let dashboard_username = config_json
            .get("DASHBOARD_USERNAME")
            .and_then(|v| v.as_str())
            .unwrap_or("admin")
            .to_string();

        let dashboard_password = config_json
            .get("DASHBOARD_PASSWORD")
            .and_then(|v| v.as_str())
            .unwrap_or("password")
            .to_string();

        let eas_relay_name = config_json
            .get("EAS_RELAY_NAME")
            .and_then(|v| v.as_str())
            .unwrap_or("WAGSENDC")
            .to_string();

        let reverse_proxy_url = config_json
            .get("REVERSE_PROXY_URL")
            .and_then(|v| v.as_str())
            .unwrap_or("localhost")
            .to_string();

        let local_deeplink_host = std::env::var("LOCAL_DEEPLINK_HOST")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                config_json
                    .get("LOCAL_DEEPLINK_HOST")
                    .and_then(|v| v.as_str())
                    .unwrap_or("auto")
                    .trim()
                    .to_string()
            });

        let web_server_port = config_json
            .get("WEB_SERVER_PORT")
            .and_then(|v| v.as_str())
            .unwrap_or("3010")
            .to_string();

        let log_level = config_json
            .get("RUST_LOG")
            .and_then(|v| v.as_str())
            .unwrap_or("INFO")
            .to_string();

        let filters = filter::parse_filters(&config_json);

        Ok(Self {
            icecast_stream_urls,
            apprise_config_path,
            should_relay_icecast,
            icecast_relay,
            icecast_intro,
            icecast_outro,
            should_relay,
            should_log_all_alerts,
            should_relay_dasdec,
            dasdec_url,
            shared_state_dir: shared_dir.clone(),
            alert_log_file,
            dedicated_alert_log_file: shared_dir.join(log_filename),
            timezone,
            watched_fips,
            recording_dir,
            monitoring_bind_addr,
            monitoring_max_log_entries,
            monitoring_activity_window_secs,
            use_reverse_proxy,
            monitoring_bind_port,
            ws_reverse_proxy_url,
            dashboard_username,
            dashboard_password,
            eas_relay_name,
            reverse_proxy_url,
            local_deeplink_host,
            web_server_port,
            filters,
            log_level,
        })
    }
}
