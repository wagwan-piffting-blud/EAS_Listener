use crate::filter;
use crate::state::ActiveAlert;
use crate::Config;
use chrono::Local;
use lazy_static::lazy_static;
use reqwest::{multipart, Client};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tokio::process::Command;
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct SameUsLookup {
    #[serde(rename = "ORGS")]
    orgs: HashMap<String, String>,
    #[serde(rename = "EVENTS")]
    events: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct WebhookRuntimeConfig {
    apprise_config_path: String,
    station_name: String,
    stream_index_map: HashMap<String, usize>,
}

impl WebhookRuntimeConfig {
    fn from_config(config: &Config) -> Self {
        Self {
            apprise_config_path: config.apprise_config_path.clone(),
            station_name: config.eas_relay_name.clone(),
            stream_index_map: config
                .icecast_stream_urls
                .iter()
                .enumerate()
                .map(|(idx, url)| (url.clone(), idx + 1))
                .collect(),
        }
    }

    fn from_disk_or_default() -> Self {
        let config_path = crate::paths::config_json();
        let config = Config::from_config_json(&config_path).unwrap_or_else(|err| {
            eprintln!(
                "Warning: failed to load {} for webhook config: {:?}. Using built-in safe defaults.",
                config_path.display(),
                err
            );
            Config::safe_internal_defaults()
        });
        Self::from_config(&config)
    }
}

lazy_static! {
    static ref WEBHOOK_RUNTIME_CONFIG: RwLock<WebhookRuntimeConfig> =
        RwLock::new(WebhookRuntimeConfig::from_disk_or_default());
    static ref github_url: String =
        "https://github.com/wagwan-piffting-blud/EAS_Listener".to_string();
    static ref same_us_lookup: SameUsLookup =
        serde_json::from_str(include_str!("../include/same-us.json")).expect("parse same-us.json");
}

fn runtime_config_snapshot() -> WebhookRuntimeConfig {
    WEBHOOK_RUNTIME_CONFIG
        .read()
        .expect("webhook runtime config lock poisoned")
        .clone()
}

pub fn apply_runtime_config(config: &Config) {
    let mut guard = WEBHOOK_RUNTIME_CONFIG
        .write()
        .expect("webhook runtime config lock poisoned");
    *guard = WebhookRuntimeConfig::from_config(config);
}

pub fn determine_event_title(event_code: &str) -> String {
    let key = event_code.trim().to_ascii_uppercase();
    match same_us_lookup.events.get(key.as_str()) {
        Some(title) => {
            let trimmed = title.trim();
            let without_article = trimmed
                .strip_prefix("an ")
                .or_else(|| trimmed.strip_prefix("a "))
                .or_else(|| trimmed.strip_prefix("An "))
                .or_else(|| trimmed.strip_prefix("A "))
                .unwrap_or(trimmed)
                .trim();
            if without_article.is_empty() {
                event_code.to_string()
            } else {
                without_article.to_string()
            }
        }
        None => event_code.to_string(),
    }
}

pub fn determine_originator_name(originator_code: &str) -> String {
    let key = originator_code.trim().to_ascii_uppercase();
    same_us_lookup
        .orgs
        .get(key.as_str())
        .cloned()
        .unwrap_or_else(|| originator_code.to_string())
}

pub fn a_or_an(word: &str) -> &str {
    let first_char = word.chars().next().unwrap_or(' ').to_ascii_lowercase();
    match first_char {
        'a' | 'e' | 'i' | 'o' | 'u' => "An",
        _ => "A",
    }
}

/// Discord webhooks are posted by the listener itself, with an embed and the recording attached;
/// Apprise gets every other URL.
pub fn is_native_discord(url: &str) -> bool {
    url.trim().starts_with("discord://")
}

/// The webhook endpoint for `discord://[botname@]id/token[/][?args]`, and the bot name if one
/// was given. Apprise-only query arguments are dropped.
fn discord_endpoint(url: &str) -> Option<(String, Option<String>)> {
    let rest = url.trim().strip_prefix("discord://")?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (botname, path) = match rest.split_once('@') {
        Some((name, path)) => (Some(name), path),
        None => (None, rest),
    };
    let mut parts = path.trim_end_matches('/').split('/');
    let (id, token) = (parts.next()?, parts.next()?);
    if id.is_empty() || token.is_empty() || parts.next().is_some() {
        return None;
    }
    let botname = botname.map(percent_decode).filter(|name| !name.is_empty());
    Some((
        format!("https://discord.com/api/webhooks/{id}/{token}"),
        botname,
    ))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Everything after the scheme reduced to its last four characters, for logs and errors.
pub fn mask_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let chars: Vec<char> = rest.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
            if chars.len() <= 8 {
                format!("{scheme}://****")
            } else {
                format!("{scheme}://****{tail}")
            }
        }
        None => "****".to_string(),
    }
}

