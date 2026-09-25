use crate::filter::{self, FilterRule};
use anyhow::{anyhow, Context, Result};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize)]
pub struct CapEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFormat {
    Mp3,
    OggOpus,
}

impl RecordingFormat {
    pub fn extension(self) -> &'static str {
        match self {
            RecordingFormat::Mp3 => "mp3",
            RecordingFormat::OggOpus => "ogg",
        }
    }

    pub fn ffmpeg_codec_args(self) -> &'static [&'static str] {
        match self {
            RecordingFormat::Mp3 => &["-c:a", "libmp3lame", "-b:a", "128k", "-f", "mp3"],
            RecordingFormat::OggOpus => &[
                "-c:a", "libopus", "-b:a", "160k", "-vbr", "off", "-f", "ogg",
            ],
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mp3" => Some(RecordingFormat::Mp3),
            "ogg" | "opus" | "ogg-opus" | "oggopus" => Some(RecordingFormat::OggOpus),
            _ => None,
        }
    }
}

/// Declares every configuration knob exactly once.
///
/// A `plain` knob names its field, its type, the `Settings` accessor that reads it, its config.json
/// key and its default -- and from that one line the macro generates the struct field, the default
/// and the code that applies the setting. Adding a knob of this shape means adding one line here
/// and nothing else.
///
/// A `derived` knob only declares its field and default; it needs logic that does not reduce to
/// "read the key, assign it", and is handled by name in `apply_derived`. Its default may use
/// `shared`, the already-resolved state directory.
macro_rules! config_knobs {
    (
        // Named here so the `derived` defaults below can refer to it; macro hygiene means the
        // binding has to come from the invocation rather than from inside the macro.
        state_dir = $shared:ident;
        plain {
            $(
                $(#[$meta:meta])*
                $field:ident: $ty:ty = $kind:ident($key:literal), $default:expr;
            )*
        }
        derived {
            $(
                $(#[$dmeta:meta])*
                $dfield:ident: $dty:ty = $ddefault:expr;
            )*
        }
    ) => {
        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub struct Config {
            $( $(#[$meta])* pub $field: $ty, )*
            $( $(#[$dmeta])* pub $dfield: $dty, )*
        }

        impl Config {
            /// Compiled-in defaults, with no environment and no config.json consulted.
            fn baseline($shared: &Path) -> Self {
                let _ = $shared;
                Self {
                    $( $field: $default, )*
                    $( $dfield: $ddefault, )*
                }
            }

            fn apply_plain_knobs(&mut self, settings: &Settings) -> Result<()> {
                $(
                    if let Some(value) = settings.$kind($key)? {
                        self.$field = value;
                    }
                )*
                Ok(())
            }

            /// The keys the table above resolves, for tests that check both sources are honoured.
            #[cfg(test)]
            pub fn plain_knob_keys() -> &'static [&'static str] {
                &[ $($key),* ]
            }
        }
    };
}

config_knobs! {
    state_dir = shared;
    plain {
        apprise_config_path: String = string("APPRISE_CONFIG_PATH"),
            crate::paths::apprise_config().to_string_lossy().into_owned();
        should_relay: bool = bool("SHOULD_RELAY"), false;
        should_relay_icecast: bool = bool("SHOULD_RELAY_ICECAST"), false;
        should_relay_dasdec: bool = bool("SHOULD_RELAY_DASDEC"), false;
        icecast_relay: String = string("ICECAST_RELAY"), String::new();
        icecast_alert_stream_enabled: bool = bool("ICECAST_ALERT_STREAM_ENABLED"), false;
        /// The listener serves the alert stream on this port itself; no Icecast server is involved.
        icecast_alert_port: u16 = u16("ICECAST_ALERT_PORT"), 8000;
        icecast_alert_public_url: String = text("ICECAST_ALERT_PUBLIC_URL"), String::new();
        dasdec_url: String = string("DASDEC_URL"), String::new();
        use_icecast_intro_outro: bool = bool("USE_ICECAST_INTRO_OUTRO"), false;
        use_pre_post_roll_for_recordings: bool = bool("USE_PRE_POST_ROLL_FOR_RECORDINGS"), false;
        icecast_intro: PathBuf = loose_path("ICECAST_INTRO"), PathBuf::new();
        icecast_outro: PathBuf = loose_path("ICECAST_OUTRO"), PathBuf::new();
        emit_header_tones: bool = bool("EMIT_HEADER_TONES"), true;
        custom_header_audio: PathBuf = loose_path("CUSTOM_HEADER_AUDIO"), PathBuf::new();
        process_cap_alerts: bool = bool("PROCESS_CAP_ALERTS"), false;
        process_capcp_alerts: bool = bool("PROCESS_CAPCP_ALERTS"), false;
        capcp_stream_endpoints: Vec<String> = string_list("CAPCP_STREAM_ENDPOINTS"),
            crate::cap_cp::default_stream_endpoints();
        capcp_geocode_filter: Vec<String> = string_list("CAPCP_GEOCODE_FILTER"), Vec::new();
        capcp_require_immediate: bool = bool("CAPCP_REQUIRE_IMMEDIATE"), true;
        capcp_use_alert_ready_tone: bool = bool("CAPCP_USE_ALERT_READY_TONE"), false;
        should_log_all_alerts: bool = bool("SHOULD_LOG_ALL_ALERTS"), false;
        alert_log_file: String = string("ALERT_LOG_FILE"), "alerts.log".to_string();
        storage_saver_mode: bool = bool("STORAGE_SAVER_MODE"), false;
        monitoring_max_log_entries: usize = usize("MONITORING_MAX_LOGS"), 500;
        use_reverse_proxy: bool = bool("USE_REVERSE_PROXY"), false;
        preferred_senderid: String = string("PREFERRED_SENDERID"), String::new();
        ws_reverse_proxy_url: String = string("WS_REVERSE_PROXY_URL"), "localhost".to_string();
        reverse_proxy_url: String = string("REVERSE_PROXY_URL"), "localhost".to_string();
        dashboard_username: String = string("DASHBOARD_USERNAME"), "admin".to_string();
        dashboard_password: String = string("DASHBOARD_PASSWORD"), "password".to_string();
        eas_relay_name: String = string("EAS_RELAY_NAME"), "EAS Listener".to_string();
        local_deeplink_host: String = optional_text("LOCAL_DEEPLINK_HOST"), "auto".to_string();
        log_level: String = string("RUST_LOG"), "INFO".to_string();
        tts_engine: String = string("TTS_ENGINE"), "piper".to_string();
        tts_read_callsign: bool = bool("TTS_READ_CALLSIGN"), false;
        tts_builtin_replacements: bool = bool("TTS_BUILTIN_REPLACEMENTS"), true;
        /// Empty uses the voice tree built into loqdave, which is the only way it ships now.
        loq6_data_dir: PathBuf = loose_path("LOQ6_DATA_DIR"), PathBuf::new();
        spfy_voice_dir: PathBuf = path("SPFY_VOICE_DIR"), crate::paths::spfy_voice_dir();
    }
    derived {
        shared_state_dir: PathBuf = shared.to_path_buf();
        dedicated_alert_log_file: PathBuf = shared.join(DEFAULT_DEDICATED_ALERT_LOG_NAME);
        alert_database_file: PathBuf = shared.join(DEFAULT_ALERT_DATABASE_NAME);
        recording_dir: PathBuf = shared.join(DEFAULT_RECORDING_DIR_NAME);
        /// Fetched at runtime rather than shipped in the image, so these live on the state volume.
        cep6_voice_dir: PathBuf = shared.join("tts_voices").join("cep6");
        icecast_alert_mount: String = "/stream.ogg".to_string();
        icecast_stream_urls: Vec<String> = vec!["https://wxr.gwes-cdn.net/KIH61".to_string()];
        /// The streams that carry NOAA Weather Radio, and so are the only ones the 1050 Hz tone
        /// is looked for on. `None` means the key is absent, which keeps every stream watched --
        /// what the listener did before the key existed.
        nwr_stream_urls: Option<HashSet<String>> = None;
        cap_endpoints: Vec<CapEndpoint> = Vec::new();
        capcp_languages: Vec<String> = crate::cap_cp::default_languages();
        monitoring_activity_window_secs: u64 = 45;
        monitoring_bind_addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 8080));
        monitoring_bind_port: u16 = 8080;
        storage_saver_ext: RecordingFormat = RecordingFormat::Mp3;
        timezone: Tz = Tz::UTC;
        watched_fips: HashSet<String> = HashSet::new();
        endec_mode: String = "default".to_string();
        tts_model: Option<String> = None;
        filters: Vec<FilterRule> = Vec::new();
        /// Explicit paths to the third-party binaries, keyed by each component's config.json key.
        component_paths: HashMap<String, String> = HashMap::new();
    }
}

/// Relative to the state directory, as config.json writes them.
pub const DEFAULT_DEDICATED_ALERT_LOG_NAME: &str = "dedicated-alerts.log";
pub const DEFAULT_ALERT_DATABASE_NAME: &str = "alerts.db";
pub const DEFAULT_RECORDING_DIR_NAME: &str = "recordings";

/// `file` makes config.json outrank the environment. The Docker entrypoint sets it: it copies
/// config.json into the environment at boot for its own startup logic, and a copy that outranked
/// the file would pin every key to its boot-time value, so an edit followed by a reload changed
/// nothing.
const PRECEDENCE_VAR: &str = "EAS_CONFIG_PRECEDENCE";
/// Comma-separated keys whose environment value outranks config.json even under `file`
/// precedence -- the entrypoint lists the ones it overrides on purpose, such as a TTS engine the
/// image does not include.
const FORCED_KEYS_VAR: &str = "EAS_CONFIG_FORCED_KEYS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Precedence {
    Environment,
    File,
}

/// A key's value in the environment, as the configuration form reports it.
#[derive(Debug, Clone)]
pub struct EnvironmentSetting {
    pub value: String,
    /// Outranks config.json whatever the precedence.
    pub forced: bool,
}

/// Resolves settings from the environment and config.json, environment first unless
/// `EAS_CONFIG_PRECEDENCE=file`.
///
/// Every key goes through here, so a setting behaves the same wherever it is written, and a key
/// set only in `.env` takes effect instead of being silently ignored, which is what it did for
/// all but a handful of keys before.
struct Settings {
    json: Value,
    /// `None` reads the process environment. Tests supply their own instead: the real one is
    /// shared by every test running in parallel, so mutating it races them.
    env: Option<HashMap<String, String>>,
    /// Every key asked for, so a test can hold the configuration form's schema to exactly the
    /// keys resolution reads.
    #[cfg(test)]
    seen: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl Settings {
    fn new(json: Value) -> Self {
        Self {
            json,
            env: None,
            #[cfg(test)]
            seen: Default::default(),
        }
    }

    /// The environment on its own, for the fallback used when config.json cannot be read.
    fn without_json() -> Self {
        Self::new(Value::Object(serde_json::Map::new()))
    }

    #[cfg(test)]
    fn isolated(json: Value, env: &[(&str, &str)]) -> Self {
        let env = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Self {
            json,
            env: Some(env),
            seen: Default::default(),
        }
    }

    fn json(&self) -> &Value {
        &self.json
    }

    fn env_var(&self, key: &str) -> Option<String> {
        match &self.env {
            Some(env) => env.get(key).cloned(),
            None => std::env::var(key).ok(),
        }
    }

    /// A blank environment variable counts as unset, so an empty entry in a `.env` file cannot
    /// mask a real value in config.json.
    fn env_value(&self, key: &str) -> Option<String> {
        self.env_var(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn precedence(&self) -> Precedence {
        match self.env_value(PRECEDENCE_VAR) {
            Some(value) if value.eq_ignore_ascii_case("file") => Precedence::File,
            _ => Precedence::Environment,
        }
    }

    fn is_forced(&self, key: &str) -> bool {
        self.env_value(FORCED_KEYS_VAR)
            .is_some_and(|list| list.split(',').any(|entry| entry.trim() == key))
    }

    fn raw(&self, key: &str) -> Option<Value> {
        #[cfg(test)]
        self.seen.borrow_mut().insert(key.to_string());

        let from_env = self.env_value(key).map(Value::String);
        let from_json = match self.json.get(key) {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.clone()),
        };

        if self.precedence() == Precedence::File && !self.is_forced(key) {
            from_json.or(from_env)
        } else {
            from_env.or(from_json)
        }
    }

    fn string(&self, key: &str) -> Result<Option<String>> {
        match self.raw(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(anyhow!("{key} must be a string in your config.json file")),
        }
    }

    /// A string that carries a path or a name, where blank is a mistake rather than a default.
    fn required_text(&self, key: &str) -> Result<Option<String>> {
        let Some(value) = self.string(key)? else {
            return Ok(None);
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("{key} cannot be empty in your config.json file"));
        }
        Ok(Some(trimmed.to_string()))
    }

    /// A string kept only when it has content; a blank one leaves the default in place.
    fn optional_text(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .string(key)?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()))
    }

    /// A string kept as written but trimmed, blank included -- for values that are legitimately
    /// cleared by setting them to "".
    fn text(&self, key: &str) -> Result<Option<String>> {
        Ok(self.string(key)?.map(|value| value.trim().to_string()))
    }

    fn path(&self, key: &str) -> Result<Option<PathBuf>> {
        Ok(self.required_text(key)?.map(PathBuf::from))
    }

    /// A path that may be blank, which reads as "not set".
    fn loose_path(&self, key: &str) -> Result<Option<PathBuf>> {
        Ok(self.text(key)?.map(PathBuf::from))
    }

    fn usize(&self, key: &str) -> Result<Option<usize>> {
        let Some(value) = self.u64(key)? else {
            return Ok(None);
        };
        let converted = usize::try_from(value)
            .with_context(|| format!("{key} is too large for this platform"))?;
        Ok(Some(converted))
    }

    fn bool(&self, key: &str) -> Result<Option<bool>> {
        let invalid = || anyhow!("{key} must be either true or false in your config.json file");
        match self.raw(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(value)),
            // Environment variables are always strings, and "true" gets written in config.json too.
            Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Ok(Some(true)),
                "0" | "false" | "no" | "off" => Ok(Some(false)),
                _ => Err(invalid()),
            },
            Some(_) => Err(invalid()),
        }
    }

    fn u64(&self, key: &str) -> Result<Option<u64>> {
        match self.raw(key) {
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

    fn u16(&self, key: &str) -> Result<Option<u16>> {
        let Some(value) = self.u64(key)? else {
            return Ok(None);
        };

        let converted = u16::try_from(value)
            .with_context(|| format!("{key} must be between 0 and {}", u16::MAX))?;
        Ok(Some(converted))
    }

    /// An array arrives as a real array from config.json and as JSON text from the environment,
    /// which is exactly how the Docker entrypoint serialises one.
    fn array(&self, key: &str) -> Result<Option<Vec<Value>>> {
        let not_an_array = || anyhow!("{key} must be an array in your config.json file");
        match self.raw(key) {
            None => Ok(None),
            Some(Value::Array(entries)) => Ok(Some(entries)),
            Some(Value::String(text)) => {
                let parsed: Value = serde_json::from_str(text.trim())
                    .with_context(|| format!("{key} must be a JSON array"))?;
                match parsed {
                    Value::Array(entries) => Ok(Some(entries)),
                    _ => Err(not_an_array()),
                }
            }
            Some(_) => Err(not_an_array()),
        }
    }

    fn string_list(&self, key: &str) -> Result<Option<Vec<String>>> {
        let Some(entries) = self.array(key)? else {
            return Ok(None);
        };

        let values = entries
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(|text| text.trim().to_string())
                    .ok_or_else(|| {
                        anyhow!("{key} must only contain strings in your config.json file")
                    })
            })
            .collect::<Result<Vec<String>>>()?;
        Ok(Some(
            values.into_iter().filter(|text| !text.is_empty()).collect(),
        ))
    }
}

