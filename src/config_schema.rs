//! Describes every config.json key for the dashboard's configuration form and first-run setup.
//!
//! `Config` decides what a key does; this says where it sits on the form, what kind of input it
//! takes and what it falls back to. Defaults are read off `Config::defaults()` wherever `Config`
//! has a field for the key, and `the_schema_describes_exactly_the_keys_resolution_reads` fails
//! when a key is read without being described here, or described without being read.

use crate::components::{self, Requirement};
use crate::config::{self, Config};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Serialize)]
pub struct Group {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// First-run setup walks through these, one step each.
    pub essential: bool,
}

pub const GROUPS: &[Group] = &[
    Group {
        id: "dashboard",
        title: "Dashboard sign-in",
        summary: "Who can sign in to this dashboard, and how it behaves.",
        essential: true,
    },
    Group {
        id: "sources",
        title: "Monitored streams",
        summary: "The audio streams the listener decodes EAS headers from.",
        essential: true,
    },
    Group {
        id: "location",
        title: "Location & wording",
        summary:
            "Which alerts are yours, what time zone they are shown in, and how they are worded.",
        essential: true,
    },
    Group {
        id: "cap",
        title: "CAP alerts (IPAWS)",
        summary: "Alerts fetched from FEMA's IPAWS feeds as text and read aloud by the TTS engine.",
        essential: true,
    },
    Group {
        id: "capcp",
        title: "CAP-CP alerts (Canada)",
        summary: "Alert Ready alerts from the NAAD streaming feeds.",
        essential: false,
    },
    Group {
        id: "tts",
        title: "Text-to-speech",
        summary: "The voice CAP alerts and test alerts are narrated with.",
        essential: false,
    },
    Group {
        id: "filters",
        title: "Filters",
        summary: "What happens to an alert, chosen by its event code.",
        essential: false,
    },
    Group {
        id: "relay",
        title: "Relaying",
        summary: "Sending alert audio on to Icecast, a MYOD DASDEC, or notification targets.",
        essential: false,
    },
    Group {
        id: "alert_stream",
        title: "24/7 alert stream",
        summary: "A continuous Icecast stream that carries alerts as they happen.",
        essential: false,
    },
    Group {
        id: "storage",
        title: "Recordings & storage",
        summary: "Where recordings, the alert database and other state are kept.",
        essential: false,
    },
    Group {
        id: "logging",
        title: "Logging",
        summary: "What gets logged, where, and how much.",
        essential: false,
    },
    Group {
        id: "network",
        title: "Web server & network",
        summary: "Where the dashboard listens, and how it is reached through a reverse proxy.",
        essential: false,
    },
    Group {
        id: "components",
        title: "External programs",
        summary: "Where to find the programs the listener runs. Leave a path empty to look in the \
                  install's tools folder, then on PATH.",
        essential: false,
    },
];

#[derive(Debug, Serialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

fn choices(pairs: &[(&str, &str)]) -> Vec<Choice> {
    pairs
        .iter()
        .map(|(value, label)| Choice {
            value: value.to_string(),
            label: label.to_string(),
        })
        .collect()
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Bool,
    Text {
        placeholder: &'static str,
    },
    Secret,
    Integer {
        min: u64,
        max: u64,
    },
    Path {
        placeholder: String,
    },
    /// The form still shows a value outside `options` that config.json already holds, so a
    /// hand-written one (a `RUST_LOG` directive, say) survives being opened and saved.
    Choice {
        options: Vec<Choice>,
    },
    TimeZone {
        options: Vec<String>,
    },
    StringList {
        placeholder: &'static str,
    },
    /// Six-digit SAME location codes, kept in config.json as one comma-separated string.
    FipsList,
    /// Edited together with the nicknames kept under `nicknames_key` and the NOAA Weather Radio
    /// marks kept under `nwr_key`.
    StreamList {
        nicknames_key: &'static str,
        nwr_key: &'static str,
    },
    /// Edited by another field's input rather than one of its own.
    Managed,
    CapEndpoints {
        presets: Value,
    },
    Filters {
        actions: Vec<Choice>,
    },
}

