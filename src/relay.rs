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
const TARGET_CHANNEL_LAYOUT: &str = "mono";

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

        let mut audio_segments = Vec::new();

        if !config.icecast_intro.as_os_str().is_empty() {
            audio_segments.push(config.icecast_intro.clone());
        }

        audio_segments.push(recorded_segment.to_path_buf());

        if !config.icecast_outro.as_os_str().is_empty() {
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
                            TARGET_CHANNEL_LAYOUT, TARGET_SAMPLE_RATE
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
                TARGET_SAMPLE_RATE,
                TARGET_SAMPLE_RATE,
                TARGET_CHANNEL_LAYOUT,
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
        prepare.arg("-ar").arg(TARGET_SAMPLE_RATE.to_string());
        prepare.arg("-ac").arg("1");
        prepare.arg("-c:a").arg("libvorbis");
        prepare.arg("-b:a").arg("128k");
        prepare.arg(&combined_path_buf);

        info!(path = %combined_path.display(), "Creating relay bundle with FFmpeg");
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

        if config.should_relay && config.should_relay_icecast {
            info!("Starting relay to Icecast servers...");
            if config.icecast_relay.is_empty() {
                return Err(anyhow!("ICECAST_RELAY is not set. Cannot start relay."));
            }

            let mut stream_cmd = Command::new("ffmpeg");
            stream_cmd.arg("-nostdin");
            stream_cmd.arg("-hide_banner");
            stream_cmd.arg("-loglevel").arg("info");
            stream_cmd.arg("-re");
            stream_cmd.arg("-i").arg(&combined_path_buf);
            stream_cmd.arg("-c:a").arg("copy");
            stream_cmd.arg("-f").arg("wav");
            stream_cmd
                .arg("-metadata")
                .arg(format!("title={}", "Emergency Alert"));
            stream_cmd
                .arg("-metadata")
                .arg(format!("artist={}", "EAS Listener"));
            stream_cmd.arg(&config.icecast_relay);

            info!(destination = %config.icecast_relay, "Streaming relay audio to Icecast");
            let stream_status = stream_cmd
                .status()
                .await
                .context("Failed to execute ffmpeg relay stream command")?;

            if !stream_status.success() {
                return Err(anyhow!(
                    "ffmpeg relay stream process exited with status {:?}",
                    stream_status.code()
                ));
            }

            combined_path
                .close()
                .context("Failed to clean up temporary relay bundle")?;
        }

        let should_relay_dasdec = config.should_relay && config.should_relay_dasdec;
        let dasdec_url = config.dasdec_url.clone();

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

            let audio_b64 = base64::engine::general_purpose::STANDARD
                .encode(tokio::fs::read(&combined_path_buf).await?);

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