pub async fn send_discord_test(url: &str, title: &str, body: &str) -> Result<(), String> {
    let (endpoint, botname) = discord_endpoint(url)
        .ok_or_else(|| "Expected discord://webhook_id/webhook_token.".to_string())?;
    let mut payload = json!({
        "embeds": [{ "title": title, "description": body, "color": 0x2e7d32 }]
    });
    if let Some(name) = botname {
        payload["username"] = json!(name);
    }
    let response = Client::new()
        .post(&endpoint)
        .timeout(std::time::Duration::from_secs(20))
        .json(&payload)
        .send()
        .await
        .map_err(|err| format!("Discord could not be reached: {err}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Err(format!(
        "Discord answered {status}: {}",
        truncate_for_log(text.trim(), 400)
    ))
}

pub async fn send_alert_webhook(
    url: &str,
    alert: &ActiveAlert,
    _dsame_text: &str,
    protocol_header: Option<&str>,
    recording_path: Option<PathBuf>,
) {
    let runtime_config = runtime_config_snapshot();
    let config_path = crate::notifications::resolve(&runtime_config.apprise_config_path);
    let apprise_urls_from_config_array: Vec<String> = match fs::File::open(&config_path) {
        Ok(mut file) => {
            let mut contents = String::new();
            if let Err(err) = file.read_to_string(&mut contents) {
                warn!(
                    "Failed to read AppRise config file at '{}': {}",
                    config_path.display(),
                    err
                );
                return;
            }
            crate::notifications::parse(&contents).urls
        }
        Err(err) => {
            warn!(
                "Failed to open AppRise config file at '{}': {}",
                config_path.display(),
                err
            );
            return;
        }
    };
    let data = &alert.data;
    let description = data
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let instructions = data
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let event_code = &data.event_code;
    let event_title = determine_event_title(event_code);
    let originator_code = &data.originator;
    let originator = determine_originator_name(originator_code);
    let apprise_title = format!(
        "{} {} has just been issued/received",
        a_or_an(&event_title),
        event_title.as_str()
    );
    let received_timestamp = Local::now().to_rfc3339();
    let attachment_path = if let Some(path) = recording_path {
        match tokio::fs::metadata(&path).await {
            Ok(_) => Some(path),
            Err(err) => {
                warn!(
                    "Recording attachment unavailable at '{}': {}",
                    path.display(),
                    err
                );
                None
            }
        }
    } else {
        None
    };
    let discord_embed_body = build_discord_embed_body(
        url,
        &event_title,
        event_code,
        &originator,
        &received_timestamp,
        &data.eas_text,
        protocol_header,
        description,
        instructions,
    );
    let markdown_body = build_markdown_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        protocol_header,
        description,
        instructions,
    );
    let html_body = build_html_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        protocol_header,
        description,
        instructions,
    );
    let text_body = build_plain_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        protocol_header,
        description,
        instructions,
    );

    let discord_urls: Vec<&str> = apprise_urls_from_config_array
        .iter()
        .map(|url| url.trim())
        .filter(|url| is_native_discord(url))
        .collect();

    if !discord_urls.is_empty() {
        let client = Client::new();
        let attachment_bytes = if let Some(path) = attachment_path.as_ref() {
            match tokio::fs::read(path).await {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    warn!(
                        "Failed to read recording attachment at '{}': {}",
                        path.display(),
                        err
                    );
                    None
                }
            }
        } else {
            None
        };

        let prepared_attachment: Option<(Vec<u8>, String)> =
            match (attachment_path.as_ref(), attachment_bytes) {
                (Some(path), Some(bytes)) => Some(prepare_discord_attachment(path, bytes).await),
                _ => None,
            };

        for discord_url in discord_urls {
            let Some((url, botname)) = discord_endpoint(discord_url) else {
                warn!(
                    "Skipping Discord URL '{}': expected discord://webhook_id/webhook_token",
                    mask_url(discord_url)
                );
                continue;
            };
            let mut payload_value = json!({ "embeds": [discord_embed_body.clone()] });
            if let Some(name) = botname {
                payload_value["username"] = json!(name);
            }
            let validation_errors = validate_discord_payload(&payload_value);
            if !validation_errors.is_empty() {
                warn!(
                    "Discord payload preflight validation found {} issue(s) for '{}': {}",
                    validation_errors.len(),
                    discord_url,
                    validation_errors.join("; ")
                );
            }

            let payload_json = payload_value.to_string();
            let mut form = multipart::Form::new().text("payload_json", payload_json.clone());
            let mut attachment_included = false;

            if let Some((bytes, file_name)) = prepared_attachment.as_ref() {
                match multipart::Part::bytes(bytes.clone())
                    .file_name(file_name.clone())
                    .mime_str("application/octet-stream")
                {
                    Ok(part) => {
                        form = form.part("file", part);
                        attachment_included = true;
                    }
                    Err(err) => {
                        warn!(
                            "Failed to prepare Discord attachment part '{}': {}",
                            file_name, err
                        );
                    }
                }
            }

            match client.post(&url).multipart(form).send().await {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    let status = response.status();
                    if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE && attachment_included {
                        log_discord_webhook_error_response(
                            response,
                            discord_url,
                            "initial request with attachment",
                        )
                        .await;
                        let retry_form = multipart::Form::new().text("payload_json", payload_json);
                        match client.post(&url).multipart(retry_form).send().await {
                            Ok(retry_response) if retry_response.status().is_success() => {}
                            Ok(retry_response) => {
                                log_discord_webhook_error_response(
                                    retry_response,
                                    discord_url,
                                    "retry without attachment",
                                )
                                .await;
                            }
                            Err(err) => {
                                warn!(
                                    "Failed to retry Discord webhook '{}' without attachment: {}",
                                    discord_url, err
                                );
                            }
                        }
                    } else {
                        log_discord_webhook_error_response(
                            response,
                            discord_url,
                            "initial request",
                        )
                        .await;
                    }
                }
                Err(e) => {
                    warn!("Failed to send Discord webhook '{}': {}", discord_url, e);
                }
            }
        }
    }

    let non_discord_urls: Vec<&str> = apprise_urls_from_config_array
        .iter()
        .map(|u| u.trim())
        .filter(|u| !is_native_discord(u))
        .collect();

    if non_discord_urls.is_empty() {
        return;
    }

    let attempts = [
        ("markdown", markdown_body),
        ("html", html_body),
        ("text", text_body),
    ];

    for (format, body) in attempts.iter() {
        let mut command = Command::new(crate::components::apprise());
        command.arg("--title").arg(&apprise_title);
        command.arg("--body").arg(body);
        command.arg("--input-format").arg(format);

        if let Some(path) = attachment_path.as_ref() {
            command.arg("--attach").arg(path);
        }

        for target in &non_discord_urls {
            command.arg(target);
        }

        match command.output().await {
            Ok(output) if output.status.success() => {
                info!(
                    "Delivered notification via AppRise using '{}' format to {} target(s)",
                    format,
                    non_discord_urls.len()
                );
                return;
            }
            Ok(output) => {
                warn!(
                    "AppRise '{}' format attempt failed (exit {:?}): stderr={} stdout={}",
                    format,
                    output.status.code(),
                    truncate_for_log(String::from_utf8_lossy(&output.stderr).trim(), 800),
                    truncate_for_log(String::from_utf8_lossy(&output.stdout).trim(), 800)
                );
            }
            Err(err) => {
                warn!(
                    "Failed to invoke 'apprise' for '{}' format (is it installed and on PATH?): {}",
                    format, err
                );
            }
        }
    }

    warn!("Unable to deliver notification via AppRise after trying all formats");
}

