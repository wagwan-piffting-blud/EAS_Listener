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
use std::path::PathBuf;
use tokio::process::Command;
use tracing::warn;

#[derive(Debug, Deserialize)]
struct SameUsLookup {
    #[serde(rename = "ORGS")]
    orgs: HashMap<String, String>,
    #[serde(rename = "EVENTS")]
    events: HashMap<String, String>,
}

lazy_static! {
    static ref json_config: Config = Config::from_config_json("/app/config.json").unwrap_or_else(
        |err| {
            eprintln!(
                "Warning: failed to load /app/config.json for webhook config: {:?}. Using built-in safe defaults.",
                err
            );
            Config::safe_internal_defaults()
        },
    );
    static ref station_name: String = json_config.eas_relay_name.clone();
    static ref STREAM_INDEX_MAP: HashMap<String, usize> = json_config
        .icecast_stream_urls
        .iter()
        .enumerate()
        .map(|(idx, url)| (url.clone(), idx + 1))
        .collect();
    static ref github_url: String =
        "https://github.com/wagwan-piffting-blud/EAS_Listener".to_string();
    static ref same_us_lookup: SameUsLookup =
        serde_json::from_str(include_str!("../include/same-us.json")).expect("parse same-us.json");
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

pub async fn send_alert_webhook(
    url: &str,
    alert: &ActiveAlert,
    _dsame_text: &str,
    _raw_header: &str,
    recording_path: Option<PathBuf>,
) {
    let config_path = json_config.apprise_config_path.to_string();
    let apprise_urls_from_config_array: Vec<String> = match fs::File::open(&config_path) {
        Ok(mut file) => {
            let mut contents = String::new();
            if let Err(err) = file.read_to_string(&mut contents) {
                warn!(
                    "Failed to read AppRise config file at '{}': {}",
                    config_path, err
                );
                return;
            }
            contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| {
                    line.strip_prefix('-')
                        .map(str::trim_start)
                        .unwrap_or(line)
                        .to_owned()
                })
                .collect()
        }
        Err(err) => {
            warn!(
                "Failed to open AppRise config file at '{}': {}",
                config_path, err
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
    // Fetch event title from `include/same-us.json` based on the event code in the alert data.
    let event_code = &data.event_code;
    let event_title = determine_event_title(&event_code);
    let originator_code = &data.originator;
    let originator = determine_originator_name(&originator_code);
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
        &url,
        &event_title,
        event_code,
        &originator,
        &received_timestamp,
        &data.eas_text,
        &alert.raw_header,
        description,
    );
    let markdown_body = build_markdown_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        &alert.raw_header,
        description,
    );
    let html_body = build_html_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        &alert.raw_header,
        description,
    );
    let text_body = build_plain_body(
        &event_title,
        &originator,
        &received_timestamp,
        &data.eas_text,
        &alert.raw_header,
        description,
    );

    let discord_urls: Vec<&str> = apprise_urls_from_config_array
        .iter()
        .map(|url| url.trim())
        .filter(|url| url.starts_with("discord://"))
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

        for discord_url in discord_urls {
            let payload_value = json!({ "embeds": [discord_embed_body.clone()] });
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

            if let (Some(path), Some(bytes)) = (attachment_path.as_ref(), attachment_bytes.as_ref())
            {
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "recording.bin".to_string());

                match multipart::Part::bytes(bytes.clone())
                    .file_name(file_name)
                    .mime_str("application/octet-stream")
                {
                    Ok(part) => {
                        form = form.part("file", part);
                        attachment_included = true;
                    }
                    Err(err) => {
                        warn!(
                            "Failed to prepare Discord attachment part for '{}': {}",
                            path.display(),
                            err
                        );
                    }
                }
            }

            let url = format!(
                "https://discord.com/api/webhooks/{}",
                discord_url.trim_start_matches("discord://")
            );

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

        return;
    }

    let attempts = [
        ("markdown", markdown_body),
        ("html", html_body),
        ("text", text_body),
    ];

    for (format, body) in attempts.iter() {
        let mut command = Command::new("apprise");
        command.arg("--config").arg(&config_path);
        command.arg("--title").arg(&apprise_title);
        command.arg("--body").arg(body);
        command.arg("--input-format").arg(format);

        if let Some(path) = attachment_path.as_ref() {
            command.arg("--attach").arg(path);
        }

        match command.output().await {
            Ok(output) if output.status.success() => {
                return;
            }
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    "AppRise CLI failed with format {} (exit code {:?}): stdout='{}' stderr='{}'",
                    format,
                    output.status.code(),
                    stdout.trim(),
                    stderr.trim()
                );
            }
            Err(err) => {
                warn!(
                    "Failed to invoke AppRise CLI with format {}: {}",
                    format, err
                );
            }
        }
    }

    warn!("Unable to deliver notification via AppRise after trying all formats");
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

