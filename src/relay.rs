use crate::config::Config;
use crate::filter::{self, FilterAction, FilterRule};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use reqwest::Client;
use std::path::{Path, PathBuf};
use tempfile::Builder;
use tokio::process::Command;
use tracing::{info, warn};

const TARGET_SAMPLE_RATE: u32 = 48_000;

fn channel_layout_name(channels: u16) -> &'static str {
    match channels {
        1 => "mono",
        _ => "stereo",
    }
}

struct MatchedFormat {
    encoder: &'static str,
    container: &'static str,
    content_type: &'static str,
    sample_rate: u32,
    channels: u16,
    bitrate: Option<u32>,
}

fn icecast_source_to_listener_url(source: &str) -> Option<String> {
    let source = source.trim();
    let (scheme, rest) = match source.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => (String::from("http"), source),
    };
    let listener_scheme = if scheme.contains("ssl") || scheme == "https" {
        "https"
    } else {
        "http"
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.is_empty() {
        return None;
    }
    Some(format!("{listener_scheme}://{host_port}{path}"))
}

async fn probe_icecast_format(source_url: &str) -> Option<MatchedFormat> {
    let listener_url = icecast_source_to_listener_url(source_url)?;

    let probe = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-hide_banner")
        .arg("-rw_timeout")
        .arg("8000000") 
        .arg("-select_streams")
        .arg("a:0")
        .arg("-show_entries")
        .arg("stream=codec_name,sample_rate,channels,bit_rate:format=bit_rate")
        .arg("-of")
        .arg("json")
        .arg(&listener_url)
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(std::time::Duration::from_secs(10), probe)
        .await
        .ok()? 
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let stream = json.get("streams")?.as_array()?.first()?;

    let codec = stream.get("codec_name")?.as_str()?;
    let sample_rate = stream
        .get("sample_rate")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u32>().ok())?;
    let channels = stream
        .get("channels")
        .and_then(|v| v.as_u64())
        .map(|c| c as u16)?;
    let bitrate = stream
        .get("bit_rate")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| {
            json.get("format")
                .and_then(|f| f.get("bit_rate"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u32>().ok())
        });

    let (encoder, container, content_type) = match codec {
        "mp3" => ("libmp3lame", "mp3", "audio/mpeg"),
        "vorbis" => ("libvorbis", "ogg", "audio/ogg"),
        "opus" => ("libopus", "ogg", "audio/ogg"),
        "aac" => ("aac", "adts", "audio/aac"),
        "flac" => ("flac", "ogg", "audio/ogg"),
        _ => return None,
    };

    Some(MatchedFormat {
        encoder,
        container,
        content_type,
        sample_rate,
        channels,
        bitrate: if encoder == "flac" { None } else { bitrate },
    })
}

pub struct RelayState {
    pub config: Config,
}

impl RelayState {
    pub async fn new(config: Config) -> Result<Self> {
        if config.should_relay && config.should_relay_icecast && config.icecast_relay.is_empty() {
            return Err(anyhow!(
                "ICECAST_RELAY must be set if SHOULD_RELAY and SHOULD_RELAY_ICECAST are true"
            ));
        }

        Ok(Self { config })
    }

    pub async fn start_relay<P>(
        &self,
        event_code: &str,
        filters: &[FilterRule],
        recorded_segment: P,
        _source_stream: Option<&str>,
        raw_header: &str,
    ) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let (action, filter_name) = filter::match_filter(filters, event_code)
            .map(|rule| (rule.action, rule.name.as_str()))
            .unwrap_or((FilterAction::Relay, "Default Filter"));

        match action {
            FilterAction::Ignore => {
                info!(
                    event_code,
                    filter = filter_name,
                    "Filter action 'ignore'; skipping relay."
                );
                return Ok(());
            }
            FilterAction::Log => {
                info!(
                    event_code,
                    filter = filter_name,
                    "Filter action 'log'; recording retained, skipping relay."
                );
                return Ok(());
            }
            FilterAction::Relay => {
                info!(
                    event_code,
                    filter = filter_name,
                    "Filter action 'relay'; proceeding with relay."
                );
            }
            FilterAction::Forward => {
                info!(
                    event_code,
                    filter = filter_name,
                    "Filter action 'forward'; forwarded to Apprise but NOT relaying over Icecast/MYOD."
                );
                return Ok(());
            }
        }

        let config = &self.config;
        let recorded_segment = recorded_segment.as_ref();