fn read_config_json(config_file: &Path) -> Result<Value> {
    let config_data = std::fs::read_to_string(config_file)
        .with_context(|| format!("Failed to read config file: {}", config_file.display()))?;
    serde_json::from_str(&config_data)
        .with_context(|| format!("Failed to parse config file: {}", config_file.display()))
}

fn default_shared_state_dir() -> PathBuf {
    state_dir_default(crate::paths::running_as_service())
}

/// A service's temp folder is not the user's -- LocalSystem's on Windows, one systemd-tmpfiles
/// may clear at boot on Linux -- so a service keeps its alert archive beside the install instead.
fn state_dir_default(service: bool) -> PathBuf {
    if service {
        crate::paths::in_app_root("data")
    } else {
        std::env::temp_dir().join("eas-listener")
    }
}

/// Absolute in the sense the alert database path has always used: a POSIX root or a drive letter.
fn looks_absolute(value: &str) -> bool {
    value.starts_with('/') || value.chars().nth(1) == Some(':')
}

impl Config {
    /// Settings from the environment alone, used when config.json is missing or unreadable.
    ///
    /// An invalid value cannot fail this path the way it fails a real load, so anything that does
    /// not resolve drops back to the compiled-in defaults rather than taking the process down --
    /// this is the fallback, and it has to produce *something*.
    pub fn safe_internal_defaults() -> Self {
        let settings = Settings::without_json();
        Self::resolve(&settings)
            .unwrap_or_else(|_| Self::baseline(&Self::resolve_shared_state_dir(&settings)))
    }

