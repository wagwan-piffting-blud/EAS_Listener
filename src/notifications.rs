//! Where alert notifications go: the URLs in apprise.yml, and what the dashboard needs to edit
//! them -- the services Apprise can send to, and a test message to a single URL.
//!
//! The file is a list of Apprise URLs, one per line, optionally as YAML list items. Discord
//! webhooks are sent by the listener itself; every other URL is handed to the `apprise` binary.

use serde::Serialize;
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileContents {
    pub urls: Vec<String>,
    /// Lines that are neither URLs nor comments: YAML keys, tags. Saving from the dashboard
    /// drops them, which the page warns about.
    pub other_lines: usize,
}

pub fn parse(contents: &str) -> FileContents {
    let mut parsed = FileContents::default();
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let item = line.strip_prefix('-').map(str::trim_start).unwrap_or(line);
        let item = unquote(item);
        if item.contains("://") && !item.chars().any(char::is_whitespace) {
            parsed.urls.push(item.to_string());
        } else {
            parsed.other_lines += 1;
        }
    }
    parsed
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

pub fn render(urls: &[String]) -> String {
    let mut text = String::from(
        "# Where alert notifications are sent: one Apprise URL per line.\n\
         # Discord webhooks (discord://) are sent by the listener itself; everything else goes\n\
         # through Apprise. Edited from the dashboard, which keeps only the URLs.\n",
    );
    for url in urls {
        text.push_str("- ");
        text.push_str(url);
        text.push('\n');
    }
    text
}

/// Trims the URLs and refuses anything that could not have come back out of the file intact.
pub fn clean(urls: &[String]) -> Result<Vec<String>, String> {
    let mut cleaned = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        let url = url.trim();
        if url.is_empty() {
            continue;
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
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) => {}
            _ => {
                return Err(format!(
                    "URL {} is not an Apprise URL (service://...): {}",
                    index + 1,
                    crate::webhook::mask_url(url)
                ))
            }
        }
        cleaned.push(url.to_string());
    }
    Ok(cleaned)
}

/// Replaces the file, keeping the outgoing one as apprise.yml.bak.
pub fn write(path: &Path, urls: &[String]) -> Result<(), String> {
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
    crate::backend::write_atomic(path, render(urls).as_bytes())
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;
    info!(
        "Saved {} notification target(s) to {}",
        urls.len(),
        path.display()
    );
    Ok(())
}

static SERVICES: Mutex<Option<(PathBuf, Arc<Value>)>> = Mutex::const_new(None);

/// Every service the installed Apprise can send to, trimmed to what the dashboard's URL builder
/// uses. Cached per binary, since `--schema` takes a moment and does not change while it runs.
pub async fn services() -> Result<Arc<Value>, String> {
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
    if let Err(err) = clean(&[url.to_string()]) {
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
        pub urls: Vec<String>,
        /// Setup only: APPRISE_CONFIG_PATH from the configuration being written alongside.
        #[serde(default)]
        pub path: Option<String>,
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
                "urls": contents.urls,
                "other_lines": contents.other_lines,
            }))
            .into_response(),
            Err(err) => failed(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not read {}: {err}", path.display()),
            ),
        }
    }

    pub fn save(path: &Path, urls: &[String]) -> Response {
        let urls = match clean(urls) {
            Ok(urls) => urls,
            Err(err) => return failed(StatusCode::BAD_REQUEST, err),
        };
        match write(path, &urls) {
            Ok(()) => Json(json!({
                "ok": true,
                "path": path.display().to_string(),
                "count": urls.len(),
            }))
            .into_response(),
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
            parsed.urls,
            vec!["discord://123/abc", "tgram://bot/chat", "ntfys://topic"]
        );
        assert_eq!(parsed.other_lines, 2);
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let urls = vec![
            "discord://1/two".to_string(),
            "pover://user@token/%23group".to_string(),
        ];
        let parsed = parse(&render(&urls));
        assert_eq!(parsed.urls, urls);
        assert_eq!(parsed.other_lines, 0);
    }

    #[test]
    fn cleaning_drops_blanks_and_refuses_what_is_not_a_url() {
        assert_eq!(
            clean(&[" json://host/ ".to_string(), "".to_string()]).unwrap(),
            vec!["json://host/"]
        );
        assert!(clean(&["just text".to_string()]).is_err());
        assert!(clean(&["://nothing".to_string()]).is_err());
        assert!(clean(&["mailto://a b".to_string()]).is_err());
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
        write(&path, &["json://one/".to_string()]).expect("first write");
        write(&path, &["json://two/".to_string()]).expect("second write");
        assert_eq!(read(&path).unwrap().urls, vec!["json://two/"]);
        let backup = dir.path().join("apprise.yml.bak");
        assert_eq!(read(&backup).unwrap().urls, vec!["json://one/"]);
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