        if recorded_segment.as_os_str().is_empty() {
            return Err(anyhow!(
                "Recording segment path is empty. Cannot start relay."
            ));
        }

        let include_icecast_intro_outro =
            config.should_relay && config.should_relay_icecast && config.use_icecast_intro_outro;
        let mut audio_segments =
            Vec::with_capacity(if include_icecast_intro_outro { 3 } else { 1 });

        if include_icecast_intro_outro && !config.icecast_intro.as_os_str().is_empty() {
            audio_segments.push(config.icecast_intro.clone());
        }

        audio_segments.push(recorded_segment.to_path_buf());

        if include_icecast_intro_outro && !config.icecast_outro.as_os_str().is_empty() {
            audio_segments.push(config.icecast_outro.clone());
        }

        #[derive(Clone)]
        enum Segment {
            File(PathBuf),
            Silence,
        }

        let mut ordered_segments = Vec::new();
        for (idx, segment) in audio_segments.into_iter().enumerate() {
            if idx > 0 {
                ordered_segments.push(Segment::Silence);
            }
            ordered_segments.push(Segment::File(segment));
        }

        if ordered_segments.is_empty() {
            return Err(anyhow!("No segments available to relay"));
        }

        let matched_format = if config.should_relay
            && config.should_relay_icecast
            && !config.icecast_relay.trim().is_empty()
        {
            probe_icecast_format(&config.icecast_relay).await
        } else {
            None
        };

        let (norm_sample_rate, norm_channels) = match &matched_format {
            Some(fmt) => (fmt.sample_rate, fmt.channels),
            None => (TARGET_SAMPLE_RATE, 1),
        };
        let norm_layout = channel_layout_name(norm_channels);

        let combined_temp = Builder::new()
            .prefix("relay_combined_")
            .suffix(".ogg")
            .tempfile()
            .context("Failed to allocate temporary relay file")?;
        let combined_path = combined_temp.into_temp_path();
        let combined_path_buf = combined_path.to_path_buf();

        let mut prepare = Command::new("ffmpeg");
        prepare.arg("-nostdin");
        prepare.arg("-hide_banner");
        prepare.arg("-loglevel").arg("info");
        prepare.arg("-y");

        let mut input_count = 0u32;
        for segment in &ordered_segments {
            match segment {
                Segment::File(path) => {
                    prepare.arg("-i").arg(path);
                }
                Segment::Silence => {
                    prepare
                        .arg("-f")
                        .arg("lavfi")
                        .arg("-t")
                        .arg("1")
                        .arg("-i")
                        .arg(format!(
                            "anullsrc=channel_layout={}:sample_rate={}",
                            norm_layout, norm_sample_rate
                        ));
                }
            }
            input_count += 1;
        }

        if input_count == 0 {
            return Err(anyhow!("Failed to prepare inputs for relay"));
        }

        let mut filter_parts = Vec::new();
        let mut remapped_labels = Vec::new();
        for idx in 0..input_count {
            filter_parts.push(format!(
                "[{}:a]aresample=sample_rate={},aformat=sample_rates={}:channel_layouts={},asetpts=N/SR/TB[s{}]",
                idx,
                norm_sample_rate,
                norm_sample_rate,
                norm_layout,
                idx
            ));
            remapped_labels.push(format!("[s{}]", idx));
        }

        let mut output_label = String::from("[s0]");
        if input_count > 1 {
            filter_parts.push(format!(
                "{}concat=n={}:v=0:a=1[outa]",
                remapped_labels.join(""),
                remapped_labels.len()
            ));
            output_label = String::from("[outa]");
        }

        prepare.arg("-filter_complex").arg(filter_parts.join(";"));
        prepare.arg("-map").arg(output_label);
        prepare.arg("-ar").arg(norm_sample_rate.to_string());
        prepare.arg("-ac").arg(norm_channels.to_string());
        prepare.arg("-c:a").arg("libvorbis");
        prepare.arg("-b:a").arg("128k");
        prepare.arg(&combined_path_buf);

        let prepare_status = prepare
            .status()
            .await
            .context("Failed to execute ffmpeg bundle command")?;

        if !prepare_status.success() {
            return Err(anyhow!(
                "ffmpeg bundle process exited with status {:?}",
                prepare_status.code()
            ));
        }

        let should_relay_dasdec = config.should_relay && config.should_relay_dasdec;
        let dasdec_url = config.dasdec_url.clone();