    pub fn from_config_json(config_file: &Path) -> Result<Self> {
        Self::resolve(&Settings::new(read_config_json(config_file)?))
    }

    /// The compiled-in defaults alone, with neither the environment nor config.json consulted --
    /// what the configuration form shows a key falls back to.
    pub fn defaults() -> Self {
        Self::baseline(&default_shared_state_dir())
    }

    pub fn precedence() -> Precedence {
        Settings::without_json().precedence()
    }

    /// The audio that stands in for the tones an alert opens with -- the SAME header burst and
    /// attention tone, or the Alert Ready signal -- when one is configured and still on disk.
    /// A path that no longer resolves falls back to the generated tones rather than leaving the
    /// alert with no opening at all.
    pub fn header_audio_override(&self) -> Option<&Path> {
        if !self.emit_header_tones || self.custom_header_audio.as_os_str().is_empty() {
            return None;
        }
        if !self.custom_header_audio.is_file() {
            tracing::warn!(
                "CUSTOM_HEADER_AUDIO is set to {:?}, which is not a readable file; using the \
                 generated tones instead",
                self.custom_header_audio
            );
            return None;
        }
        Some(&self.custom_header_audio)
    }

    /// Whether the 1050 Hz NOAA Weather Radio tone is looked for on this stream. With
    /// `ICECAST_STREAM_NWR` absent every stream is watched, which is what installs that predate
    /// the key expect; once it is set, only the streams it lists are.
    pub fn stream_is_nwr(&self, stream_url: &str) -> bool {
        match &self.nwr_stream_urls {
            None => true,
            Some(marked) => marked.contains(stream_url),
        }
    }