fn build_discord_embed_body(
    stream_id: &str,
    title: &str,
    event_code: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    raw_header: &str,
    description: Option<&str>,
) -> serde_json::Value {
    let monitor_number = STREAM_INDEX_MAP.get(stream_id).copied().unwrap_or(999);
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
        format!("{} - Software ENDEC Logs", station_name.as_str()).as_str(),
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
        json!({
            "name": "EAS Protocol Data:",
            "value": discord_codeblock(raw_header.trim_end(), 1024),
            "inline": false
        }),
    ];

    if let Some(value) = description {
        fields.push(json!({
            "name": "CAP Description:",
            "value": discord_codeblock(value, 1024),
            "inline": false
        }));
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

    return embed;
}

fn build_markdown_body(
    title: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    raw_header: &str,
    description: Option<&str>,
) -> String {
    let description_section = match description {
        Some(value) => format!("\n\n**CAP Description:**\n```\n{}\n```", value),
        None => String::new(),
    };

    format!(
        "**{} - Software ENDEC Logs**\n\n**{} {}** has just been received from: {}\n\n**Received:** {}\n\n**EAS Text Data:**\n```\n{}\n```\n\n**EAS Protocol Data:**\n```\n{}\n```{}\n\nPowered by [Wags' Software ENDEC]({})",
        station_name.as_str(),
        a_or_an(title),
        title,
        originator,
        received_timestamp,
        eas_text.trim_end(),
        raw_header.trim_end(),
        description_section,
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
    raw_header: &str,
    description: Option<&str>,
) -> String {
    let description_section = match description {
        Some(value) => format!(
            "<p><strong>CAP Description:</strong></p><pre>{}</pre>",
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
         <p><strong>EAS Protocol Data:</strong></p>\
         <pre>{}</pre>\
         {}\
         <p>Powered by <a href=\"{}\">Wags' Software ENDEC</a></p>",
        html_escape(&station_name.as_str()),
        html_escape(a_or_an(title)),
        html_escape(title),
        html_escape(originator),
        html_escape(received_timestamp),
        html_escape(eas_text.trim_end()),
        html_escape(raw_header.trim_end()),
        description_section,
        github_url.as_str()
    )
}

fn build_plain_body(
    title: &str,
    originator: &str,
    received_timestamp: &str,
    eas_text: &str,
    raw_header: &str,
    description: Option<&str>,
) -> String {
    let description_section = match description {
        Some(value) => format!("\n\nCAP Description:\n{}", value),
        None => String::new(),
    };

    format!(
        "{} - Software ENDEC Logs\n\n{} {} has just been received from: {}\nReceived: {}\n\nEAS Text Data:\n{}\n\nEAS Protocol Data:\n{}{}\n\nPowered by Wags' Software ENDEC ({})",
        station_name.as_str(),
        a_or_an(title),
        title,
        originator,
        received_timestamp,
        eas_text.trim_end(),
        raw_header.trim_end(),
        description_section,
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