const DISCORD_ATTACHMENT_COMPRESS_THRESHOLD: usize = 9 * 1024 * 1024;

async fn prepare_discord_attachment(path: &Path, original_bytes: Vec<u8>) -> (Vec<u8>, String) {
    let original_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "recording.bin".to_string());

    if original_bytes.len() <= DISCORD_ATTACHMENT_COMPRESS_THRESHOLD {
        return (original_bytes, original_name);
    }

    let compressed_temp = match tempfile::Builder::new()
        .prefix("discord_recording_")
        .suffix(".mp3")
        .tempfile()
    {
        Ok(file) => file,
        Err(err) => {
            warn!(
                "Failed to allocate temp file to compress '{}' for Discord; sending original: {}",
                path.display(),
                err
            );
            return (original_bytes, original_name);
        }
    };

    let compressed_path = compressed_temp.into_temp_path();
    let compressed_path_buf = compressed_path.to_path_buf();

    let mut ffmpeg = Command::new(crate::components::ffmpeg());
    ffmpeg
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(path)
        .arg("-vn")
        .arg("-c:a")
        .arg("libmp3lame")
        .arg("-b:a")
        .arg("128k")
        .arg(&compressed_path_buf);

    match ffmpeg.status().await {
        Ok(status) if status.success() => match tokio::fs::read(&compressed_path_buf).await {
            Ok(compressed_bytes) => {
                let mp3_name = Path::new(&original_name)
                    .with_extension("mp3")
                    .to_string_lossy()
                    .into_owned();
                info!(
                    "Recording '{}' is {} bytes (over the {} byte Discord limit); attaching {} byte 128 kbps MP3 '{}' instead",
                    path.display(),
                    original_bytes.len(),
                    DISCORD_ATTACHMENT_COMPRESS_THRESHOLD,
                    compressed_bytes.len(),
                    mp3_name
                );
                (compressed_bytes, mp3_name)
            }
            Err(err) => {
                warn!(
                    "Failed to read compressed Discord attachment for '{}'; sending original: {}",
                    path.display(),
                    err
                );
                (original_bytes, original_name)
            }
        },
        Ok(status) => {
            warn!(
                "ffmpeg failed to compress '{}' for Discord (status {:?}); sending original",
                path.display(),
                status.code()
            );
            (original_bytes, original_name)
        }
        Err(err) => {
            warn!(
                "Failed to invoke ffmpeg to compress '{}' for Discord; sending original: {}",
                path.display(),
                err
            );
            (original_bytes, original_name)
        }
    }
}