        let dasdec_audio_b64 = if should_relay_dasdec && !dasdec_url.trim().is_empty() {
            let audio_bytes = tokio::fs::read(&combined_path_buf)
                .await
                .context("Failed to read combined relay bundle for DASDEC relay")?;
            Some(base64::engine::general_purpose::STANDARD.encode(audio_bytes))
        } else {
            None
        };

        if config.should_relay && config.should_relay_icecast {
            info!("Starting relay to Icecast servers...");

            if config.icecast_relay.is_empty() {
                return Err(anyhow!("ICECAST_RELAY is not set. Cannot start relay."));
            }

            match &matched_format {
                Some(fmt) => {
                    info!(
                        "Icecast mount serving {}/{} ({}), {} Hz, {} ch{}; matching relay format.",
                        fmt.encoder,
                        fmt.container,
                        fmt.content_type,
                        fmt.sample_rate,
                        fmt.channels,
                        fmt.bitrate
                            .map(|b| format!(", {} bps", b))
                            .unwrap_or_default()
                    );

                    let mut stream_cmd = Command::new("ffmpeg");
                    stream_cmd.arg("-nostdin");
                    stream_cmd.arg("-hide_banner");
                    stream_cmd.arg("-loglevel").arg("info");
                    stream_cmd.arg("-re");
                    stream_cmd.arg("-i").arg(&combined_path_buf);
                    stream_cmd.arg("-c:a").arg(fmt.encoder);
                    stream_cmd.arg("-ar").arg(fmt.sample_rate.to_string());
                    stream_cmd.arg("-ac").arg(fmt.channels.to_string());
                    if let Some(bitrate) = fmt.bitrate {
                        stream_cmd.arg("-b:a").arg(bitrate.to_string());
                    }
                    stream_cmd.arg("-f").arg(fmt.container);
                    stream_cmd.arg("-content_type").arg(fmt.content_type);
                    stream_cmd
                        .arg("-metadata")
                        .arg(format!("title={}", "Emergency Alert"));
                    stream_cmd
                        .arg("-metadata")
                        .arg(format!("artist={}", "EAS Listener"));
                    stream_cmd.arg(&config.icecast_relay);

                    let mut stream_child = stream_cmd
                        .spawn()
                        .context("Failed to execute ffmpeg relay stream command")?;
                    let relay_target = config.icecast_relay.clone();

                    tokio::spawn(async move {
                        match stream_child.wait().await {
                            Ok(status) if status.success() => {
                                info!("Icecast relay finished successfully.");
                            }
                            Ok(status) => {
                                warn!(
                                    "ffmpeg relay stream process to '{}' exited with status {:?}",
                                    relay_target,
                                    status.code()
                                );
                            }
                            Err(err) => {
                                warn!(
                                    "Failed while waiting for ffmpeg relay stream to '{}': {}",
                                    relay_target, err
                                );
                            }
                        }

                        if let Err(err) = combined_path.close() {
                            warn!("Failed to clean up temporary relay bundle: {}", err);
                        }
                    });

                    info!("Icecast relay running in background; continuing with DASDEC relay.");
                }
                None => {
                    warn!(
                        "Could not determine the current output format of Icecast mount '{}'; \
                         aborting Icecast relay to avoid a format mismatch. (DASDEC relay, if \
                         enabled, still proceeds.)",
                        config.icecast_relay
                    );
                }
            }
        }

