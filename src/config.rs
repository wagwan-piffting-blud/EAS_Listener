use crate::filter::{self, FilterRule};
use anyhow::{anyhow, Context, Result};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct CapEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub apprise_config_path: String,
    pub should_relay_icecast: bool,
    pub icecast_relay: String,
    pub dasdec_url: String,
    pub should_relay_dasdec: bool,
    pub use_icecast_intro_outro: bool,
    pub icecast_intro: PathBuf,
    pub icecast_outro: PathBuf,
    pub should_relay: bool,
    pub process_cap_alerts: bool,
    pub cap_endpoints: Vec<CapEndpoint>,
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
    pub preferred_senderid: String,
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

fn optional_string(config_json: &Value, key: &str) -> Result<Option<String>> {
    match config_json.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| anyhow!("{key} must be a string in your config.json file")),
    }
}

fn optional_bool(config_json: &Value, key: &str) -> Result<Option<bool>> {
    match config_json.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| anyhow!("{key} must be either true or false in your config.json file")),
    }
}

fn optional_u64(config_json: &Value, key: &str) -> Result<Option<u64>> {
    match config_json.get(key) {
        None => Ok(None),
        Some(value) => {
            if let Some(number) = value.as_u64() {
                return Ok(Some(number));
            }

            if let Some(text) = value.as_str() {
                return text
                    .trim()
                    .parse::<u64>()
                    .map(Some)
                    .with_context(|| format!("{key} must be a valid integer"));
            }

            Err(anyhow!(
                "{key} must be a number or numeric string in your config.json file"
            ))
        }
    }
}

fn optional_u16(config_json: &Value, key: &str) -> Result<Option<u16>> {
    let Some(value) = optional_u64(config_json, key)? else {
        return Ok(None);
    };

    let converted = u16::try_from(value)
        .with_context(|| format!("{key} must be between 0 and {}", u16::MAX))?;
    Ok(Some(converted))
}

impl Config {
    pub fn safe_internal_defaults() -> Self {
        let shared_dir = std::env::var("SHARED_STATE_DIR")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("eas-listener"));