#[derive(Debug, Serialize)]
pub struct Field {
    pub key: String,
    pub group: &'static str,
    pub label: String,
    pub help: String,
    #[serde(flatten)]
    pub kind: Kind,
    pub default: Value,
    /// A boolean key that has to be on for this one to matter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<&'static str>,
    /// First-run setup does not finish until this is set.
    pub setup_required: bool,
    /// Read once at startup, so a reload does not apply a change to it.
    pub restart: bool,
    /// Read straight from config.json, so the environment cannot set it.
    pub json_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Value>,
}

impl Field {
    fn new(
        key: impl Into<String>,
        group: &'static str,
        label: impl Into<String>,
        help: impl Into<String>,
        kind: Kind,
        default: Value,
    ) -> Self {
        Self {
            key: key.into(),
            group,
            label: label.into(),
            help: help.into(),
            kind,
            default,
            requires: None,
            setup_required: false,
            restart: false,
            json_only: false,
            environment: None,
        }
    }

    fn requires(mut self, key: &'static str) -> Self {
        self.requires = Some(key);
        self
    }

    fn setup_required(mut self) -> Self {
        self.setup_required = true;
        self
    }

    fn restart(mut self) -> Self {
        self.restart = true;
        self
    }

    fn json_only(mut self) -> Self {
        self.json_only = true;
        self
    }
}

fn text(placeholder: &'static str) -> Kind {
    Kind::Text { placeholder }
}

fn path(placeholder: impl Into<String>) -> Kind {
    Kind::Path {
        placeholder: placeholder.into(),
    }
}

fn path_value(value: &std::path::Path) -> Value {
    Value::String(value.display().to_string())
}

pub const TTS_ENGINES: &[(&str, &str)] = &[
    ("piper", "Piper (neural)"),
    ("espeak-ng", "eSpeak NG"),
    ("speechify", "Speechify Tom"),
    ("cepstral", "Cepstral Swift 6.2"),
    ("loquendo", "Loquendo 6.9 Dave"),
];

fn cap_endpoint_presets() -> Value {
    json!([
        {
            "label": "FEMA IPAWS feeds (EAS, WEA and ENDEC)",
            "endpoints": [
                {
                    "name": "ENDEC CAP Endpoint",
                    "url": "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/feed"
                },
                {
                    "name": "WEA CAP Endpoint",
                    "url": "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/PublicWEA/recent/2012-08-21T11:40:43Z"
                },
                {
                    "name": "EAS CAP Endpoint",
                    "url": "https://apps.fema.gov/IPAWSOPEN_EAS_SERVICE/rest/eas/recent/2019-12-31T11:59:59Z"
                }
            ]
        }
    ])
}

fn endec_modes() -> Vec<Choice> {
    crate::e2t_ng::known_endec_modes()
        .into_iter()
        .map(|mode| Choice {
            label: if mode == "DEFAULT" {
                "DEFAULT, the listener's own wording".to_string()
            } else {
                mode.clone()
            },
            value: mode,
        })
        .collect()
}

fn component_fields() -> Vec<Field> {
    components::ALL
        .iter()
        .map(|spec| {
            let requirement = match spec.requirement {
                Requirement::Required => "Required: the listener does not start without it.",
                Requirement::Optional => "Optional: only that feature needs it.",
            };
            let bundled = format!("tools/{}{}", spec.binary, std::env::consts::EXE_SUFFIX);
            Field::new(
                spec.config_key,
                "components",
                format!("{} path", spec.binary),
                format!("{}. {requirement}", spec.purpose),
                path(format!("{bundled}, then {} on PATH", spec.binary)),
                json!(""),
            )
        })
        .collect()
}