        if should_relay_dasdec && !dasdec_url.trim().is_empty() {
            let client = Client::new();

            let base_url = dasdec_url.trim().trim_end_matches('/').to_string();
            let send_url = if base_url.ends_with("/send") {
                base_url.clone()
            } else if base_url.ends_with("/send_chunk") {
                format!("{}/send", base_url.trim_end_matches("/send_chunk"))
            } else {
                format!("{}/send", base_url)
            };

            let send_chunk_url = if base_url.ends_with("/send_chunk") {
                base_url.clone()
            } else if base_url.ends_with("/send") {
                format!("{}/send_chunk", base_url.trim_end_matches("/send"))
            } else {
                format!("{}/send_chunk", base_url)
            };

            let audio_b64 = dasdec_audio_b64
                .as_ref()
                .ok_or_else(|| anyhow!("DASDEC relay audio buffer was not prepared"))?;

            const DIRECT_B64_THRESHOLD: usize = 2_750_000;
            let mime_type = "audio/wav";

            let should_send_chunked = audio_b64.len() > DIRECT_B64_THRESHOLD;

            if !should_send_chunked {
                let raw_audio_data_uri = format!("data:{};base64,{}", mime_type, audio_b64);

                let direct_payload = vec![
                    ("eas_header".to_string(), raw_header.to_string()),
                    ("description".to_string(), "".to_string()),
                    ("raw_audio".to_string(), raw_audio_data_uri),
                ];

                match client.post(&send_url).form(&direct_payload).send().await {
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        let body_lc = body.to_ascii_lowercase();

                        let size_related_failure = status == reqwest::StatusCode::PAYLOAD_TOO_LARGE
                            || (status == reqwest::StatusCode::ACCEPTED
                                && (body_lc.contains("too large") || body_lc.contains("chunk")));

                        if status.is_success() && !size_related_failure {
                            info!("Successfully relayed alert to DASDEC (direct)");
                        } else if size_related_failure {
                            warn!(
                                "Direct DASDEC relay hit size limit (status {}), switching to chunked upload. body='{}'",
                                status, body
                            );
                        } else {
                            warn!(
                                "DASDEC direct relay failed with status {}: body='{}'",
                                status, body
                            );
                        }
                    }
                    Err(err) => {
                        warn!("Failed to send DASDEC direct relay request: {}", err);
                    }
                }
            }

            const CHUNK_SIZE: usize = 128_000;

            let upload_id = format!(
                "relay-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or_default()
            );

            let total_chunks = (audio_b64.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
            if total_chunks == 0 {
                warn!("Chunked relay aborted: no audio data to send.");
                return Ok(());
            }

            for (idx, chunk_bytes) in audio_b64.as_bytes().chunks(CHUNK_SIZE).enumerate() {
                let is_last = idx + 1 == total_chunks;
                let chunk = match std::str::from_utf8(chunk_bytes) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("Chunk UTF-8 conversion failed: {}", err);
                        return Ok(());
                    }
                };

                let payload = vec![
                    ("upload_id".to_string(), upload_id.clone()),
                    ("eas_header".to_string(), raw_header.to_string()),
                    ("description".to_string(), "".to_string()),
                    ("audio_mime_type".to_string(), "audio/wav".to_string()),
                    ("raw_audio_chunk".to_string(), chunk.to_string()),
                    (
                        "is_last_chunk".to_string(),
                        if is_last { "true" } else { "false" }.to_string(),
                    ),
                ];

                let resp = match client.post(&send_chunk_url).form(&payload).send().await {
                    Ok(r) => r,
                    Err(err) => {
                        warn!("Failed sending chunk {}/{}: {}", idx + 1, total_chunks, err);
                        return Ok(());
                    }
                };

                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();

                if body.contains("\"error\"") {
                    warn!(
                        "Server returned error for chunk {}/{}: status {} body='{}'",
                        idx + 1,
                        total_chunks,
                        status,
                        body
                    );
                    return Ok(());
                }

                if !is_last {
                    if status != reqwest::StatusCode::ACCEPTED || !body.contains("chunk_received") {
                        warn!(
                            "Unexpected intermediate chunk response {}/{}: status {} body='{}'",
                            idx + 1,
                            total_chunks,
                            status,
                            body
                        );
                        return Ok(());
                    }
                } else if status == reqwest::StatusCode::OK && body.trim() == "OK" {
                    info!(
                        "Successfully relayed alert to DASDEC (chunked, {} chunks)",
                        total_chunks
                    );
                } else {
                    warn!("Final chunk failed: status {} body='{}'", status, body);
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::icecast_source_to_listener_url;

    #[test]
    fn derives_listener_url_stripping_credentials() {
        assert_eq!(
            icecast_source_to_listener_url(
                "icecast://source:hackme@stream.example.com:8000/eas.mp3"
            )
            .as_deref(),
            Some("http://stream.example.com:8000/eas.mp3")
        );
    }

    #[test]
    fn derives_listener_url_without_userinfo_or_path() {
        assert_eq!(
            icecast_source_to_listener_url("icecast://host:8000").as_deref(),
            Some("http://host:8000")
        );
    }

    #[test]
    fn maps_ssl_scheme_to_https() {
        assert_eq!(
            icecast_source_to_listener_url("icecast+ssl://u:p@host:8443/mount").as_deref(),
            Some("https://host:8443/mount")
        );
    }

    #[test]
    fn passes_through_plain_http_listener_url() {
        assert_eq!(
            icecast_source_to_listener_url("http://host:8000/mount").as_deref(),
            Some("http://host:8000/mount")
        );
    }
}