    /// The key's value in the process environment, if it has a non-blank one.
    pub fn environment_setting(key: &str) -> Option<EnvironmentSetting> {
        let settings = Settings::without_json();
        settings.env_value(key).map(|value| EnvironmentSetting {
            value,
            forced: settings.is_forced(key),
        })
    }

    /// Every config.json key resolution reads, found by resolving and recording what was asked for
    /// rather than by listing them, so the list cannot fall behind the code.
    #[cfg(test)]
    pub fn keys_read_by_resolve() -> std::collections::BTreeSet<String> {
        let settings = Settings::isolated(Value::Object(serde_json::Map::new()), &[]);
        Self::resolve(&settings).expect("an empty config resolves");
        settings.seen.into_inner()
    }

    /// Resolves `json` with no environment at all, for tests outside this module.
    #[cfg(test)]
    pub fn resolve_isolated(json: Value) -> Result<Self> {
        Self::resolve(&Settings::isolated(json, &[]))
    }

    /// The state directory has to be known before the paths that hang off it, so it is resolved on
    /// its own rather than through the knob table.
    fn resolve_shared_state_dir(settings: &Settings) -> PathBuf {
        settings
            .path("SHARED_STATE_DIR")
            .ok()
            .flatten()
            .unwrap_or_else(default_shared_state_dir)
    }

    /// The single path both `safe_internal_defaults` and `from_config_json` take, so no key can be
    /// honoured by one and ignored by the other.
    fn resolve(settings: &Settings) -> Result<Self> {
        let shared = settings
            .path("SHARED_STATE_DIR")?
            .unwrap_or_else(default_shared_state_dir);

        let mut merged = Self::baseline(&shared);
        merged.apply_plain_knobs(settings)?;
        merged.apply_derived_knobs(settings)?;
        merged.validate()?;
        Ok(merged)
    }