        let monitoring_bind_addr = std::env::var("MONITORING_BIND_ADDR")
            .ok()
            .and_then(|value| value.trim().parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8080)));

        let monitoring_bind_port = std::env::var("MONITORING_BIND_PORT")
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok())
            .unwrap_or_else(|| monitoring_bind_addr.port());

        let local_deeplink_host = std::env::var("LOCAL_DEEPLINK_HOST")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "auto".to_string());

        let log_level = std::env::var("RUST_LOG")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "INFO".to_string());

        Self {
            apprise_config_path: "/app/apprise.yml".to_string(),
            should_relay_icecast: false,
            icecast_relay: String::new(),
            dasdec_url: String::new(),
            should_relay_dasdec: false,
            use_icecast_intro_outro: false,
            icecast_intro: PathBuf::new(),
            icecast_outro: PathBuf::new(),
            should_relay: false,
            process_cap_alerts: false,
            cap_endpoints: Vec::new(),
            should_log_all_alerts: false,
            icecast_stream_urls: vec!["https://wxr.gwes-cdn.net/KIH61".to_string()],
            shared_state_dir: shared_dir.clone(),
            alert_log_file: "alerts.log".to_string(),
            dedicated_alert_log_file: shared_dir.join("dedicated-alerts.log"),
            timezone: Tz::UTC,
            watched_fips: HashSet::new(),
            recording_dir: shared_dir.join("recordings"),
            monitoring_bind_addr,
            monitoring_max_log_entries: 500,
            monitoring_activity_window_secs: 45,
            use_reverse_proxy: false,
            preferred_senderid: String::new(),
            monitoring_bind_port,
            ws_reverse_proxy_url: "localhost".to_string(),
            dashboard_username: "admin".to_string(),
            dashboard_password: "password".to_string(),
            eas_relay_name: "EAS Listener".to_string(),
            reverse_proxy_url: "localhost".to_string(),
            local_deeplink_host,
            web_server_port: "3010".to_string(),
            filters: Vec::new(),
            log_level,
        }
    }

    pub fn from_config_json(config_file: &str) -> Result<Self> {
        let config_data = std::fs::read_to_string(config_file)
            .with_context(|| format!("Failed to read config file: {}", config_file))?;
        let config_json: Value = serde_json::from_str(&config_data)
            .with_context(|| format!("Failed to parse config file: {}", config_file))?;
        let mut merged = Self::safe_internal_defaults();

        let mut shared_dir_overridden = false;
        if let Some(value) = optional_string(&config_json, "SHARED_STATE_DIR")? {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("SHARED_STATE_DIR cannot be empty in your config.json file"));
            }
            merged.shared_state_dir = PathBuf::from(trimmed);
            shared_dir_overridden = true;
        }

        let dedicated_log_name = optional_string(&config_json, "DEDICATED_ALERT_LOG_FILE")?
            .and_then(|value| {
                let trimmed = value.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
            .or_else(|| {
                merged
                    .dedicated_alert_log_file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.to_string())
            })
            .unwrap_or_else(|| "dedicated-alerts.log".to_string());
        merged.dedicated_alert_log_file = merged.shared_state_dir.join(dedicated_log_name);

        if let Some(value) = optional_string(&config_json, "RECORDING_DIR")? {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(anyhow!("RECORDING_DIR cannot be empty in your config.json file"));
            }
            merged.recording_dir = merged.shared_state_dir.join(trimmed);
        } else if shared_dir_overridden {
            merged.recording_dir = merged.shared_state_dir.join("recordings");
        }

        if let Some(value) = optional_bool(&config_json, "SHOULD_LOG_ALL_ALERTS")? {
            merged.should_log_all_alerts = value;
        }
        if let Some(value) = optional_bool(&config_json, "SHOULD_RELAY")? {
            merged.should_relay = value;
        }
        if let Some(value) = optional_bool(&config_json, "SHOULD_RELAY_ICECAST")? {
            merged.should_relay_icecast = value;
        }
        if let Some(value) = optional_bool(&config_json, "SHOULD_RELAY_DASDEC")? {
            merged.should_relay_dasdec = value;
        }
        if let Some(value) = optional_bool(&config_json, "USE_ICECAST_INTRO_OUTRO")? {
            merged.use_icecast_intro_outro = value;
        }
        if let Some(value) = optional_bool(&config_json, "PROCESS_CAP_ALERTS")? {
            merged.process_cap_alerts = value;
        }
        if let Some(value) = optional_bool(&config_json, "USE_REVERSE_PROXY")? {
            merged.use_reverse_proxy = value;
        }

        if let Some(value) = optional_string(&config_json, "ICECAST_RELAY")? {
            merged.icecast_relay = value;
        }
        if let Some(value) = optional_string(&config_json, "DASDEC_URL")? {
            merged.dasdec_url = value;
        }
        if let Some(value) = optional_string(&config_json, "ICECAST_INTRO")? {
            merged.icecast_intro = PathBuf::from(value);
        }
        if let Some(value) = optional_string(&config_json, "ICECAST_OUTRO")? {
            merged.icecast_outro = PathBuf::from(value);
        }
        if let Some(value) = optional_string(&config_json, "ALERT_LOG_FILE")? {
            merged.alert_log_file = value;
        }
        if let Some(value) = optional_string(&config_json, "APPRISE_CONFIG_PATH")? {
            merged.apprise_config_path = value;
        }
        if let Some(value) = optional_string(&config_json, "WS_REVERSE_PROXY_URL")? {
            merged.ws_reverse_proxy_url = value;
        }
        if let Some(value) = optional_string(&config_json, "DASHBOARD_USERNAME")? {
            merged.dashboard_username = value;
        }
        if let Some(value) = optional_string(&config_json, "DASHBOARD_PASSWORD")? {
            merged.dashboard_password = value;
        }
        if let Some(value) = optional_string(&config_json, "EAS_RELAY_NAME")? {
            merged.eas_relay_name = value;
        }
        if let Some(value) = optional_string(&config_json, "REVERSE_PROXY_URL")? {
            merged.reverse_proxy_url = value;
        }
        if let Some(value) = optional_string(&config_json, "PREFERRED_SENDERID")? {
            merged.preferred_senderid = value;
        }
        if let Some(value) = optional_string(&config_json, "WEB_SERVER_PORT")? {
            merged.web_server_port = value;
        }
        if let Some(value) = optional_string(&config_json, "RUST_LOG")? {
            merged.log_level = value;
        }

        if let Some(value) = optional_string(&config_json, "TZ")? {
            merged.timezone = value.parse().unwrap_or(merged.timezone);
        }
        if let Some(value) = optional_string(&config_json, "WATCHED_FIPS")? {
            merged.watched_fips = value
                .split(',')
                .filter_map(|part| {
                    let trimmed = part.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                })
                .collect::<HashSet<String>>();
        }

        let mut monitoring_bind_addr_overridden = false;
        if let Some(value) = optional_string(&config_json, "MONITORING_BIND_ADDR")? {
            merged.monitoring_bind_addr = value
                .parse::<SocketAddr>()
                .with_context(|| "MONITORING_BIND_ADDR must be a valid socket address")?;
            monitoring_bind_addr_overridden = true;
        }

        if let Some(value) = optional_u16(&config_json, "MONITORING_BIND_PORT")? {
            merged.monitoring_bind_port = value;
        } else if monitoring_bind_addr_overridden {
            merged.monitoring_bind_port = merged.monitoring_bind_addr.port();
        }

        if let Some(value) = optional_u64(&config_json, "MONITORING_MAX_LOGS")? {
            merged.monitoring_max_log_entries = value as usize;
        }
        if let Some(value) = optional_u64(&config_json, "MONITORING_ACTIVITY_WINDOW_SECS")? {
            merged.monitoring_activity_window_secs = value.max(1);
        }

        if let Some(cap_entries) = config_json.get("CAP_ENDPOINTS") {
            let Some(entries) = cap_entries.as_array() else {
                return Err(anyhow!("CAP_ENDPOINTS must be an array in your config.json file"));
            };

            merged.cap_endpoints = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .as_str()
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .map(|url| CapEndpoint {
                            name: None,
                            url: url.to_string(),
                        })
                        .or_else(|| {
                            let url = entry
                                .get("url")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|url| !url.is_empty())?;
                            let name = entry
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|name| !name.is_empty())
                                .map(str::to_string);
                            Some(CapEndpoint {
                                name,
                                url: url.to_string(),
                            })
                        })
                })
                .collect();
        }

        if let Some(stream_entries) = config_json.get("ICECAST_STREAM_URL_ARRAY") {
            let Some(entries) = stream_entries.as_array() else {
                return Err(anyhow!(
                    "ICECAST_STREAM_URL_ARRAY must be an array in your config.json file"
                ));
            };

            let parsed_streams: Vec<String> = entries
                .iter()
                .filter_map(|entry| {
                    entry.as_str().and_then(|url| {
                        let trimmed = url.trim();
                        (!trimmed.is_empty()).then(|| trimmed.to_string())
                    })
                })
                .collect();

            if parsed_streams.is_empty() {
                return Err(anyhow!(
                    "ICECAST_STREAM_URL_ARRAY must contain at least one stream URL"
                ));
            }

            merged.icecast_stream_urls = parsed_streams;
        }

        if merged.should_relay && merged.should_relay_icecast && merged.icecast_relay.is_empty() {
            return Err(anyhow!(
                "ICECAST_RELAY must be set if SHOULD_RELAY and SHOULD_RELAY_ICECAST are true"
            ));
        }

        if merged.should_relay
            && merged.should_relay_icecast
            && merged.use_icecast_intro_outro
            && (merged.icecast_intro.as_os_str().is_empty()
                || merged.icecast_outro.as_os_str().is_empty())
        {
            return Err(anyhow!(
                "ICECAST_INTRO and ICECAST_OUTRO must be set if USE_ICECAST_INTRO_OUTRO is true in your config.json file"
            ));
        }

        if merged.process_cap_alerts && merged.cap_endpoints.is_empty() {
            return Err(anyhow!(
                "CAP_ENDPOINTS must contain at least one endpoint in your config.json file if PROCESS_CAP_ALERTS is true"
            ));
        }

        if let Some(env_local_host) = std::env::var("LOCAL_DEEPLINK_HOST")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            merged.local_deeplink_host = env_local_host;
        } else if let Some(value) = optional_string(&config_json, "LOCAL_DEEPLINK_HOST")? {
            merged.local_deeplink_host = value.trim().to_string();
        }

        merged.filters = filter::parse_filters(&config_json);

        Ok(merged)
    }
}
