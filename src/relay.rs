use crate::config::Config;
use crate::filter::{self, FilterAction, FilterRule};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tempfile::Builder;
use tokio::process::Command;
use reqwest::Client;
use base64::Engine;
use reqwest::header::AUTHORIZATION;
use tracing::{info, warn};
use lazy_static::lazy_static;

lazy_static! {
    static ref json_config: Config =
        Config::from_config_json("/app/config.json").expect("Failed to load config");
}

const TARGET_SAMPLE_RATE: u32 = 48_000;
const TARGET_CHANNEL_LAYOUT: &str = "mono";

pub struct RelayState {
    pub config: Config,
}

impl RelayState {
    pub async fn new(config: Config) -> Result<Self> {
        if config.should_relay && config.icecast_relay.is_empty() {
            return Err(anyhow!("ICECAST_RELAY must be set if SHOULD_RELAY is true"));
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

        info!("Starting relay to Icecast servers...");

        let config = &self.config;
        let recorded_segment = recorded_segment.as_ref();

        if recorded_segment.as_os_str().is_empty() {
            return Err(anyhow!(
                "Recording segment path is empty. Cannot start relay."
            ));
        }

        if config.icecast_relay.is_empty() {
            return Err(anyhow!("ICECAST_RELAY is not set. Cannot start relay."));
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

        let should_relay_dasdec = json_config.should_relay_dasdec;
        let dasdec_url = json_config.dasdec_url.clone();

        if should_relay_dasdec && !dasdec_url.trim().is_empty() {
            let client = Client::new();

            let use_reverse_proxy = json_config.use_reverse_proxy;

            let latest_url = format!(
                "http{}://{}:{}/archive.php?latest_id=true",
                if use_reverse_proxy { "s" } else { "" },
                if use_reverse_proxy {
                    json_config.reverse_proxy_url.clone()
                } else {
                    "localhost".to_string()
                },
                if use_reverse_proxy {
                    "443".to_string()
                } else {
                    json_config.web_server_port.clone()
                }
            );

            let bearer_token =
                json_config.dashboard_username.clone() + ":" + &json_config.dashboard_password.clone();
            let bearer_token = Engine::encode(&base64::engine::general_purpose::STANDARD, bearer_token);

            let latest_id = match client
                .get(&latest_url)
                .header(AUTHORIZATION, format!("Bearer {}", bearer_token))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => match response.text().await {
                    Ok(text) => text.trim().to_string(),
                    Err(err) => {
                        warn!("Failed to read latest ID response body: {}", err);
                        "0".to_string()
                    }
                },
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        "Failed to fetch latest recording ID with status {}: body='{}'",
                        status, body
                    );
                    "0".to_string()
                }
                Err(err) => {
                    warn!("Failed to send latest ID request: {}", err);
                    "0".to_string()
                }
            };

            let audio_deeplink = {
                format!(
                    "http{}://{}:{}/archive.php?recording_id={}",
                    if use_reverse_proxy { "s" } else { "" },
                    if use_reverse_proxy {
                        json_config.reverse_proxy_url.clone()
                    } else {
                        "localhost".to_string()
                    },
                    if use_reverse_proxy {
                        "443".to_string()
                    } else {
                        json_config.web_server_port.clone()
                    },
                    latest_id
                )
            };

            let dasdec_payload = [
                ("eas_header", raw_header),
                ("description", ""),
                ("audio_deeplink", audio_deeplink.as_str()),
            ];

            match client.post(&dasdec_url).form(&dasdec_payload).send().await {
                Ok(response) if response.status().is_success() => {
                    info!("Successfully relayed alert to DASDEC");
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    warn!(
                        "DASDEC relay failed with status {}: body='{}'",
                        status, body
                    );
                }
                Err(err) => {
                    warn!("Failed to send DASDEC relay request: {}", err);
                }
            }
        }

        Ok(())
    }
}
