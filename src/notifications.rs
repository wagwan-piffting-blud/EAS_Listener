//! Where alert notifications go: the URLs in apprise.yml, and what the dashboard needs to edit
//! them -- the services Apprise can send to, and a test message to a single URL.
//!
//! The file is a YAML list. An item that is a bare URL gets every alert; an item with `url:` can
//! also name the `sources:` and SAME `events:` it is sent, so one webhook can take IPAWS and
//! another NAAD. A file that is not a YAML list is read the old way, a URL per line. Discord
//! webhooks are sent by the listener itself; every other URL is handed to the `apprise` binary.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

const FILE_NAME: &str = "apprise.yml";
const SCHEMA_TIMEOUT: Duration = Duration::from_secs(20);
const TEST_TIMEOUT: Duration = Duration::from_secs(45);
const TEST_TITLE: &str = "EAS Listener test notification";

/// The configured path, looked inside when it is a directory -- what Docker creates for a
/// bind-mounted apprise.yml that does not exist on the host yet.
pub fn resolve(configured: &str) -> PathBuf {
    let path = if configured.trim().is_empty() {
        crate::paths::apprise_config()
    } else {
        PathBuf::from(configured.trim())
    };
    if path.is_dir() {
        path.join(FILE_NAME)
    } else {
        path
    }
}

/// Where an alert came from, as a route names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Decoded from a monitored stream, the 1050 Hz tone included.
    OffAir,
    Ipaws,
    Wea,
    /// CAP-CP from NAAD: Alert Ready / NPAS.
    Naad,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::OffAir, Source::Ipaws, Source::Wea, Source::Naad];

    pub fn key(self) -> &'static str {
        match self {
            Source::OffAir => "offair",
            Source::Ipaws => "ipaws",
            Source::Wea => "wea",
            Source::Naad => "naad",
        }
    }

    /// Judged by the sender ID in the alert's header, which every CAP feed sets to its own.
    pub fn of_raw_header(raw_header: &str) -> Source {
        use crate::cap::{
            CAP_HEADER_SOURCE_MARKER_CAP, CAP_HEADER_SOURCE_MARKER_NAAD,
            CAP_HEADER_SOURCE_MARKER_WEA,
        };
        match crate::cap::cap_feed_of_raw_header(raw_header) {
            Some(CAP_HEADER_SOURCE_MARKER_CAP) => Source::Ipaws,
            Some(CAP_HEADER_SOURCE_MARKER_WEA) => Source::Wea,
            Some(CAP_HEADER_SOURCE_MARKER_NAAD) => Source::Naad,
            _ => Source::OffAir,
        }
    }
}

/// One URL and the alerts it is sent. Empty `sources` or `events` means every one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub url: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
}

impl Target {
    pub fn every_alert(url: impl Into<String>) -> Self {
        Target {
            url: url.into(),
            ..Target::default()
        }
    }

    pub fn accepts(&self, source: Source, event_code: &str) -> bool {
        let event_code = event_code.trim();
        (self.sources.is_empty() || self.sources.iter().any(|key| key == source.key()))
            && (self.events.is_empty()
                || self
                    .events
                    .iter()
                    .any(|code| code.eq_ignore_ascii_case(event_code)))
    }

    fn routed(&self) -> bool {
        !self.sources.is_empty() || !self.events.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileContents {
    pub targets: Vec<Target>,
    /// Lines that are neither URLs nor comments: YAML keys, tags. Saving from the dashboard
    /// drops them, which the page warns about.
    pub other_lines: usize,
}

pub fn parse(contents: &str) -> FileContents {
    match serde_norway::from_str::<Value>(contents) {
        Ok(Value::Array(items)) => parse_items(&items),
        _ => parse_lines(contents),
    }
}

fn parse_items(items: &[Value]) -> FileContents {
    let mut parsed = FileContents::default();
    let strings = |value: &Value| -> Vec<String> {
        match value {
            Value::String(one) => one
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect(),
            Value::Array(many) => many
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.trim().to_string()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                })
                .filter(|item| !item.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    };
    for item in items {
        let target = match item {
            Value::String(url) => Some(Target::every_alert(url.trim())),
            Value::Object(fields) => fields
                .get("url")
                .and_then(Value::as_str)
                .map(|url| Target {
                    url: url.trim().to_string(),
                    sources: fields.get("sources").map(strings).unwrap_or_default(),
                    events: fields.get("events").map(strings).unwrap_or_default(),
                }),
            _ => None,
        };
        match target {
            Some(target) if looks_like_url(&target.url) => parsed.targets.push(target),
            _ => parsed.other_lines += 1,
        }
    }
    parsed
}

fn parse_lines(contents: &str) -> FileContents {
    let mut parsed = FileContents::default();
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let item = line.strip_prefix('-').map(str::trim_start).unwrap_or(line);
        let item = unquote(item);
        if looks_like_url(item) {
            parsed.targets.push(Target::every_alert(item));
        } else {
            parsed.other_lines += 1;
        }
    }
    parsed
}

fn looks_like_url(item: &str) -> bool {
    item.contains("://") && !item.chars().any(char::is_whitespace)
}

fn unquote(item: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = item
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    item
}

/// A missing file is an empty list, which is what a new install has.
pub fn read(path: &Path) -> std::io::Result<FileContents> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(parse(&contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FileContents::default()),
        Err(err) => Err(err),
    }
}