async fn log_discord_webhook_error_response(
    response: reqwest::Response,
    discord_url: &str,
    attempt_label: &str,
) {
    let status = response.status();
    let body = match response.text().await {
        Ok(text) => text,
        Err(err) => {
            warn!(
                "Discord webhook {} responded with status {} for '{}' and body could not be read: {}",
                attempt_label, status, discord_url, err
            );
            return;
        }
    };

    let trimmed_body = body.trim();
    if trimmed_body.is_empty() {
        warn!(
            "Discord webhook {} responded with status {} for '{}' (empty response body)",
            attempt_label, status, discord_url
        );
        return;
    }

    if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(trimmed_body) {
        let message = json_body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("<missing message>");
        let code = json_body
            .get("code")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "<missing code>".to_string());
        let errors = json_body.get("errors");

        if let Some(errors) = errors {
            warn!(
                "Discord webhook {} responded with status {} for '{}': message='{}' code={} errors={}",
                attempt_label,
                status,
                discord_url,
                message,
                code,
                truncate_for_log(&errors.to_string(), 1600)
            );
        } else {
            warn!(
                "Discord webhook {} responded with status {} for '{}': message='{}' code={} body={}",
                attempt_label,
                status,
                discord_url,
                message,
                code,
                truncate_for_log(trimmed_body, 1600)
            );
        }
    } else {
        warn!(
            "Discord webhook {} responded with status {} for '{}': non-JSON body={}",
            attempt_label,
            status,
            discord_url,
            truncate_for_log(trimmed_body, 1600)
        );
    }
}

fn truncate_for_log(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...(truncated)", &input[..end])
}

const DISCORD_FIELD_VALUE_LIMIT: usize = 1024;
const DISCORD_EMBED_TOTAL_LIMIT: usize = 6000;

fn discord_fields_char_count(fields: &[serde_json::Value]) -> usize {
    fields
        .iter()
        .map(|field| {
            let name_len = field
                .get("name")
                .and_then(|value| value.as_str())
                .map_or(0, |value| value.chars().count());
            let value_len = field
                .get("value")
                .and_then(|value| value.as_str())
                .map_or(0, |value| value.chars().count());
            name_len + value_len
        })
        .sum()
}