    /// Knobs whose handling is more than "read the key, assign it".
    fn apply_derived_knobs(&mut self, settings: &Settings) -> Result<()> {
        // Anchored to the state directory, so they follow it unless given explicitly.
        let dedicated_log_name = settings
            .required_text("DEDICATED_ALERT_LOG_FILE")?
            .unwrap_or_else(|| DEFAULT_DEDICATED_ALERT_LOG_NAME.to_string());
        self.dedicated_alert_log_file = self.shared_state_dir.join(dedicated_log_name);

        let alert_db_name = settings
            .required_text("ALERT_DATABASE_FILE")?
            .unwrap_or_else(|| DEFAULT_ALERT_DATABASE_NAME.to_string());
        self.alert_database_file = if looks_absolute(&alert_db_name) {
            PathBuf::from(alert_db_name)
        } else {
            self.shared_state_dir.join(alert_db_name)
        };

        if let Some(value) = settings.required_text("RECORDING_DIR")? {
            self.recording_dir = self.shared_state_dir.join(value);
        }

        if let Some(value) = settings.path("CEP6_VOICE_DIR")? {
            self.cep6_voice_dir = value;
        }

        // Icecast mounts are always rooted, and forgetting the slash is an easy mistake.
        if let Some(value) = settings.optional_text("ICECAST_ALERT_MOUNT")? {
            self.icecast_alert_mount = if value.starts_with('/') {
                value
            } else {
                format!("/{value}")
            };
        }

        if let Some(value) = settings.string("TTS_MODEL")? {
            self.tts_model = Some(value);
        }

        if let Some(value) = settings.optional_text("STORAGE_SAVER_MODE_EXT")? {
            self.storage_saver_ext = RecordingFormat::parse(&value).ok_or_else(|| {
                anyhow!(
                    "STORAGE_SAVER_MODE_EXT must be either \"mp3\" or \"ogg\" in your config.json file"
                )
            })?;
        }

        if let Some(value) = settings.optional_text("TZ")? {
            self.timezone = value
                .parse()
                .map_err(|_| anyhow!("TZ '{value}' is not a known IANA time zone name"))?;
        }

        if let Some(value) = settings.string("WATCHED_FIPS")? {
            self.watched_fips = value
                .split(',')
                .filter_map(|part| {
                    let trimmed = part.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                })
                .collect::<HashSet<String>>();
        }

        // E2T matches the mode in upper case and quietly falls back to its generic wording for
        // anything it does not recognise, so a typo is caught here instead of silently rewording
        // every alert.
        if let Some(value) = settings.optional_text("ENDEC_MODE")? {
            let mode = value.to_uppercase();
            let known = crate::e2t_ng::known_endec_modes();
            if !known.iter().any(|candidate| candidate == &mode) {
                return Err(anyhow!(
                    "ENDEC_MODE '{}' is not a known ENDEC emulation mode. Supported: {}",
                    value,
                    known.join(", ")
                ));
            }
            self.endec_mode = mode;
        }

        if let Some(value) = settings.string("MONITORING_BIND_ADDR")? {
            self.monitoring_bind_addr = value
                .trim()
                .parse::<SocketAddr>()
                .with_context(|| "MONITORING_BIND_ADDR must be a valid socket address")?;
            self.monitoring_bind_port = self.monitoring_bind_addr.port();
        }

        // MONITORING_BIND_PORT predates MONITORING_BIND_ADDR and is still what docker-compose
        // publishes, so an explicit port decides the port that is actually bound; the address
        // supplies the interface. Keeping the two in step means the reported port can never lie.
        if let Some(value) = settings.u16("MONITORING_BIND_PORT")? {
            self.monitoring_bind_port = value;
            self.monitoring_bind_addr.set_port(value);
        }

        if let Some(value) = settings.u64("MONITORING_ACTIVITY_WINDOW_SECS")? {
            self.monitoring_activity_window_secs = value.max(1);
        }

        if let Some(languages) = settings.string_list("CAPCP_LANGUAGES")? {
            let languages = languages
                .into_iter()
                .map(|value| value.to_ascii_lowercase())
                .collect::<Vec<String>>();
            if !languages.is_empty() {
                self.capcp_languages = languages;
            }
        }

        if let Some(entries) = settings.array("CAP_ENDPOINTS")? {
            self.cap_endpoints = entries
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

        if let Some(streams) = settings.string_list("ICECAST_STREAM_URL_ARRAY")? {
            if streams.is_empty() {
                return Err(anyhow!(
                    "ICECAST_STREAM_URL_ARRAY must contain at least one stream URL"
                ));
            }
            self.icecast_stream_urls = streams;
        }

        if let Some(nwr) = settings.string_list("ICECAST_STREAM_NWR")? {
            self.nwr_stream_urls = Some(nwr.into_iter().collect());
        }

        for spec in crate::components::ALL {
            // A key present but blank clears an inherited path rather than setting an empty one.
            if let Some(value) = settings.text(spec.config_key)? {
                if value.is_empty() {
                    self.component_paths.remove(spec.config_key);
                } else {
                    self.component_paths
                        .insert(spec.config_key.to_string(), value);
                }
            }
        }

        self.filters = filter::parse_filters(settings.json(), settings.bool("ENABLE_FILTERS")?);
        Ok(())
    }

    /// Rules that need more than one knob to be known.
    fn validate(&self) -> Result<()> {
        if self.should_relay && self.should_relay_icecast && self.icecast_relay.is_empty() {
            return Err(anyhow!(
                "ICECAST_RELAY must be set if SHOULD_RELAY and SHOULD_RELAY_ICECAST are true"
            ));
        }

        if self.icecast_alert_stream_enabled {
            if self.icecast_alert_port == self.monitoring_bind_addr.port() {
                return Err(anyhow!(
                    "ICECAST_ALERT_PORT must differ from the dashboard's port ({}): the alert \
                     stream is served on its own port",
                    self.monitoring_bind_addr.port()
                ));
            }
            if self.icecast_alert_port == 0 {
                return Err(anyhow!(
                    "ICECAST_ALERT_PORT must be a valid port if ICECAST_ALERT_STREAM_ENABLED is true"
                ));
            }
        }

        if self.should_relay
            && self.should_relay_icecast
            && self.use_icecast_intro_outro
            && (self.icecast_intro.as_os_str().is_empty()
                || self.icecast_outro.as_os_str().is_empty())
        {
            return Err(anyhow!(
                "ICECAST_INTRO and ICECAST_OUTRO must be set if USE_ICECAST_INTRO_OUTRO is true in your config.json file"
            ));
        }

        if self.use_pre_post_roll_for_recordings
            && (self.icecast_intro.as_os_str().is_empty()
                || self.icecast_outro.as_os_str().is_empty())
        {
            return Err(anyhow!(
                "ICECAST_INTRO and ICECAST_OUTRO must be set if USE_PRE_POST_ROLL_FOR_RECORDINGS is true in your config.json file"
            ));
        }

        if self.process_cap_alerts && self.cap_endpoints.is_empty() {
            return Err(anyhow!(
                "CAP_ENDPOINTS must contain at least one endpoint in your config.json file if PROCESS_CAP_ALERTS is true"
            ));
        }

        if self.process_capcp_alerts && self.capcp_stream_endpoints.is_empty() {
            return Err(anyhow!(
                "CAPCP_STREAM_ENDPOINTS must contain at least one host:port entry in your config.json file if PROCESS_CAPCP_ALERTS is true"
            ));
        }

        Ok(())
    }

    pub fn get() -> &'static Config {
        static INSTANCE: OnceLock<Config> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let config_path = crate::paths::config_json();
            match Config::from_config_json(&config_path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("Failed to load config.json: {err}");
                    std::process::exit(1);
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn fixture_json(name: &str) -> Value {
        read_config_json(&fixture_path(name)).expect("fixture parses")
    }

    /// Resolves against `env` alone rather than the process environment, so these tests neither
    /// race each other nor depend on the shell they happen to run in.
    fn load(json: Value, env: &[(&str, &str)]) -> Result<Config> {
        Config::resolve(&Settings::isolated(json, env))
    }

    #[test]
    fn tts_voice_paths_follow_the_state_dir_and_env_overrides() {
        let cfg = load(json!({}), &[("SHARED_STATE_DIR", "C:/tmp/eas-unit")]).expect("config");
        assert_eq!(
            cfg.cep6_voice_dir,
            PathBuf::from("C:/tmp/eas-unit")
                .join("tts_voices")
                .join("cep6")
        );
        let app_root = crate::paths::app_root();
        assert_eq!(cfg.loq6_data_dir, PathBuf::new());
        assert_eq!(
            cfg.spfy_voice_dir,
            app_root
                .join("tts_voices")
                .join("spfy")
                .join("voices")
                .join("tom")
        );

        let cfg = load(
            json!({}),
            &[
                ("CEP6_VOICE_DIR", "/srv/voices/cep6"),
                ("LOQ6_DATA_DIR", "/srv/voices/loq6"),
                ("SPFY_VOICE_DIR", "/srv/voices/tom"),
            ],
        )
        .expect("config");
        assert_eq!(cfg.cep6_voice_dir, PathBuf::from("/srv/voices/cep6"));
        assert_eq!(cfg.loq6_data_dir, PathBuf::from("/srv/voices/loq6"));
        assert_eq!(cfg.spfy_voice_dir, PathBuf::from("/srv/voices/tom"));
    }

    #[test]
    fn defaults_have_expected_values() {
        let cfg = load(json!({}), &[]).expect("config");
        assert!(!cfg.shared_state_dir.as_os_str().is_empty());
        assert_eq!(cfg.monitoring_bind_addr.port(), cfg.monitoring_bind_port);
        assert_eq!(cfg.local_deeplink_host, "auto");
        assert_eq!(cfg.log_level, "INFO");
        assert_eq!(cfg.icecast_stream_urls.len(), 1);
        assert_eq!(cfg.storage_saver_ext, RecordingFormat::Mp3);
        assert!(!cfg.capcp_use_alert_ready_tone);
        assert!(cfg.emit_header_tones);
        assert_eq!(cfg.custom_header_audio, PathBuf::new());
    }

    #[test]
    fn a_service_keeps_its_state_beside_the_install() {
        assert_eq!(state_dir_default(true), crate::paths::in_app_root("data"));
        assert_eq!(
            state_dir_default(false),
            std::env::temp_dir().join("eas-listener")
        );
    }

    #[test]
    fn the_1050_hz_tone_is_watched_everywhere_until_the_streams_are_named() {
        let both = json!({ "ICECAST_STREAM_URL_ARRAY": ["one", "two"] });

        // No key: every stream, which is what an install made before the key expects.
        let cfg = load(both.clone(), &[]).expect("config");
        assert!(cfg.stream_is_nwr("one"));
        assert!(cfg.stream_is_nwr("two"));

        let mut named = both.clone();
        named["ICECAST_STREAM_NWR"] = json!(["two"]);
        let cfg = load(named, &[]).expect("config");
        assert!(!cfg.stream_is_nwr("one"));
        assert!(cfg.stream_is_nwr("two"));

        // An empty list is a decision, not an absent key: nothing is watched.
        let mut none = both.clone();
        none["ICECAST_STREAM_NWR"] = json!([]);
        let cfg = load(none, &[]).expect("config");
        assert!(!cfg.stream_is_nwr("one"));
        assert!(!cfg.stream_is_nwr("two"));

        // The environment carries it as a JSON array, the way every other list arrives.
        let cfg = load(both, &[("ICECAST_STREAM_NWR", r#"["one"]"#)]).expect("config");
        assert!(cfg.stream_is_nwr("one"));
        assert!(!cfg.stream_is_nwr("two"));
    }

    #[test]
    fn custom_header_audio_only_counts_when_it_is_a_file_and_tones_are_on() {
        let mut cfg = load(json!({}), &[]).expect("config");
        assert_eq!(cfg.header_audio_override(), None);

        cfg.custom_header_audio = PathBuf::from("C:/tmp/eas-unit/no-such-header.wav");
        assert_eq!(cfg.header_audio_override(), None);

        let real = std::env::current_exe().expect("a real file");
        cfg.custom_header_audio = real.clone();
        assert_eq!(cfg.header_audio_override(), Some(real.as_path()));

        cfg.emit_header_tones = false;
        assert_eq!(cfg.header_audio_override(), None);
    }

    #[test]
    fn env_overrides_apply_without_config_json() {
        let cfg = load(
            json!({}),
            &[
                ("SHARED_STATE_DIR", "C:/tmp/eas-unit"),
                ("MONITORING_BIND_ADDR", "127.0.0.1:18999"),
                ("MONITORING_BIND_PORT", "19001"),
                ("LOCAL_DEEPLINK_HOST", "example.local"),
                ("RUST_LOG", "DEBUG"),
            ],
        )
        .expect("config");
        assert_eq!(cfg.shared_state_dir, PathBuf::from("C:/tmp/eas-unit"));
        // The address supplies the interface, the explicit port wins.
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "127.0.0.1:19001");
        assert_eq!(cfg.monitoring_bind_port, 19001);
        assert_eq!(cfg.local_deeplink_host, "example.local");
        assert_eq!(cfg.log_level, "DEBUG");
    }

    #[test]
    fn minimal_fixture_merges_over_defaults() {
        let cfg = load(fixture_json("config_minimal.json"), &[]).expect("config");
        assert_eq!(
            cfg.icecast_stream_urls,
            vec!["http://example.local/stream1.mp3"]
        );
        assert!(cfg.watched_fips.contains("031055"));
        assert!(cfg.watched_fips.contains("031153"));
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "127.0.0.1:18080");
    }

    #[test]
    fn cap_endpoints_accept_mixed_entries() {
        let cfg = load(fixture_json("config_cap_endpoints_mixed.json"), &[]).expect("config");
        assert!(cfg.process_cap_alerts);
        assert_eq!(cfg.cap_endpoints.len(), 2);
        assert_eq!(cfg.cap_endpoints[0].name, None);
        assert_eq!(cfg.cap_endpoints[0].url, "https://alerts.example/feed");
        assert_eq!(cfg.cap_endpoints[1].name.as_deref(), Some("Named Feed"));
    }

    #[test]
    fn relay_misconfiguration_is_rejected() {
        let err = load(fixture_json("config_relay_invalid.json"), &[])
            .expect_err("expected relay config error");
        assert!(err.to_string().contains(
            "ICECAST_RELAY must be set if SHOULD_RELAY and SHOULD_RELAY_ICECAST are true"
        ));
    }

    #[test]
    fn cap_without_endpoints_is_rejected() {
        let err = load(fixture_json("config_cap_invalid.json"), &[])
            .expect_err("expected cap config error");
        assert!(err
            .to_string()
            .contains("CAP_ENDPOINTS must contain at least one endpoint"));
    }

    #[test]
    fn bad_monitoring_port_type_is_rejected() {
        let err = load(fixture_json("config_malformed_types.json"), &[])
            .expect_err("expected malformed config error");
        assert!(err
            .to_string()
            .contains("MONITORING_BIND_PORT must be a valid integer"));
    }

    #[test]
    fn env_local_deeplink_host_takes_precedence() {
        let cfg = load(
            json!({
                "LOCAL_DEEPLINK_HOST": "config-host.test",
                "ICECAST_STREAM_URL_ARRAY": ["http://example.local/stream1.mp3"]
            }),
            &[("LOCAL_DEEPLINK_HOST", "env-host.test")],
        )
        .expect("config");
        assert_eq!(cfg.local_deeplink_host, "env-host.test");
    }

    #[test]
    fn monitoring_bind_port_moves_the_listener() {
        // The port alone has to move the bound address, or a deployment that only ever set
        // MONITORING_BIND_PORT would serve on 8080 while claiming otherwise.
        let cfg = load(json!({}), &[("MONITORING_BIND_PORT", "19100")]).expect("config");
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "127.0.0.1:19100");
        assert_eq!(cfg.monitoring_bind_port, 19100);

        // The address alone still decides both halves.
        let cfg = load(json!({}), &[("MONITORING_BIND_ADDR", "0.0.0.0:19200")]).expect("config");
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "0.0.0.0:19200");
        assert_eq!(cfg.monitoring_bind_port, 19200);

        let cfg = load(json!({ "MONITORING_BIND_PORT": 19300 }), &[]).expect("config");
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "127.0.0.1:19300");
        assert_eq!(cfg.monitoring_bind_port, 19300);

        let cfg = load(
            json!({
                "MONITORING_BIND_ADDR": "0.0.0.0:19400",
                "MONITORING_BIND_PORT": 19500
            }),
            &[],
        )
        .expect("config");
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "0.0.0.0:19500");
        assert_eq!(cfg.monitoring_bind_port, 19500);