/// JSON strings and arrays are valid YAML, which spares quoting rules of its own.
pub fn render(targets: &[Target]) -> String {
    let mut text = String::from(
        "# Where alert notifications are sent: one Apprise URL per item.\n\
         # A bare URL gets every alert. To send one only some alerts, give it as\n\
         #   - url: \"service://...\"\n\
         #     sources: [\"ipaws\", \"wea\", \"naad\", \"offair\"]   # any of these; omit for all\n\
         #     events: [\"TOR\", \"SVR\"]                          # SAME event codes; omit for all\n\
         # Discord webhooks (discord://) are sent by the listener itself; everything else goes\n\
         # through Apprise. Edited from the dashboard, which keeps only the URLs and routes.\n",
    );
    for target in targets {
        let url = Value::String(target.url.clone());
        if !target.routed() {
            text.push_str(&format!("- {url}\n"));
            continue;
        }
        text.push_str(&format!("- url: {url}\n"));
        if !target.sources.is_empty() {
            text.push_str(&format!("  sources: {}\n", json!(target.sources)));
        }
        if !target.events.is_empty() {
            text.push_str(&format!("  events: {}\n", json!(target.events)));
        }
    }
    text
}

/// Trims every target and refuses anything that could not have come back out of the file
/// intact: a URL that is not one, a source the listener does not know, an event code that is not
/// three letters or digits.
pub fn clean(targets: &[Target]) -> Result<Vec<Target>, String> {
    let mut cleaned = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        let url = clean_url(index, &target.url)?;
        if url.is_empty() {
            continue;
        }
        let mut sources = Vec::new();
        for source in &target.sources {
            let source = source.trim().to_ascii_lowercase();
            if !Source::ALL.iter().any(|known| known.key() == source) {
                return Err(format!(
                    "URL {} is routed to an unknown source '{source}'. The sources are offair, \
                     ipaws, wea and naad.",
                    index + 1
                ));
            }
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        let mut events = Vec::new();
        for code in &target.events {
            let code = code.trim().to_ascii_uppercase();
            if code.is_empty() {
                continue;
            }
            if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(format!(
                    "URL {} lists '{code}', which is not a SAME event code (three letters, such \
                     as TOR).",
                    index + 1
                ));
            }
            if !events.contains(&code) {
                events.push(code);
            }
        }
        // Every source listed is the same as none listed, and reads more plainly as none.
        if sources.len() == Source::ALL.len() {
            sources.clear();
        }
        cleaned.push(Target {
            url,
            sources,
            events,
        });
    }
    Ok(cleaned)
}

fn clean_url(index: usize, url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    if url.chars().any(char::is_whitespace) {
        return Err(format!(
            "URL {} contains a space or line break. Encode it as %20.",
            index + 1
        ));
    }
    match url.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && !rest.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            Ok(url.to_string())
        }
        _ => Err(format!(
            "URL {} is not an Apprise URL (service://...): {}",
            index + 1,
            crate::webhook::mask_url(url)
        )),
    }
}

/// Fetches Apprise when a URL needs it and it is missing, so a service added from the dashboard
/// works from the next alert without anyone running the fetch script.
pub async fn ensure_apprise() -> Result<(), String> {
    crate::components::ensure(&crate::components::APPRISE)
        .await
        .map(|_| ())
}