#[allow(
    clippy::too_many_arguments,
    reason = "one argument per field Discord's embed schema renders"
)]
fn build_discord_embed_body(
    stream_id: &str,
    title: &str,
    event_code: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    protocol_header: Option<&str>,
    description: Option<&str>,
    instructions: Option<&str>,
) -> serde_json::Value {
    let runtime_config = runtime_config_snapshot();
    let monitor_number = runtime_config
        .stream_index_map
        .get(stream_id)
        .copied()
        .unwrap_or(999);
    let normalized_event_code = event_code
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect::<String>();
    let filter_name = filter::determine_filter_name(&normalized_event_code);

    let img_name = if !normalized_event_code.is_empty() {
        normalized_event_code.as_str()
    } else {
        "ZZZ"
    };

    let img_color = if title.to_lowercase().contains("test") {
        "105733"
    } else if title.to_lowercase().contains("advisory") || title.to_lowercase().contains("watch") {
        "FFFF00"
    } else if title.to_lowercase().contains("warning") || title.to_lowercase().contains("emergency")
    {
        "FF0000"
    } else {
        "808080"
    };

    let img_color_dec = u32::from_str_radix(img_color, 16).unwrap_or(0x808080);
    let event_title = truncate_discord_text(
        format!(
            "{} {} has just been issued/received.",
            a_or_an(title),
            title
        )
        .as_str(),
        256,
    );
    let author_name = truncate_discord_text(
        format!("{} - Software ENDEC Logs", runtime_config.station_name).as_str(),
        256,
    );

    let mut fields = vec![
        json!({
            "name": "Received From:",
            "value": truncate_discord_text(originator, 1024),
            "inline": false
        }),
        json!({
            "name": "Received At:",
            "value": truncate_discord_text(received_timestamp, 1024),
            "inline": false
        }),
        json!({
            "name": "Monitor",
            "value": truncate_discord_text(format!("#{}", monitor_number).as_str(), 1024),
            "inline": true
        }),
        json!({
            "name": "Filter",
            "value": truncate_discord_text(filter_name.as_str(), 1024),
            "inline": true
        }),
        json!({
            "name": "EAS Text Data:",
            "value": discord_codeblock(eas_text.trim_end(), 1024),
            "inline": false
        }),
    ];

    if let Some(header) = protocol_header {
        fields.push(json!({
            "name": "EAS Protocol Data:",
            "value": discord_codeblock(header.trim_end(), 1024),
            "inline": false
        }));
    }

    if let Some(value) = description {
        fields.push(json!({
            "name": "CAP Description:",
            "value": discord_codeblock(value, DISCORD_FIELD_VALUE_LIMIT),
            "inline": false
        }));
    }

    if let Some(value) = instructions {
        let field_name = "CAP Instructions:";
        let field_value = format!("```\n{}\n```", value);
        let field_len = field_name.chars().count() + field_value.chars().count();
        let projected_total = event_title.chars().count()
            + author_name.chars().count()
            + discord_fields_char_count(&fields)
            + field_len;

        if field_value.chars().count() <= DISCORD_FIELD_VALUE_LIMIT
            && projected_total <= DISCORD_EMBED_TOTAL_LIMIT
        {
            fields.push(json!({
                "name": field_name,
                "value": field_value,
                "inline": false
            }));
        } else {
            info!(
                "Omitting CAP instructions from Discord embed for '{}': {} chars would exceed the embed limits",
                event_code,
                value.chars().count()
            );
        }
    }

    let embed = json!({
        "title": event_title,
        "color": img_color_dec,
        "author": {
            "name": author_name,
            "icon_url": format!("https://wagspuzzle.space/assets/eas-icons/index.php?code={}&hex=0x{}", img_name, img_color),
            "url": github_url.as_str()
        },
        "fields": fields
    });

    embed
}

fn build_markdown_body(
    title: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    protocol_header: Option<&str>,
    description: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let runtime_config = runtime_config_snapshot();
    let protocol_section = match protocol_header {
        Some(value) => format!("\n\n**EAS Protocol Data:**\n```\n{}\n```", value.trim_end()),
        None => String::new(),
    };
    let description_section = match description {
        Some(value) => format!("\n\n**CAP Description:**\n```\n{}\n```", value),
        None => String::new(),
    };
    let instructions_section = match instructions {
        Some(value) => format!("\n\n**CAP Instructions:**\n```\n{}\n```", value),
        None => String::new(),
    };

    format!(
        "**{} - Software ENDEC Logs**\n\n**{} {}** has just been received from: {}\n\n**Received:** {}\n\n**EAS Text Data:**\n```\n{}\n```{}{}{}\n\nPowered by [Wags' Software ENDEC]({})",
        runtime_config.station_name,
        a_or_an(title),
        title,
        originator,
        received_timestamp,
        eas_text.trim_end(),
        protocol_section,
        description_section,
        instructions_section,
        github_url.as_str()
    )
}