/// The form's fields, in the order they appear.
pub fn fields() -> Vec<Field> {
    let d = Config::defaults();
    let mut watched_fips = d.watched_fips.iter().cloned().collect::<Vec<_>>();
    watched_fips.sort();

    let mut fields =
        vec![
        // Dashboard
        Field::new(
            "DASHBOARD_USERNAME",
            "dashboard",
            "Username",
            "The name you sign in with. The dashboard lets nobody in while it is left as admin.",
            text("your name"),
            json!(d.dashboard_username),
        )
        .setup_required(),
        Field::new(
            "DASHBOARD_PASSWORD",
            "dashboard",
            "Password",
            "The dashboard lets nobody in while it is left as password. Changing it signs \
             everyone out once the configuration is reloaded.",
            Kind::Secret,
            json!(d.dashboard_password),
        )
        .setup_required(),
        Field::new(
            "ALERT_SOUND_ENABLED",
            "dashboard",
            "Play a sound on new alerts",
            "The open dashboard plays a sound whenever an alert arrives.",
            Kind::Bool,
            json!(false),
        )
        .json_only(),
        Field::new(
            "ALERT_SOUND_SRC",
            "dashboard",
            "Alert sound",
            "A sound file the dashboard can load. The default is the IEMBot ping.",
            text("iembot.mp3"),
            json!("iembot.mp3"),
        )
        .requires("ALERT_SOUND_ENABLED")
        .json_only(),
        Field::new(
            "MONITORING_MAX_LOGS",
            "dashboard",
            "Log entries kept",
            "How many log entries the dashboard holds before dropping the oldest.",
            Kind::Integer { min: 1, max: 100_000 },
            json!(d.monitoring_max_log_entries),
        )
        .restart(),
        // Streams
        Field::new(
            "ICECAST_STREAM_URL_ARRAY",
            "sources",
            "Streams",
            "Every stream to monitor, one per row. Anything ffmpeg can open works: Icecast, \
             Shoutcast, HLS or a plain HTTP audio URL. The nickname is what the dashboard shows \
             instead of the URL. Mark the ones that carry NOAA Weather Radio: the 1050 Hz alert \
             tone is only listened for on those, so nothing that merely sounds like it can start \
             a recording elsewhere.",
            Kind::StreamList {
                nicknames_key: "ICECAST_STREAM_URL_MAPPING",
                nwr_key: "ICECAST_STREAM_NWR",
            },
            json!(d.icecast_stream_urls),
        )
        .setup_required(),
        Field::new(
            "ICECAST_STREAM_URL_MAPPING",
            "sources",
            "Stream nicknames",
            "Edited alongside the stream list.",
            Kind::Managed,
            json!({}),
        )
        .json_only(),
        Field::new(
            "ICECAST_STREAM_NWR",
            "sources",
            "NOAA Weather Radio streams",
            "Edited alongside the stream list. Absent from config.json, every stream is watched \
             for the 1050 Hz tone, which is what installs made before this setting existed \
             expect; an empty list means no stream is.",
            Kind::Managed,
            // Not `[]`: absent and empty mean different things here, and the form's "same as the
            // default, so do not write it" rule would collapse one into the other.
            Value::Null,
        ),
        // Location & wording
        Field::new(
            "TZ",
            "location",
            "Time zone",
            "The IANA time zone alert times and logs are shown in.",
            Kind::TimeZone {
                options: chrono_tz::TZ_VARIANTS
                    .iter()
                    .map(|tz| tz.name().to_string())
                    .collect(),
            },
            json!(d.timezone.name()),
        )
        .setup_required(),
        Field::new(
            "WATCHED_FIPS",
            "location",
            "Watched locations",
            "The SAME location codes that count as yours. Alerts elsewhere are still decoded, \
             but are not relayed. Leave empty to watch everywhere; 000000 is the whole United \
             States.",
            Kind::FipsList,
            json!(watched_fips.join(",")),
        ),
        Field::new(
            "EAS_RELAY_NAME",
            "location",
            "Relay name",
            "Tells people who relayed an alert: the Icecast stream name and the station in \
             notifications. A call sign works well.",
            text("EAS Listener"),
            json!(d.eas_relay_name),
        ),
        Field::new(
            "ENDEC_MODE",
            "location",
            "ENDEC wording",
            "Which ENDEC's style the plain-English alert text imitates.",
            Kind::Choice {
                options: endec_modes(),
            },
            json!(d.endec_mode.to_uppercase()),
        ),
        Field::new(
            "PREFERRED_SENDERID",
            "location",
            "Preferred sender",
            "When several stations relay the same alert, the copy from this sender ID is always \
             processed instead of whichever arrives first.",
            text("KWO35"),
            json!(d.preferred_senderid),
        ),
        // CAP
        Field::new(
            "PROCESS_CAP_ALERTS",
            "cap",
            "Process CAP alerts",
            "Poll the feeds below for new alerts, about once a minute.",
            Kind::Bool,
            json!(d.process_cap_alerts),
        ),
        Field::new(
            "CAP_ENDPOINTS",
            "cap",
            "CAP feeds",
            "Each feed's URL, with an optional name for the dashboard.",
            Kind::CapEndpoints {
                presets: cap_endpoint_presets(),
            },
            json!([]),
        )
        .requires("PROCESS_CAP_ALERTS"),
        // CAP-CP
        Field::new(
            "PROCESS_CAPCP_ALERTS",
            "capcp",
            "Process CAP-CP alerts",
            "Listen to the NAAD streaming feeds for Canadian Alert Ready alerts.",
            Kind::Bool,
            json!(d.process_capcp_alerts),
        ),
        Field::new(
            "CAPCP_STREAM_ENDPOINTS",
            "capcp",
            "NAAD servers",
            "Streaming servers, as host:port. The defaults are Pelmorex's two public servers.",
            Kind::StringList {
                placeholder: "streaming1.naad-adna.pelmorex.com:8080",
            },
            json!(d.capcp_stream_endpoints),
        )
        .requires("PROCESS_CAPCP_ALERTS"),
        Field::new(
            "CAPCP_LANGUAGES",
            "capcp",
            "Languages",
            "CAP language codes to process, such as en or fr.",
            Kind::StringList { placeholder: "en" },
            json!(d.capcp_languages),
        )
        .requires("PROCESS_CAPCP_ALERTS"),
        Field::new(
            "CAPCP_GEOCODE_FILTER",
            "capcp",
            "Geocodes",
            "SGC geocodes to accept. A trailing * matches a prefix, so 35* is all of Ontario; * \
             or an empty list accepts everything.",
            Kind::StringList { placeholder: "35*" },
            json!(d.capcp_geocode_filter),
        )
        .requires("PROCESS_CAPCP_ALERTS"),
        Field::new(
            "CAPCP_REQUIRE_IMMEDIATE",
            "capcp",
            "Only immediate alerts",
            "Only process alerts SOREM marks for immediate broadcast or wireless delivery.",
            Kind::Bool,
            json!(d.capcp_require_immediate),
        )
        .requires("PROCESS_CAPCP_ALERTS"),
        Field::new(
            "CAPCP_USE_ALERT_READY_TONE",
            "capcp",
            "Record with the Alert Ready tone",
            "Record these alerts the Canadian way: the Alert Ready attention signal, then the \
             message, with no SAME header and no NNNN.",
            Kind::Bool,
            json!(d.capcp_use_alert_ready_tone),
        )
        .requires("PROCESS_CAPCP_ALERTS"),
        // Text-to-speech
        Field::new(
            "TTS_ENGINE",
            "tts",
            "Engine",
            "In Docker, an engine the image does not include falls back to Piper when the \
             container starts, and a banner on the dashboard says so.",
            Kind::Choice {
                options: choices(TTS_ENGINES),
            },
            json!(d.tts_engine),
        ),
        Field::new(
            "TTS_MODEL",
            "tts",
            "Voice",
            "Piper: a path to an .onnx model. Cepstral: Allison, David, Jean-Pierre or William. \
             Speechify: a voice name or path. eSpeak NG and Loquendo ignore it. Leave empty for \
             the engine's own default.",
            text("engine default"),
            Value::Null,
        ),
        Field::new(
            "TTS_READ_CALLSIGN",
            "tts",
            "Read the call sign",
            "On reads the sender at the end of the alert's opening sentence -- \"(KWO35)\", \
             \"Message from KWO35\" -- aloud; off leaves it out, whatever ENDEC_MODE is. The text \
             on the dashboard keeps it either way. Loquendo in SAGE mode never reads it, as on a \
             real SAGE.",
            Kind::Bool,
            json!(d.tts_read_callsign),
        ),
        Field::new(
            "TTS_BUILTIN_REPLACEMENTS",
            "tts",
            "Use the built-in pronunciation dictionary",
            "Fixes how county names, abbreviations and N-1-1 numbers are read. Entries in \
             cap_tts_replacement_config.json are added on top and win where both have a key.",
            Kind::Bool,
            json!(d.tts_builtin_replacements),
        ),
        Field::new(
            "CEP6_VOICE_DIR",
            "tts",
            "Cepstral voice folder",
            "Where Cepstral voices are installed. Docker downloads the chosen voice here the \
             first time the engine is selected.",
            path(d.cep6_voice_dir.display().to_string()),
            path_value(&d.cep6_voice_dir),
        ),
        Field::new(
            "LOQ6_DATA_DIR",
            "tts",
            "Loquendo voice folder",
            "Leave empty: Dave is built into loqdave. Set only to run it against a different \
             Loquendo voice tree, which then replaces the built-in one entirely.",
            path("/srv/loquendo/data"),
            path_value(&d.loq6_data_dir),
        ),
        Field::new(
            "SPFY_VOICE_DIR",
            "tts",
            "Speechify voice folder",
            "Only worth changing when running outside Docker.",
            path(d.spfy_voice_dir.display().to_string()),
            path_value(&d.spfy_voice_dir),
        ),
        // Filters
        Field::new(
            "ENABLE_FILTERS",
            "filters",
            "Use filters",
            "Off means every alert is relayed.",
            Kind::Bool,
            json!(true),
        ),
        Field::new(
            "FILTERS",
            "filters",
            "Rules",
            "An event code listed by a rule gets that rule's action; * catches every code no rule \
             lists. Codes no rule matches are relayed.",
            Kind::Filters {
                actions: choices(&[
                    ("relay", "Relay (everything)"),
                    ("forward", "Forward (log and notify, no relay)"),
                    ("log", "Log only"),
                    ("ignore", "Ignore"),
                ]),
            },
            json!([]),
        )
        .requires("ENABLE_FILTERS")
        .json_only(),
        // Relaying
        Field::new(
            "SHOULD_RELAY",
            "relay",
            "Relay alert audio",
            "Send each alert's audio on to the destinations below.",
            Kind::Bool,
            json!(d.should_relay),
        ),
        Field::new(
            "SHOULD_RELAY_ICECAST",
            "relay",
            "Relay to Icecast",
            "Play each alert into an Icecast mount you run.",
            Kind::Bool,
            json!(d.should_relay_icecast),
        )
        .requires("SHOULD_RELAY"),
        Field::new(
            "ICECAST_RELAY",
            "relay",
            "Icecast destination",
            "An ffmpeg output URL, including the source password.",
            text("icecast://source:password@192.168.1.100:8000/live"),
            json!(d.icecast_relay),
        )
        .requires("SHOULD_RELAY_ICECAST"),
        Field::new(
            "USE_ICECAST_INTRO_OUTRO",
            "relay",
            "Play intro and outro",
            "Wrap each relayed alert in the intro and outro audio below.",
            Kind::Bool,
            json!(d.use_icecast_intro_outro),
        )
        .requires("SHOULD_RELAY_ICECAST"),
        Field::new(
            "ICECAST_INTRO",
            "relay",
            "Intro audio",
            "Played before the alert when relaying, and before recordings when that is on.",
            path("/app/leadin.mp3"),
            path_value(&d.icecast_intro),
        ),
        Field::new(
            "ICECAST_OUTRO",
            "relay",
            "Outro audio",
            "Played after the alert when relaying, and after recordings when that is on.",
            path("/app/leadout.mp3"),
            path_value(&d.icecast_outro),
        ),
        Field::new(
            "SHOULD_RELAY_DASDEC",
            "relay",
            "Relay to a MYOD DASDEC",
            "Send each alert to a Make Your Own DASDEC instance.",
            Kind::Bool,
            json!(d.should_relay_dasdec),
        )
        .requires("SHOULD_RELAY"),
        Field::new(
            "DASDEC_URL",
            "relay",
            "DASDEC address",
            "Where your MYOD instance accepts alerts.",
            text("http://192.168.1.100:5000/send"),
            json!(d.dasdec_url),
        )
        .requires("SHOULD_RELAY_DASDEC"),
        Field::new(
            "APPRISE_CONFIG_PATH",
            "relay",
            "Apprise configuration",
            "The file listing where alert notifications are sent, one Apprise URL per line. Edit \
             the list itself under Notifications on this page.",
            path(d.apprise_config_path.clone()),
            json!(d.apprise_config_path),
        ),
        // 24/7 alert stream
        Field::new(
            "ICECAST_ALERT_STREAM_ENABLED",
            "alert_stream",
            "Serve the alert stream",
            "A continuous Ogg Vorbis stream (Ogg Opus where ffmpeg has no libvorbis), served by \
             the listener itself: comfort noise between alerts, and each alert's audio as it \
             happens. No Icecast server is needed.",
            Kind::Bool,
            json!(d.icecast_alert_stream_enabled),
        ),
        Field::new(
            "ICECAST_ALERT_PORT",
            "alert_stream",
            "Stream port",
            "Its own port, on the dashboard's interface: listeners connect to \
             http://<host>:<port><mount>. In Docker, publish it in the compose file too.",
            Kind::Integer { min: 1, max: 65_535 },
            json!(d.icecast_alert_port),
        )
        .requires("ICECAST_ALERT_STREAM_ENABLED"),
        Field::new(
            "ICECAST_ALERT_MOUNT",
            "alert_stream",
            "Mount",
            "The path listeners connect to. A missing leading slash is added.",
            text("/stream.ogg"),
            json!(d.icecast_alert_mount),
        )
        .requires("ICECAST_ALERT_STREAM_ENABLED"),
        Field::new(
            "ICECAST_ALERT_PUBLIC_URL",
            "alert_stream",
            "Public URL",
            "The address the dashboard links to for listening. Leave empty to hide the link.",
            text("https://stream.example.com/stream.ogg"),
            json!(d.icecast_alert_public_url),
        )
        .requires("ICECAST_ALERT_STREAM_ENABLED"),
        // Storage
        Field::new(
            "SHARED_STATE_DIR",
            "storage",
            "State folder",
            "Where logs, recordings and the alert database live. In Docker this is the /data \
             volume.",
            path(d.shared_state_dir.display().to_string()),
            path_value(&d.shared_state_dir),
        )
        .restart(),
        Field::new(
            "RECORDING_DIR",
            "storage",
            "Recordings folder",
            "Inside the state folder, unless given as an absolute path.",
            path(config::DEFAULT_RECORDING_DIR_NAME),
            json!(config::DEFAULT_RECORDING_DIR_NAME),
        ),
        Field::new(
            "ALERT_DATABASE_FILE",
            "storage",
            "Alert database",
            "The SQLite archive of received alerts. Inside the state folder, unless given as an \
             absolute path.",
            path(config::DEFAULT_ALERT_DATABASE_NAME),
            json!(config::DEFAULT_ALERT_DATABASE_NAME),
        )
        .restart(),
        Field::new(
            "STORAGE_SAVER_MODE",
            "storage",
            "Compress recordings",
            "Save recordings compressed instead of as WAV.",
            Kind::Bool,
            json!(d.storage_saver_mode),
        ),
        Field::new(
            "STORAGE_SAVER_MODE_EXT",
            "storage",
            "Compressed format",
            "The format compressed recordings are saved in.",
            Kind::Choice {
                options: choices(&[("mp3", "MP3"), ("ogg", "Ogg Opus")]),
            },
            json!(d.storage_saver_ext.extension()),
        )
        .requires("STORAGE_SAVER_MODE"),
        Field::new(
            "USE_PRE_POST_ROLL_FOR_RECORDINGS",
            "storage",
            "Add intro and outro to recordings",
            "Uses the intro and outro audio set under Relaying.",
            Kind::Bool,
            json!(d.use_pre_post_roll_for_recordings),
        ),
        Field::new(
            "EMIT_HEADER_TONES",
            "storage",
            "Include the header tones",
            "Off leaves a recording with the message audio alone -- no SAME header, no attention \
             tone, no end-of-message tone, and no Alert Ready signal.",
            Kind::Bool,
            json!(d.emit_header_tones),
        ),
        Field::new(
            "CUSTOM_HEADER_AUDIO",
            "storage",
            "Custom header audio",
            "Played where the tones an alert opens with would be, instead of them. The \
             end-of-message tone is unaffected. Separate from the intro and outro audio, which \
             still play outside it.",
            path("/app/header.mp3"),
            path_value(&d.custom_header_audio),
        )
        .requires("EMIT_HEADER_TONES"),
        // Logging
        Field::new(
            "RUST_LOG",
            "logging",
            "Log level",
            "How much the listener logs. A tracing filter directive written by hand is kept.",
            Kind::Choice {
                options: choices(&[
                    ("ERROR", "Errors only"),
                    ("WARN", "Warnings"),
                    ("INFO", "Information"),
                    ("DEBUG", "Debugging"),
                    ("TRACE", "Everything"),
                ]),
            },
            json!(d.log_level),
        )
        .restart(),
        Field::new(
            "SHOULD_LOG_ALL_ALERTS",
            "logging",
            "Log every alert",
            "Write every decoded alert to the alert log, not only those for your watched \
             locations.",
            Kind::Bool,
            json!(d.should_log_all_alerts),
        ),
        Field::new(
            "ALERT_LOG_FILE",
            "logging",
            "Log file name",
            "The listener's own log, rotated daily inside the state folder.",
            text("alerts.log"),
            json!(d.alert_log_file),
        )
        .restart(),
        Field::new(
            "DEDICATED_ALERT_LOG_FILE",
            "logging",
            "Alert log file",
            "A plain-text line for each received alert, inside the state folder.",
            path(config::DEFAULT_DEDICATED_ALERT_LOG_NAME),
            json!(config::DEFAULT_DEDICATED_ALERT_LOG_NAME),
        ),
        // Network
        Field::new(
            "MONITORING_BIND_ADDR",
            "network",
            "Listen address",
            "127.0.0.1 keeps the dashboard on this machine; 0.0.0.0 makes it reachable from \
             the network. In Docker, set this in .env.",
            text("0.0.0.0:8080"),
            json!(d.monitoring_bind_addr.to_string()),
        )
        .restart(),
        Field::new(
            "MONITORING_BIND_PORT",
            "network",
            "Listen port",
            "Overrides the port in the listen address. Docker publishes this port, so set it in \
             .env there.",
            Kind::Integer { min: 1, max: 65_535 },
            json!(d.monitoring_bind_port),
        )
        .restart(),
        Field::new(
            "MONITORING_ACTIVITY_WINDOW_SECS",
            "network",
            "Stream activity window",
            "Seconds without audio before the dashboard stops showing a stream as active.",
            Kind::Integer { min: 1, max: 86_400 },
            json!(d.monitoring_activity_window_secs),
        )
        .restart(),
        Field::new(
            "USE_REVERSE_PROXY",
            "network",
            "Behind a reverse proxy",
            "Marks the session cookie Secure and allows the proxy's origin.",
            Kind::Bool,
            json!(d.use_reverse_proxy),
        )
        .restart(),
        Field::new(
            "REVERSE_PROXY_URL",
            "network",
            "Dashboard host",
            "The host the dashboard is reached at from outside your network.",
            text("eas.example.com"),
            json!(d.reverse_proxy_url),
        )
        .requires("USE_REVERSE_PROXY"),
        Field::new(
            "WS_REVERSE_PROXY_URL",
            "network",
            "WebSocket host",
            "The host WebSocket connections arrive through.",
            text("eas-ws.example.com"),
            json!(d.ws_reverse_proxy_url),
        )
        .requires("USE_REVERSE_PROXY")
        .restart(),
        Field::new(
            "LOCAL_DEEPLINK_HOST",
            "network",
            "Link host",
            "Leave as auto unless this machine has a fixed address you want links back to the \
             dashboard to use.",
            text("auto"),
            json!(d.local_deeplink_host),
        ),
    ];

    fields.extend(component_fields());
    fields
}