pub fn needs_apprise(targets: &[Target]) -> bool {
    targets
        .iter()
        .any(|target| !crate::webhook::is_native_discord(&target.url))
}

/// Replaces the file, keeping the outgoing one as apprise.yml.bak.
pub fn write(path: &Path, targets: &[Target]) -> Result<(), String> {
    if path.exists() {
        let mut backup = path.as_os_str().to_os_string();
        backup.push(".bak");
        if let Err(err) = std::fs::copy(path, PathBuf::from(backup)) {
            warn!("Could not back up {}: {}", path.display(), err);
            return Err(format!(
                "Could not back up {}; nothing was written.",
                path.display()
            ));
        }
    } else if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    crate::backend::write_atomic(path, render(targets).as_bytes())
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;
    info!(
        "Saved {} notification target(s) to {}",
        targets.len(),
        path.display()
    );
    Ok(())
}

static SERVICES: Mutex<Option<(PathBuf, Arc<Value>)>> = Mutex::const_new(None);

/// Every service the installed Apprise can send to, trimmed to what the dashboard's URL builder
/// uses. Cached per binary, since `--schema` takes a moment and does not change while it runs.
/// Asking for the list is how the dashboard starts adding a service, so a missing Apprise is
/// fetched first.
pub async fn services() -> Result<Arc<Value>, String> {
    ensure_apprise().await?;
    let binary = crate::components::apprise();
    let mut cache = SERVICES.lock().await;
    if let Some((cached_for, services)) = cache.as_ref() {
        if *cached_for == binary {
            return Ok(services.clone());
        }
    }

    let mut command = Command::new(&binary);
    command
        .arg("--schema")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(SCHEMA_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(output)) => {
            return Err(format!(
                "Apprise could not list its services: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
        Ok(Err(err)) => {
            return Err(format!(
                "Apprise is not installed ({} could not be run: {err}).",
                binary.display()
            ))
        }
        Err(_) => return Err("Apprise took too long to list its services.".to_string()),
    };
    let raw: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("Apprise's service list could not be read: {err}"))?;
    let services = Arc::new(trim_schema(&raw));
    *cache = Some((binary, services.clone()));
    Ok(services)
}

/// Keeps what builds a URL: templates, tokens and query arguments per service. The arguments
/// every service accepts (timeouts, format, retries) are listed once as `common_args`.
pub fn trim_schema(raw: &Value) -> Value {
    let enabled: Vec<&Value> = raw["schemas"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter(|service| service["enabled"].as_bool().unwrap_or(true))
                .collect()
        })
        .unwrap_or_default();

    let arg_keys = |service: &Value| -> BTreeSet<String> {
        service["details"]["args"]
            .as_object()
            .map(|args| {
                args.iter()
                    .filter(|(_, spec)| spec.get("alias_of").is_none())
                    .map(|(key, _)| key.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut common: Option<BTreeSet<String>> = None;
    for service in &enabled {
        let keys = arg_keys(service);
        common = Some(match common {
            None => keys,
            Some(seen) => seen.intersection(&keys).cloned().collect(),
        });
    }
    let common = common.unwrap_or_default();

    let spec_without_map_to = |spec: &Value| -> Value {
        let mut spec = spec.clone();
        if let Some(object) = spec.as_object_mut() {
            object.remove("map_to");
        }
        spec
    };

    let mut common_args = Map::new();
    let mut services = Vec::with_capacity(enabled.len());
    for service in &enabled {
        let details = &service["details"];
        let mut args = Map::new();
        if let Some(all) = details["args"].as_object() {
            for (key, spec) in all {
                if spec.get("alias_of").is_some() {
                    continue;
                }
                if common.contains(key) {
                    common_args
                        .entry(key.clone())
                        .or_insert_with(|| spec_without_map_to(spec));
                } else {
                    args.insert(key.clone(), spec_without_map_to(spec));
                }
            }
        }
        let tokens: Map<String, Value> = details["tokens"]
            .as_object()
            .map(|all| {
                all.iter()
                    .filter(|(_, spec)| spec.get("alias_of").is_none())
                    .map(|(key, spec)| (key.clone(), spec_without_map_to(spec)))
                    .collect()
            })
            .unwrap_or_default();
        let schemes: Vec<Value> = ["protocols", "secure_protocols"]
            .iter()
            .flat_map(|key| service[*key].as_array().cloned().unwrap_or_default())
            .collect();
        services.push(json!({
            "name": service["service_name"],
            "service_url": service["service_url"],
            "setup_url": service["setup_url"],
            "schemes": schemes,
            "secure_schemes": service["secure_protocols"].as_array().cloned().unwrap_or_default(),
            "attachments": service["attachment_support"].as_bool().unwrap_or(false),
            "templates": details["templates"],
            "tokens": tokens,
            "args": args,
        }));
    }
    services.sort_by_key(|service| {
        service["name"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
    });

    json!({
        "version": raw["version"],
        "common_args": common_args,
        "services": services,
    })
}

#[derive(Debug, Serialize)]
pub struct TestOutcome {
    pub ok: bool,
    pub message: String,
}

/// Sends one short message to one URL, the same way an alert would reach it.
pub async fn send_test(url: &str, station: &str) -> TestOutcome {
    let url = url.trim();
    if let Err(err) = clean_url(0, url) {
        return TestOutcome {
            ok: false,
            message: err,
        };
    }
    let from = if station.trim().is_empty() {
        String::new()
    } else {
        format!(" ({})", station.trim())
    };
    let body = format!(
        "This is a test from EAS Listener{from}. If you can read this, alert notifications will \
         reach this service."
    );

    if crate::webhook::is_native_discord(url) {
        return match crate::webhook::send_discord_test(url, TEST_TITLE, &body).await {
            Ok(()) => TestOutcome {
                ok: true,
                message: "Sent to Discord.".to_string(),
            },
            Err(err) => TestOutcome {
                ok: false,
                message: err,
            },
        };
    }

    if let Err(err) = ensure_apprise().await {
        return TestOutcome {
            ok: false,
            message: format!("{err}. Discord webhooks work without Apprise."),
        };
    }
    let binary = crate::components::apprise();
    let mut command = Command::new(&binary);
    command
        .arg("--title")
        .arg(TEST_TITLE)
        .arg("--body")
        .arg(&body)
        .arg("--input-format")
        .arg("text")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(TEST_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => TestOutcome {
            ok: true,
            message: "Apprise sent it.".to_string(),
        },
        Ok(Ok(output)) => {
            let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if detail.is_empty() {
                detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
            if detail.is_empty() {
                detail = format!("Apprise exited with {}.", output.status);
            }
            TestOutcome {
                ok: false,
                message: detail.chars().take(800).collect(),
            }
        }
        Ok(Err(err)) => TestOutcome {
            ok: false,
            message: format!(
                "Apprise is not installed ({} could not be run: {err}). Discord webhooks work \
                 without it.",
                binary.display()
            ),
        },
        Err(_) => TestOutcome {
            ok: false,
            message: format!("No answer within {} seconds.", TEST_TIMEOUT.as_secs()),
        },
    }
}

/// The HTTP side, shared by the dashboard and first-run setup, which differ only in how they
/// authenticate and which file they point at.
pub mod api {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::Json;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct SaveBody {
        #[serde(default)]
        pub targets: Vec<Target>,
        /// A page from before routes: every URL gets every alert.
        #[serde(default)]
        pub urls: Vec<String>,
        /// Setup only: APPRISE_CONFIG_PATH from the configuration being written alongside.
        #[serde(default)]
        pub path: Option<String>,
    }

    impl SaveBody {
        pub fn targets(&self) -> Vec<Target> {
            let mut targets = self.targets.clone();
            targets.extend(self.urls.iter().map(Target::every_alert));
            targets
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct TestBody {
        pub url: String,
    }

    fn failed(status: StatusCode, error: String) -> Response {
        (status, Json(json!({ "ok": false, "error": error }))).into_response()
    }

    pub fn view(path: &Path) -> Response {
        match read(path) {
            Ok(contents) => Json(json!({
                "path": path.display().to_string(),
                "targets": contents.targets,
                "other_lines": contents.other_lines,
            }))
            .into_response(),
            Err(err) => failed(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not read {}: {err}", path.display()),
            ),
        }
    }

    /// A save that adds a service Apprise sends to starts fetching Apprise, so the next alert
    /// reaches it; the page does not wait for the download.
    pub fn save(path: &Path, targets: &[Target]) -> Response {
        let targets = match clean(targets) {
            Ok(targets) => targets,
            Err(err) => return failed(StatusCode::BAD_REQUEST, err),
        };
        match write(path, &targets) {
            Ok(()) => {
                if needs_apprise(&targets) {
                    tokio::spawn(async {
                        if let Err(err) = ensure_apprise().await {
                            warn!("Notifications other than Discord will not be sent: {err}");
                        }
                    });
                }
                Json(json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "count": targets.len(),
                }))
                .into_response()
            }
            Err(err) => failed(StatusCode::INTERNAL_SERVER_ERROR, err),
        }
    }

    /// Without Apprise there is no service list, and the page offers only pasting URLs, which
    /// still covers Discord. That is an answer, not a failure, so it is not an error status.
    pub async fn services_response() -> Response {
        match services().await {
            Ok(services) => Json((*services).clone()).into_response(),
            Err(err) => Json(json!({ "available": false, "error": err })).into_response(),
        }
    }

    pub async fn test_response(url: &str, station: &str) -> Response {
        Json(send_test(url, station).await).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(parsed: &FileContents) -> Vec<&str> {
        parsed
            .targets
            .iter()
            .map(|target| target.url.as_str())
            .collect()
    }

    fn every(urls: &[&str]) -> Vec<Target> {
        urls.iter().map(|url| Target::every_alert(*url)).collect()
    }

    #[test]
    fn the_file_is_read_as_urls_with_everything_else_counted() {
        let parsed = parse(
            "# comment\n\
             discord://123/abc\n\
             - tgram://bot/chat\n\
             -   \"ntfys://topic\"\n\
             urls:\n\
             \x20 tag: ops\n\
             \n",
        );
        assert_eq!(
            urls(&parsed),
            vec!["discord://123/abc", "tgram://bot/chat", "ntfys://topic"]
        );
        assert_eq!(parsed.other_lines, 2);
    }

    #[test]
    fn a_yaml_list_carries_routes_and_counts_what_is_not_a_target() {
        let parsed = parse(
            "# comment\n\
             - discord://1/everything\n\
             - url: tgram://bot/chat\n\
             \x20 sources: [ipaws, wea]\n\
             \x20 events: [TOR, svr]\n\
             - url: \"json://host/?a=b&c=d\"\n\
             \x20 sources: naad, offair\n\
             - tag: ops\n\
             - not a url\n",
        );
        assert_eq!(
            parsed.targets,
            vec![
                Target::every_alert("discord://1/everything"),
                Target {
                    url: "tgram://bot/chat".into(),
                    sources: vec!["ipaws".into(), "wea".into()],
                    events: vec!["TOR".into(), "svr".into()],
                },
                Target {
                    url: "json://host/?a=b&c=d".into(),
                    sources: vec!["naad".into(), "offair".into()],
                    events: vec![],
                },
            ]
        );
        assert_eq!(parsed.other_lines, 2);
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let targets = vec![
            Target::every_alert("discord://1/two"),
            Target::every_alert("pover://user@token/%23group"),
            Target {
                url: "json://host/path?x=1&y=#frag".into(),
                sources: vec!["naad".into()],
                events: vec![],
            },
            Target {
                url: "mailto://user:p%40ss@example.com".into(),
                sources: vec![],
                events: vec!["TOR".into(), "EAN".into()],
            },
        ];
        let parsed = parse(&render(&targets));
        assert_eq!(parsed.targets, targets);
        assert_eq!(parsed.other_lines, 0);
    }

    #[test]
    fn cleaning_drops_blanks_and_refuses_what_is_not_a_url() {
        assert_eq!(
            clean(&every(&[" json://host/ ", ""])).unwrap(),
            every(&["json://host/"])
        );
        assert!(clean(&every(&["just text"])).is_err());
        assert!(clean(&every(&["://nothing"])).is_err());
        assert!(clean(&every(&["mailto://a b"])).is_err());
    }

    #[test]
    fn cleaning_normalises_routes_and_refuses_unknown_ones() {
        let target = |sources: &[&str], events: &[&str]| Target {
            url: "json://host/".into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            events: events.iter().map(|e| e.to_string()).collect(),
        };
        assert_eq!(
            clean(&[target(&[" IPAWS ", "ipaws"], &["tor", " SVR", ""])]).unwrap(),
            vec![target(&["ipaws"], &["TOR", "SVR"])]
        );
        // All four sources is no restriction, and is saved as none.
        assert_eq!(
            clean(&[target(&["offair", "ipaws", "wea", "naad"], &[])]).unwrap(),
            vec![target(&[], &[])]
        );
        assert!(clean(&[target(&["npas"], &[])]).is_err());
        assert!(clean(&[target(&[], &["TORNADO"])]).is_err());
    }

    #[test]
    fn a_route_takes_only_its_sources_and_events() {
        let naad_only = Target {
            url: "json://ca/".into(),
            sources: vec!["naad".into()],
            events: vec![],
        };
        assert!(naad_only.accepts(Source::Naad, "TOR"));
        assert!(!naad_only.accepts(Source::Ipaws, "TOR"));
        assert!(!naad_only.accepts(Source::OffAir, "RWT"));

        let tornadoes = Target {
            url: "json://us/".into(),
            sources: vec!["ipaws".into(), "offair".into()],
            events: vec!["TOR".into()],
        };
        assert!(tornadoes.accepts(Source::OffAir, "tor"));
        assert!(!tornadoes.accepts(Source::OffAir, "SVR"));
        assert!(!tornadoes.accepts(Source::Wea, "TOR"));

        assert!(Target::every_alert("json://all/").accepts(Source::Wea, "RMT"));
    }

    #[test]
    fn the_source_comes_from_the_header_sender() {
        let header = |sender: &str| format!("ZCZC-CIV-TOR-031055+0030-2681200-{sender}-");
        assert_eq!(Source::of_raw_header(&header("IPAWSCAP")), Source::Ipaws);
        assert_eq!(Source::of_raw_header(&header("IPAWSWEA")), Source::Wea);
        assert_eq!(Source::of_raw_header(&header("NAADSCAP")), Source::Naad);
        assert_eq!(Source::of_raw_header(&header("KOAX/NWS")), Source::OffAir);
    }

    #[test]
    fn a_directory_in_place_of_the_file_is_looked_inside() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mounted = dir.path().join("apprise.yml");
        std::fs::create_dir(&mounted).expect("mkdir");
        assert_eq!(
            resolve(&mounted.to_string_lossy()),
            mounted.join("apprise.yml")
        );
        let file = dir.path().join("targets.yml");
        assert_eq!(resolve(&file.to_string_lossy()), file);
    }

    #[test]
    fn a_save_keeps_the_previous_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("apprise.yml");
        write(&path, &every(&["json://one/"])).expect("first write");
        write(&path, &every(&["json://two/"])).expect("second write");
        assert_eq!(urls(&read(&path).unwrap()), vec!["json://two/"]);
        let backup = dir.path().join("apprise.yml.bak");
        assert_eq!(urls(&read(&backup).unwrap()), vec!["json://one/"]);
        assert_eq!(
            read(&dir.path().join("absent.yml")).unwrap(),
            FileContents::default()
        );
    }

    #[test]
    fn the_schema_is_trimmed_to_what_builds_a_url() {
        let raw = json!({
            "version": "1.0",
            "schemas": [
                {
                    "service_name": "Beta",
                    "enabled": true,
                    "protocols": ["beta"],
                    "secure_protocols": ["betas"],
                    "attachment_support": true,
                    "details": {
                        "templates": ["{schema}://{host}"],
                        "tokens": { "host": { "name": "Host", "map_to": "host", "type": "string" } },
                        "args": {
                            "verify": { "name": "Verify", "type": "bool", "map_to": "verify" },
                            "to": { "alias_of": "host" },
                            "mode": { "name": "Mode", "type": "string", "map_to": "mode" }
                        }
                    }
                },
                {
                    "service_name": "alpha",
                    "protocols": null,
                    "secure_protocols": ["alpha"],
                    "details": {
                        "templates": ["{schema}://{token}"],
                        "tokens": { "token": { "name": "Token", "type": "string" } },
                        "args": { "verify": { "name": "Verify", "type": "bool" } }
                    }
                },
                {
                    "service_name": "Desktop",
                    "enabled": false,
                    "details": { "templates": [], "tokens": {}, "args": {} }
                }
            ]
        });
        let trimmed = trim_schema(&raw);
        let services = trimmed["services"].as_array().unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services[0]["name"], "alpha");
        assert_eq!(services[1]["schemes"], json!(["beta", "betas"]));
        assert_eq!(services[1]["secure_schemes"], json!(["betas"]));
        assert!(services[1]["tokens"]["host"].get("map_to").is_none());
        assert_eq!(
            services[1]["args"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["mode"]
        );
        assert!(trimmed["common_args"]["verify"].is_object());
    }
}