fn validate_discord_payload(payload: &serde_json::Value) -> Vec<String> {
    let mut issues = Vec::new();

    let Some(embeds) = payload.get("embeds").and_then(|v| v.as_array()) else {
        issues.push("payload.embeds is missing or not an array".to_string());
        return issues;
    };

    if embeds.is_empty() {
        issues.push("payload.embeds is empty".to_string());
        return issues;
    }

    for (idx, embed) in embeds.iter().enumerate() {
        let Some(embed_obj) = embed.as_object() else {
            issues.push(format!("payload.embeds[{idx}] is not an object"));
            continue;
        };

        let mut total_chars = 0usize;
        if let Some(title) = embed_obj.get("title").and_then(|v| v.as_str()) {
            let len = title.chars().count();
            total_chars += len;
            if len > 256 {
                issues.push(format!(
                    "payload.embeds[{idx}].title exceeds 256 chars ({len})"
                ));
            }
        }

        if let Some(color) = embed_obj.get("color") {
            if !color.is_number() {
                issues.push(format!(
                    "payload.embeds[{idx}].color must be a number (got {})",
                    color
                ));
            }
        }

        if let Some(author_name) = embed_obj
            .get("author")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
        {
            let len = author_name.chars().count();
            total_chars += len;
            if len > 256 {
                issues.push(format!(
                    "payload.embeds[{idx}].author.name exceeds 256 chars ({len})"
                ));
            }
        }

        if let Some(fields) = embed_obj.get("fields").and_then(|v| v.as_array()) {
            if fields.len() > 25 {
                issues.push(format!(
                    "payload.embeds[{idx}].fields has more than 25 items"
                ));
            }
            for (field_idx, field) in fields.iter().enumerate() {
                let Some(field_obj) = field.as_object() else {
                    issues.push(format!(
                        "payload.embeds[{idx}].fields[{field_idx}] is not an object"
                    ));
                    continue;
                };

                if let Some(name) = field_obj.get("name").and_then(|v| v.as_str()) {
                    let len = name.chars().count();
                    total_chars += len;
                    if len > 256 {
                        issues.push(format!(
                            "payload.embeds[{idx}].fields[{field_idx}].name exceeds 256 chars ({len})"
                        ));
                    }
                }

                if let Some(value) = field_obj.get("value").and_then(|v| v.as_str()) {
                    let len = value.chars().count();
                    total_chars += len;
                    if len > 1024 {
                        issues.push(format!(
                            "payload.embeds[{idx}].fields[{field_idx}].value exceeds 1024 chars ({len})"
                        ));
                    }
                }
            }
        }

        if total_chars > 6000 {
            issues.push(format!(
                "payload.embeds[{idx}] total text exceeds 6000 chars ({total_chars})"
            ));
        }
    }

    issues
}

fn truncate_discord_text(input: &str, max_chars: usize) -> String {
    let current_len = input.chars().count();
    if current_len <= max_chars {
        return input.to_string();
    }

    let suffix = "...(truncated)";
    let suffix_len = suffix.chars().count();
    let keep = max_chars.saturating_sub(suffix_len);
    let prefix: String = input.chars().take(keep).collect();
    format!("{prefix}{suffix}")
}

fn discord_codeblock(content: &str, max_total_chars: usize) -> String {
    let wrapper = "```\n\n```";
    let wrapper_len = wrapper.chars().count();
    let inner_limit = max_total_chars.saturating_sub(wrapper_len);
    let clipped = truncate_discord_text(content, inner_limit);
    format!("```\n{}\n```", clipped)
}