        let cfg = load(json!({ "MONITORING_BIND_ADDR": "0.0.0.0:19600" }), &[]).expect("config");
        assert_eq!(cfg.monitoring_bind_addr.to_string(), "0.0.0.0:19600");
        assert_eq!(cfg.monitoring_bind_port, 19600);
    }

    /// ENDEC_MODE used to be readable from the environment only, so writing it in config.json did
    /// nothing outside Docker. Both sources have to move the same field.
    #[test]
    fn endec_mode_is_honoured_from_either_source() {
        let cfg = load(json!({ "ENDEC_MODE": "SAGE" }), &[]).expect("config");
        assert_eq!(cfg.endec_mode, "SAGE");

        let cfg = load(json!({ "ENDEC_MODE": "sage" }), &[]).expect("config");
        assert_eq!(cfg.endec_mode, "SAGE");

        let err = load(json!({ "ENDEC_MODE": "SAGEY" }), &[]).expect_err("unknown mode");
        assert!(err.to_string().contains("SAGEY"), "{err}");
        assert!(err.to_string().contains("SAGE"), "{err}");

        let cfg = load(json!({}), &[("ENDEC_MODE", "BURK")]).expect("config");
        assert_eq!(cfg.endec_mode, "BURK");

        // The environment wins, which is what keeps Docker's behaviour intact: its entrypoint
        // exports config.json into the environment before the listener starts.
        let cfg = load(json!({ "ENDEC_MODE": "SAGE" }), &[("ENDEC_MODE", "BURK")]).expect("config");
        assert_eq!(cfg.endec_mode, "BURK");
    }

    /// Docker's entrypoint copies config.json into the environment at boot. With the environment
    /// outranking the file, that copy pinned every key to its boot-time value, so an edit followed
    /// by a reload did nothing; `file` precedence is what the entrypoint now sets instead.
    #[test]
    fn file_precedence_lets_config_json_outrank_the_environment() {
        let json = json!({ "TZ": "America/Chicago", "EAS_RELAY_NAME": "FROM-FILE" });
        let env = [("TZ", "UTC"), ("EAS_RELAY_NAME", "FROM-ENV")];

        let cfg = load(json.clone(), &env).expect("config");
        assert_eq!(cfg.timezone.name(), "UTC");
        assert_eq!(cfg.eas_relay_name, "FROM-ENV");

        let file_first = [env.as_slice(), &[("EAS_CONFIG_PRECEDENCE", "file")]].concat();
        let cfg = load(json.clone(), &file_first).expect("config");
        assert_eq!(cfg.timezone.name(), "America/Chicago");
        assert_eq!(cfg.eas_relay_name, "FROM-FILE");

        // A key config.json leaves out still comes from the environment.
        let cfg = load(json!({}), &file_first).expect("config");
        assert_eq!(cfg.eas_relay_name, "FROM-ENV");

        let forced = [
            file_first.as_slice(),
            &[("EAS_CONFIG_FORCED_KEYS", "TTS_ENGINE, EAS_RELAY_NAME")],
        ]
        .concat();
        let cfg = load(json, &forced).expect("config");
        assert_eq!(cfg.eas_relay_name, "FROM-ENV");
        assert_eq!(cfg.timezone.name(), "America/Chicago");
    }

    /// The guard against the whole bug class: every knob declared in the table must be reachable
    /// from the environment and from config.json alike, whatever its type.
    #[test]
    fn every_plain_knob_is_read_from_either_source() {
        // One value per accessor kind, in each source's native form, that parses as that kind and
        // differs from every default.
        let probes: &[(&str, Value)] = &[
            ("true", json!(true)),
            ("19999", json!(19999)),
            ("4242", json!(4242)),
            (r#"["probe-entry"]"#, json!(["probe-entry"])),
            ("probe-value", json!("probe-value")),
        ];

        // resolve() rather than safe_internal_defaults(), which swallows errors back to the
        // baseline and would hide a knob whose probe value trips a cross-field rule.
        let baseline = format!("{:?}", load(json!({}), &[]).expect("baseline resolves"));
        let moved = |outcome: Result<Config>| match outcome {
            // Failing a validation rule still proves the key was read.
            Err(_) => true,
            Ok(candidate) => format!("{candidate:?}") != baseline,
        };

        for key in Config::plain_knob_keys() {
            assert!(
                probes
                    .iter()
                    .any(|(text, _)| moved(load(json!({}), &[(key, text)]))),
                "{key} is declared as a knob but setting it in the environment changed nothing"
            );
            assert!(
                probes
                    .iter()
                    .any(|(_, value)| moved(load(json!({ *key: value.clone() }), &[]))),
                "{key} is declared as a knob but setting it in config.json changed nothing"
            );
        }
    }

    #[test]
    fn storage_saver_mode_ext_parses_and_validates() {
        let cfg = load(
            json!({
                "STORAGE_SAVER_MODE": true,
                "STORAGE_SAVER_MODE_EXT": "OGG"
            }),
            &[],
        )
        .expect("config");
        assert!(cfg.storage_saver_mode);
        assert_eq!(cfg.storage_saver_ext, RecordingFormat::OggOpus);

        let err = load(json!({ "STORAGE_SAVER_MODE_EXT": "flac" }), &[])
            .expect_err("expected invalid format error");
        assert!(err.to_string().contains("STORAGE_SAVER_MODE_EXT"));
    }

    /// The one test that goes through the real process environment. It uses a key no setting
    /// has, so it cannot disturb any other test, and needs no lock.
    #[test]
    fn process_environment_is_read_and_blank_values_do_not_mask_json() {
        const KEY: &str = "EAS_LISTENER_SETTINGS_SELF_TEST";
        let settings = Settings::new(json!({ KEY: "from-json" }));

        std::env::set_var(KEY, "from-env");
        assert_eq!(
            settings.string(KEY).expect("string").as_deref(),
            Some("from-env")
        );

        std::env::set_var(KEY, "   ");
        assert_eq!(
            settings.string(KEY).expect("string").as_deref(),
            Some("from-json")
        );

        std::env::remove_var(KEY);
        assert_eq!(
            settings.string(KEY).expect("string").as_deref(),
            Some("from-json")
        );
    }

    #[test]
    fn from_config_json_reads_the_file_and_reports_a_missing_one() {
        let cfg = Config::from_config_json(&fixture_path("config_minimal.json")).expect("config");
        assert!(cfg.watched_fips.contains("031055"));

        let err = Config::from_config_json(&fixture_path("no_such_config.json"))
            .expect_err("missing file");
        assert!(
            err.to_string().contains("Failed to read config file"),
            "{err}"
        );
    }
}