/// What `GET /api/config/schema` returns: the fields with each one's environment value attached.
pub fn payload() -> Value {
    let mut fields = fields();
    for field in &mut fields {
        if field.json_only {
            continue;
        }
        let secret = matches!(field.kind, Kind::Secret);
        field.environment = Config::environment_setting(&field.key).map(|setting| {
            json!({
                "value": if secret { Value::Null } else { Value::String(setting.value) },
                "forced": setting.forced,
            })
        });
    }

    json!({
        "precedence": Config::precedence(),
        "config_path": crate::paths::config_json().display().to_string(),
        "groups": GROUPS,
        "fields": fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    #[test]
    fn the_schema_describes_exactly_the_keys_resolution_reads() {
        let fields = fields();
        let described: BTreeSet<String> = fields
            .iter()
            .filter(|field| !field.json_only)
            .map(|field| field.key.clone())
            .collect();
        let read = Config::keys_read_by_resolve();

        let undescribed: Vec<_> = read.difference(&described).collect();
        assert!(
            undescribed.is_empty(),
            "read by src/config.rs but missing from the configuration form: {undescribed:?}"
        );
        let unread: Vec<_> = described.difference(&read).collect();
        assert!(
            unread.is_empty(),
            "on the configuration form but never read by src/config.rs: {unread:?}"
        );

        for field in fields.iter().filter(|field| field.json_only) {
            assert!(
                !read.contains(&field.key),
                "{} is marked json_only but resolution reads it",
                field.key
            );
        }
    }

    #[test]
    fn fields_are_unique_grouped_and_depend_on_booleans() {
        let fields = fields();
        let groups: BTreeSet<&str> = GROUPS.iter().map(|group| group.id).collect();
        let by_key: HashMap<&str, &Field> = fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect();
        assert_eq!(by_key.len(), fields.len(), "a key is described twice");

        for field in &fields {
            assert!(groups.contains(field.group), "{} has no group", field.key);
            if let Some(parent) = field.requires {
                let parent = by_key
                    .get(parent)
                    .unwrap_or_else(|| panic!("{} requires unknown {parent}", field.key));
                assert!(
                    matches!(parent.kind, Kind::Bool),
                    "{} requires {}, which is not a boolean",
                    field.key,
                    parent.key
                );
            }
            if let Kind::Choice { options } = &field.kind {
                assert!(
                    options
                        .iter()
                        .any(|option| field.default == json!(option.value)),
                    "{}'s default {} is not one of its options",
                    field.key,
                    field.default
                );
            }
        }
    }

    /// Writing every default out explicitly has to be a valid config, or the form would offer a
    /// value the listener rejects.
    #[test]
    fn every_default_written_out_is_a_valid_config() {
        let mut candidate = serde_json::Map::new();
        for field in fields() {
            if !field.default.is_null() && !matches!(field.kind, Kind::Managed) {
                candidate.insert(field.key, field.default);
            }
        }

        Config::resolve_isolated(Value::Object(candidate)).expect("the defaults resolve");
    }

    #[test]
    fn the_example_config_only_uses_described_keys() {
        let example = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json"),
        )
        .expect("config.example.json");
        let example: Value = serde_json::from_str(&example).expect("example parses");
        let described: BTreeSet<String> = fields().into_iter().map(|field| field.key).collect();

        for key in example.as_object().expect("an object").keys() {
            if key.starts_with('_') {
                continue;
            }
            assert!(
                described.contains(key),
                "config.example.json uses {key}, which the form does not describe"
            );
        }
    }
}