fn build_html_body(
    title: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    protocol_header: Option<&str>,
    description: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let runtime_config = runtime_config_snapshot();
    let protocol_section = match protocol_header {
        Some(value) => format!(
            "<p><strong>EAS Protocol Data:</strong></p><pre>{}</pre>",
            html_escape(value.trim_end())
        ),
        None => String::new(),
    };
    let description_section = match description {
        Some(value) => format!(
            "<p><strong>CAP Description:</strong></p><pre>{}</pre>",
            html_escape(value)
        ),
        None => String::new(),
    };
    let instructions_section = match instructions {
        Some(value) => format!(
            "<p><strong>CAP Instructions:</strong></p><pre>{}</pre>",
            html_escape(value)
        ),
        None => String::new(),
    };

    format!(
        "<p><strong>{} - Software ENDEC Logs</strong></p>\
         <p><strong>{} {}</strong> has just been received from: {}</p>\
         <p><strong>Received:</strong> {}</p>\
         <p><strong>EAS Text Data:</strong></p>\
         <pre>{}</pre>\
         {}{}{}\
         <p>Powered by <a href=\"{}\">Wags' Software ENDEC</a></p>",
        html_escape(&runtime_config.station_name),
        html_escape(a_or_an(title)),
        html_escape(title),
        html_escape(originator),
        html_escape(received_timestamp),
        html_escape(eas_text.trim_end()),
        protocol_section,
        description_section,
        instructions_section,
        github_url.as_str()
    )
}

fn build_plain_body(
    title: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    protocol_header: Option<&str>,
    description: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let runtime_config = runtime_config_snapshot();
    let protocol_section = match protocol_header {
        Some(value) => format!("\n\nEAS Protocol Data:\n{}", value.trim_end()),
        None => String::new(),
    };
    let description_section = match description {
        Some(value) => format!("\n\nCAP Description:\n{}", value),
        None => String::new(),
    };
    let instructions_section = match instructions {
        Some(value) => format!("\n\nCAP Instructions:\n{}", value),
        None => String::new(),
    };

    format!(
        "{} - Software ENDEC Logs\n\n{} {} has just been received from: {}\nReceived: {}\n\nEAS Text Data:\n{}{}{}{}\n\nPowered by Wags' Software ENDEC ({})",
        runtime_config.station_name,
        a_or_an(title),
        title,
        originator,
        received_timestamp,
        eas_text.trim_end(),
        protocol_section,
        description_section,
        instructions_section,
        github_url.as_str()
    )
}

fn html_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_and_originator_lookup_are_humanized() {
        assert_eq!(determine_event_title("TOR"), "Tornado Warning");
        assert_eq!(
            determine_originator_name("WXR"),
            "The National Weather Service"
        );
        assert_eq!(determine_event_title("ZZZ"), "ZZZ");
    }

    #[test]
    fn article_and_escape_helpers_work() {
        assert_eq!(a_or_an("Emergency"), "An");
        assert_eq!(a_or_an("Warning"), "A");
        assert_eq!(html_escape("<a&\"'>"), "&lt;a&amp;&quot;&#39;&gt;");
    }

    #[test]
    fn discord_urls_map_to_their_webhook_with_or_without_a_bot_name() {
        let endpoint = "https://discord.com/api/webhooks/123/abc-DEF".to_string();
        assert_eq!(
            discord_endpoint("discord://123/abc-DEF"),
            Some((endpoint.clone(), None))
        );
        assert_eq!(
            discord_endpoint("discord://EAS%20Bot@123/abc-DEF/?avatar=no"),
            Some((endpoint, Some("EAS Bot".to_string())))
        );
        assert_eq!(discord_endpoint("discord://123"), None);
        assert_eq!(discord_endpoint("discord://1/2/3"), None);
        assert!(is_native_discord(" discord://1/2"));
        assert!(!is_native_discord("https://discord.com/api/webhooks/1/2"));
    }

    #[test]
    fn masked_urls_keep_only_the_scheme_and_the_last_characters() {
        assert_eq!(mask_url("tgram://123456:secret/9876"), "tgram://****9876");
        assert_eq!(mask_url("json://host"), "json://****");
        assert_eq!(mask_url("no scheme"), "****");
    }

    #[test]
    fn truncate_for_log_preserves_char_boundaries() {
        let input = "éééé";
        let clipped = truncate_for_log(input, 3);
        assert!(clipped.starts_with("é"));
        assert!(clipped.ends_with("...(truncated)"));
    }

    #[test]
    fn validate_discord_payload_detects_and_accepts_payloads() {
        let invalid = json!({ "embeds": [] });
        let issues = validate_discord_payload(&invalid);
        assert!(!issues.is_empty());

        let embed = build_discord_embed_body(
            "unknown-stream",
            "Tornado Warning",
            "TOR",
            "The National Weather Service",
            "2026-03-06 10:00:00 PM",
            "Sample EAS text",
            Some("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-"),
            Some("CAP Description"),
            Some("CAP Instructions"),
        );
        let valid = json!({ "embeds": [embed] });
        let issues = validate_discord_payload(&valid);
        assert!(issues.is_empty(), "expected no issues, got: {:?}", issues);
    }

    fn embed_field_names(embed: &serde_json::Value) -> Vec<String> {
        embed
            .get("fields")
            .and_then(|fields| fields.as_array())
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|field| field.get("name").and_then(|name| name.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn discord_embed_includes_instructions_only_when_they_fit() {
        let fitting = build_discord_embed_body(
            "unknown-stream",
            "Tornado Warning",
            "TOR",
            "The National Weather Service",
            "2026-03-06 10:00:00 PM",
            "Sample EAS text",
            Some("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-"),
            Some("CAP Description"),
            Some("Take shelter now."),
        );
        assert!(embed_field_names(&fitting)
            .iter()
            .any(|name| name == "CAP Instructions:"));

        let oversized_instructions = "A".repeat(2000);
        let oversized = build_discord_embed_body(
            "unknown-stream",
            "Tornado Warning",
            "TOR",
            "The National Weather Service",
            "2026-03-06 10:00:00 PM",
            "Sample EAS text",
            Some("ZCZC-WXR-TOR-031055+0030-1231645-KWO35-"),
            Some("CAP Description"),
            Some(oversized_instructions.as_str()),
        );
        assert!(!embed_field_names(&oversized)
            .iter()
            .any(|name| name == "CAP Instructions:"));
        assert!(validate_discord_payload(&json!({ "embeds": [oversized] })).is_empty());
    }

    #[test]
    fn markdown_and_plain_body_include_cap_description_when_present() {
        let markdown = build_markdown_body(
            "Tornado Warning",
            "The National Weather Service",
            "2026-03-06 10:00:00 PM",
            "Text",
            Some("Header"),
            Some("CAP details"),
            Some("CAP steps"),
        );
        assert!(markdown.contains("CAP Description"));
        assert!(markdown.contains("CAP Instructions"));

        let plain = build_plain_body(
            "Tornado Warning",
            "The National Weather Service",
            "2026-03-06 10:00:00 PM",
            "Text",
            Some("Header"),
            Some("CAP details"),
            None,
        );
        assert!(plain.contains("CAP Description"));
        assert!(!plain.contains("CAP Instructions"));
    }

    /// An alert that went out as Alert Ready rather than SAME publishes no SAME header, in any of
    /// the four shapes a webhook can take; one that went out as SAME keeps it in all four.
    #[test]
    fn every_body_carries_the_protocol_header_only_when_given_one() {
        let header = "ZCZC-CIV-TOR-043100+0030-2611800-NAADSCAP-";

        for protocol_header in [Some(header), None] {
            let expected = protocol_header.is_some();

            let markdown = build_markdown_body(
                "Tornado Warning",
                "Environment Canada",
                "2026-09-18 01:00:00 PM",
                "Text",
                protocol_header,
                Some("CAP details"),
                None,
            );
            let html = build_html_body(
                "Tornado Warning",
                "Environment Canada",
                "2026-09-18 01:00:00 PM",
                "Text",
                protocol_header,
                Some("CAP details"),
                None,
            );
            let plain = build_plain_body(
                "Tornado Warning",
                "Environment Canada",
                "2026-09-18 01:00:00 PM",
                "Text",
                protocol_header,
                Some("CAP details"),
                None,
            );
            let embed = build_discord_embed_body(
                "unknown-stream",
                "Tornado Warning",
                "TOR",
                "Environment Canada",
                "2026-09-18 01:00:00 PM",
                "Text",
                protocol_header,
                Some("CAP details"),
                None,
            );

            for (shape, body) in [
                ("markdown", markdown),
                ("html", html),
                ("plain", plain),
                ("discord", embed.to_string()),
            ] {
                assert_eq!(
                    body.contains("EAS Protocol Data"),
                    expected,
                    "{shape}: {body}"
                );
                assert_eq!(body.contains(header), expected, "{shape}: {body}");
                // Everything else survives either way.
                assert!(body.contains("CAP details"), "{shape}: {body}");
            }
            assert!(validate_discord_payload(&json!({ "embeds": [embed] })).is_empty());
        }
    }
}
