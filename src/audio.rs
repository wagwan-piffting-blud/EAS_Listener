use crate::config::Config;
use crate::monitoring::MonitoringHub;
use crate::recording::{self, RecordingState};
use crate::relay::RelayState;
use crate::state::{ActiveAlert, EasAlertData};
use crate::webhook::send_alert_webhook;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use chrono::{Utc, Local};
use rubato::{Resampler, SincFixedIn};
use sameold::{Message as SameMessage, SameReceiverBuilder};
use std::io::{Read, Result as IoResult};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender as TokioSender;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{error, info, warn};

const TARGET_SAMPLE_RATE: u32 = 48000;
const CHUNK_SIZE: usize = 2048;
const NWR_TONE_FREQ_HZ: f32 = 1050.0;
const NWR_TONE_MIN_DURATION: Duration = Duration::from_secs(5);
const NWR_TONE_RECORDING_DURATION: Duration = Duration::from_secs(120);
const SAME_TONE_SUPPRESSION_DURATION: Duration = Duration::from_secs(300);

fn stream_inactivity_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(120)
}

fn nwr_tone_header_for_recording(current_same_header: Option<&str>, julian_timestamp: &str) -> String {
    if let Some(header) =
        current_same_header.filter(|header| header.starts_with("ZCZC-") && header.ends_with('-'))
    {
        header.to_string()
    } else {
        format!("ZCZC-WXR-??W-000000+0015-{julian_timestamp}-WAGSENDC-")
    }
}

struct ChannelReader {
    rx: crossbeam_channel::Receiver<Bytes>,
    buffer: Bytes,
    pos: usize,
}

struct GoertzelToneDetector {
    coeff: f32,
    ratio_threshold: f32,
    min_avg_power: f32,
    consecutive_hits_required: u8,
    consecutive_hits: u8,
}

impl GoertzelToneDetector {
    fn new(
        sample_rate_hz: f32,
        target_freq_hz: f32,
        ratio_threshold: f32,
        min_avg_power: f32,
        consecutive_hits_required: u8,
    ) -> Self {
        let omega = 2.0 * std::f32::consts::PI * target_freq_hz / sample_rate_hz;
        Self {
            coeff: 2.0 * omega.cos(),
            ratio_threshold,
            min_avg_power,
            consecutive_hits_required,
            consecutive_hits: 0,
        }
    }

    fn detect(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            self.consecutive_hits = 0;
            return false;
        }

        let mut q1 = 0.0f32;
        let mut q2 = 0.0f32;
        let mut total_energy = 0.0f32;

        for &sample in samples {
            let q0 = sample + self.coeff * q1 - q2;
            q2 = q1;
            q1 = q0;
            total_energy += sample * sample;
        }

        let tone_energy = (q1 * q1 + q2 * q2 - self.coeff * q1 * q2).max(0.0);
        let avg_power = total_energy / samples.len() as f32;
        let tone_ratio = tone_energy / total_energy.max(1e-12);
        let tone_hit = avg_power >= self.min_avg_power && tone_ratio >= self.ratio_threshold;

        if tone_hit {
            self.consecutive_hits = self.consecutive_hits.saturating_add(1);
        } else {
            self.consecutive_hits = 0;
        }

        self.consecutive_hits >= self.consecutive_hits_required
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.pos >= self.buffer.len() {
            match self.rx.recv() {
                Ok(new_buffer) => {
                    self.buffer = new_buffer;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let bytes_to_copy = (self.buffer.len() - self.pos).min(buf.len());
        let end = self.pos + bytes_to_copy;
        buf[..bytes_to_copy].copy_from_slice(&self.buffer[self.pos..end]);
        self.pos = end;
        Ok(bytes_to_copy)
    }
}

pub async fn run_audio_processor(
    config: Config,
    tx: TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: Arc<Mutex<Option<RecordingState>>>,
    nnnn_tx: BroadcastSender<()>,
    monitoring: MonitoringHub,
    mut reload_rx: BroadcastReceiver<Config>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .http1_only()
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    let current_config = Arc::new(RwLock::new(config.clone()));

    for stream_url in config.icecast_stream_urls.clone() {
        let config_clone = current_config.clone();
        let client_clone = client.clone();
        let tx_clone = tx.clone();
        let recording_state_clone = recording_state.clone();
        let nnnn_tx_clone = nnnn_tx.clone();
        let monitoring_clone = monitoring.clone();

        tokio::spawn(async move {
            let stream_for_log = stream_url.clone();
            if let Err(e) = run_stream_task(
                config_clone,
                stream_url,
                client_clone,
                tx_clone,
                recording_state_clone,
                nnnn_tx_clone,
                monitoring_clone,
            )
            .await
            {
                error!(stream = %stream_for_log, "Stream task terminated: {e:?}");
            }
        });
    }

    drop(tx);
    drop(nnnn_tx);

    let mut reload_enabled = true;
    while reload_enabled {
        match reload_rx.recv().await {
            Ok(new_config) => {
                let old_stream_urls = current_config
                    .read()
                    .expect("audio config lock poisoned")
                    .icecast_stream_urls
                    .clone();
                if old_stream_urls != new_config.icecast_stream_urls {
                    warn!(
                        "The reload updated ICECAST_STREAM_URL_ARRAY; a full Docker container restart is required to apply ANY stream URL changes."
                    );
                }

                *current_config.write().expect("audio config lock poisoned") = new_config;
                info!("Audio processor loaded updated configuration.");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    "Audio processor reload channel lagged; skipped {} update(s).",
                    skipped
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                warn!("Audio processor reload channel closed; keeping current configuration.");
                reload_enabled = false;
            }
        }
    }

    std::future::pending::<()>().await;
    #[allow(unreachable_code)]
    Ok(())
}

async fn run_stream_task(
    config: Arc<RwLock<Config>>,
    stream_url: String,
    client: reqwest::Client,
    tx: TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: Arc<Mutex<Option<RecordingState>>>,
    nnnn_tx: BroadcastSender<()>,
    monitoring: MonitoringHub,
) -> Result<()> {
    let mut last_log_time = Instant::now() - Duration::from_secs(61);
    let mut last_log_time2 = Instant::now() - Duration::from_secs(61);

    loop {
        monitoring.note_connecting(&stream_url);
        if last_log_time.elapsed() > Duration::from_secs(60) {
            info!(stream = %stream_url, "Connecting to Icecast stream");
            last_log_time = Instant::now();
        }

        match client
            .get(&stream_url)
            .header(
                reqwest::header::ACCEPT,
                "audio/*,application/ogg;q=0.9,*/*;q=0.1",
            )
            .header(reqwest::header::CONNECTION, "keep-alive")
            .send()
            .await
        {
            Ok(response) => {
                if !response.status().is_success() {
                    monitoring.note_error(
                        &stream_url,
                        format!("unexpected status: {}", response.status()),
                    );
                    if last_log_time2.elapsed() > Duration::from_secs(60) {
                        error!(
                            stream = %stream_url,
                            status = %response.status(),
                            "Received non-success status code; retrying"
                        );
                        last_log_time2 = Instant::now();
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                monitoring.note_connected(&stream_url);
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);

                let (byte_tx, byte_rx) = crossbeam_channel::bounded::<Bytes>(256);

                let stream_for_reader = stream_url.clone();
                let monitoring_reader = monitoring.clone();
                tokio::spawn(async move {
                    let mut response = response;

                    let mut last_warn = std::time::Instant::now();

                    loop {
                        match tokio::time::timeout(stream_inactivity_timeout(), response.chunk())
                            .await
                        {
                            Ok(Ok(Some(chunk))) => match byte_tx.try_send(chunk) {
                                Ok(_) => {
                                    monitoring_reader.note_activity(&stream_for_reader);
                                }
                                Err(crossbeam_channel::TrySendError::Full(_)) => {
                                    if last_warn.elapsed() > std::time::Duration::from_secs(30) {
                                        tracing::warn!(stream=%stream_for_reader, "Decoder backpressure: dropping audio chunks to keep socket draining");
                                        last_warn = std::time::Instant::now();
                                    }
                                }
                                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                                    break;
                                }
                            },
                            Ok(Ok(None)) => {
                                monitoring_reader
                                    .note_error(&stream_for_reader, "EOF from server".to_string());
                                break;
                            }
                            Ok(Err(e)) => {
                                monitoring_reader.note_error(
                                    &stream_for_reader,
                                    format!("chunk read error: {e}"),
                                );
                                break;
                            }
                            Err(_) => {
                                tracing::warn!(stream=%stream_for_reader, "Audio stream stalled; reconnecting");
                                monitoring_reader
                                    .note_error(&stream_for_reader, "stream stalled".to_string());
                                break;
                            }
                        }
                    }
                });

                let tx_clone = tx.clone();
                let recording_state_clone = recording_state.clone();
                let nnnn_tx_clone = nnnn_tx.clone();
                let config_for_decode = config.clone();
                let stream_for_decode = stream_url.clone();
                let decoding_task = tokio::task::spawn_blocking(move || {
                    let reader = ChannelReader {
                        rx: byte_rx,
                        buffer: Bytes::new(),
                        pos: 0,
                    };
                    let source = ReadOnlySource::new(reader);
                    let mss = MediaSourceStream::new(Box::new(source), Default::default());
                    process_stream(
                        mss,
                        content_type,
                        &config_for_decode,
                        &tx_clone,
                        &recording_state_clone,
                        &nnnn_tx_clone,
                        &stream_for_decode,
                    )
                });
                if let Err(e) = decoding_task.await? {
                    monitoring.note_error(&stream_url, format!("decode error: {e}"));
                    error!(
                        stream = %stream_url,
                        "Error processing audio stream: {}. Reconnecting...",
                        e
                    );
                }
                monitoring.note_disconnected(&stream_url);
            }
            Err(e) => {
                error!(
                    stream = %stream_url,
                    "Failed to connect to Icecast stream: {}. Retrying...",
                    e
                );
                monitoring.note_error(&stream_url, format!("connect error: {e}"));
                continue;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn process_stream(
    mss: MediaSourceStream,
    content_type: Option<String>,
    config: &Arc<RwLock<Config>>,
    tx: &TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: &Arc<Mutex<Option<RecordingState>>>,
    nnnn_tx: &BroadcastSender<()>,
    stream_label: &str,
) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();

    let mut hint = Hint::new();
    if let Some(ct) = content_type {
        if ct.contains("audio/mpeg") {
            hint.with_extension("mp3");
        }
    }
    let fmt_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &MetadataOptions::default())
        .context("Unsupported format")?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No default track found"))?;
    let mut track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("Failed to make decoder")?;

    let mut same_receiver = SameReceiverBuilder::new(TARGET_SAMPLE_RATE).build();
    let mut resampler: Option<SincFixedIn<f32>> = None;
    let mut current_input_rate: Option<u32> = None;
    let mut audio_buffer: Vec<f32> = Vec::new();
    let mut tone_detector =
        GoertzelToneDetector::new(TARGET_SAMPLE_RATE as f32, NWR_TONE_FREQ_HZ, 60.0, 5e-5, 8);
    let mut tone_rearm_until: Option<std::time::Instant> = None;
    let mut same_tone_suppression_until: Option<std::time::Instant> = None;
    let mut current_same_header: Option<String> = None;
    let min_tone_samples_required =
        (TARGET_SAMPLE_RATE as f64 * NWR_TONE_MIN_DURATION.as_secs_f64()) as usize;
    let mut sustained_tone_samples: usize = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(pkt) => pkt,
            Err(SymphoniaError::ResetRequired) => {
                if let Some(new_track) = format.default_track() {
                    track_id = new_track.id;
                    decoder = symphonia::default::get_codecs()
                        .make(&new_track.codec_params, &DecoderOptions::default())
                        .context("Failed to rebuild decoder after ResetRequired")?;
                }
                current_input_rate = None;
                resampler = None;
                audio_buffer.clear();
                continue;
            }
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => {
                error!(stream = %stream_label, "Packet error: {}", e);
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                if decoded.frames() == 0 {
                    continue;
                }
                let spec = *decoded.spec();

                if current_input_rate != Some(spec.rate) {
                    current_input_rate = Some(spec.rate);
                    use rubato::{
                        SincInterpolationParameters, SincInterpolationType, WindowFunction,
                    };
                    if current_input_rate.unwrap() == TARGET_SAMPLE_RATE {
                        resampler = Some(
                            SincFixedIn::new(
                                TARGET_SAMPLE_RATE as f64 / spec.rate as f64,
                                2.0,
                                SincInterpolationParameters {
                                    sinc_len: 256,
                                    f_cutoff: 0.95,
                                    interpolation: SincInterpolationType::Linear,
                                    oversampling_factor: 256,
                                    window: WindowFunction::BlackmanHarris2,
                                },
                                CHUNK_SIZE,
                                1, // mono
                            )
                            .expect("failed to create resampler"),
                        );
                    } else {
                        info!(
                            stream = %stream_label,
                            "Stream detected with sample rate {}. Resampling to {}.",
                            spec.rate,
                            TARGET_SAMPLE_RATE
                        );
                        resampler = Some(
                            SincFixedIn::new(
                                TARGET_SAMPLE_RATE as f64 / spec.rate as f64,
                                2.0,
                                SincInterpolationParameters {
                                    sinc_len: 256,
                                    f_cutoff: 0.95,
                                    interpolation: SincInterpolationType::Linear,
                                    oversampling_factor: 256,
                                    window: WindowFunction::BlackmanHarris2,
                                },
                                CHUNK_SIZE,
                                1, // mono
                            )
                            .expect("failed to create resampler"),
                        );
                    }
                }
                let rs = resampler
                    .as_mut()
                    .expect("resampler must be initialized when decoding begins");

                let mut mono_samples = vec![0.0f32; decoded.frames()];
                let mut sample_buf = SampleBuffer::<f32>::new(decoded.frames() as u64, spec);
                sample_buf.copy_interleaved_ref(decoded);
                for (i, frame) in sample_buf
                    .samples()
                    .chunks_exact(spec.channels.count())
                    .enumerate()
                {
                    mono_samples[i] = frame.iter().sum::<f32>() / frame.len() as f32;
                }
                audio_buffer.extend_from_slice(&mono_samples);

                while audio_buffer.len() >= CHUNK_SIZE {
                    let chunk_to_process = audio_buffer[..CHUNK_SIZE].to_vec();
                    let resampled = rs.process(&[chunk_to_process], None)?;
                    let samples_f32 = resampled[0].clone();
                    let tone_present = tone_detector.detect(&samples_f32);

                    if let Some(audio_tx) = {
                        let recorder = recording_state.blocking_lock();
                        recorder
                            .as_ref()
                            .filter(|state| state.source_stream == stream_label)
                            .map(|state| state.audio_tx.clone())
                    } {
                        if let Err(e) = audio_tx.try_send(samples_f32.clone()) {
                            if let TrySendError::Closed(_) = e {
                                warn!(
                                    stream = %stream_label,
                                    "Recording task channel closed unexpectedly."
                                );
                            }
                        }
                    }

                    let now = std::time::Instant::now();
                    for msg in same_receiver.iter_messages(samples_f32.iter().copied()) {
                        match msg {
                            SameMessage::StartOfMessage(header) => {
                                same_tone_suppression_until =
                                    Some(now + SAME_TONE_SUPPRESSION_DURATION);
                                let event = header.event_str().to_string();
                                let locations =
                                    header.location_str_iter().collect::<Vec<_>>().join(", ");
                                let originator = header.originator_str().to_string();
                                let raw_header = header.as_str().to_string();
                                current_same_header = Some(raw_header.clone());
                                let purge_time = header.valid_duration();
                                let std_purge_time =
                                    Duration::from_secs(purge_time.num_seconds().max(0) as u64);
                                if let Err(e) = runtime.block_on(tx.send((
                                    event,
                                    locations,
                                    originator,
                                    raw_header,
                                    std_purge_time,
                                    stream_label.to_string(),
                                ))) {
                                    error!(stream = %stream_label, "Failed to send decoded data: {}", e);
                                }
                            }
                            SameMessage::EndOfMessage => {
                                same_tone_suppression_until = None;
                                current_same_header = None;
                                info!(stream = %stream_label, "NNNN (End of Message) detected");
                                if let Err(e) = nnnn_tx.send(()) {
                                    error!(stream = %stream_label, "Failed to broadcast NNNN signal: {}", e);
                                }
                            }
                        }
                    }

                    let same_suppression_active = match same_tone_suppression_until {
                        Some(deadline) if now < deadline => true,
                        Some(_) => {
                            same_tone_suppression_until = None;
                            false
                        }
                        None => false,
                    };
                    let tone_rearm_ready = match tone_rearm_until {
                        Some(ready_at) => now >= ready_at,
                        None => true,
                    };
                    if same_suppression_active || !tone_rearm_ready {
                        sustained_tone_samples = 0;
                    } else if tone_present {
                        sustained_tone_samples =
                            sustained_tone_samples.saturating_add(samples_f32.len());
                    } else {
                        sustained_tone_samples = 0;
                    }

                    if !same_suppression_active
                        && tone_rearm_ready
                        && sustained_tone_samples >= min_tone_samples_required
                    {
                        let tone_recording = {
                            let mut recorder = recording_state.blocking_lock();
                            if recorder.is_none() {
                                let julian_timestamp = Utc::now().format("%j%H%M").to_string();
                                let full_timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
                                let config_snapshot =
                                    config.read().expect("audio config lock poisoned").clone();
                                let tone_header = nwr_tone_header_for_recording(
                                    current_same_header.as_deref(),
                                    &julian_timestamp,
                                );
                                match recording::start_encoding_task_with_timestamp(
                                    &config_snapshot,
                                    &tone_header,
                                    stream_label,
                                    Some(&full_timestamp),
                                ) {
                                    Ok((handle, new_state)) => {
                                        let output_path = new_state.output_path.clone();
                                        *recorder = Some(new_state);
                                        Some((handle, output_path))
                                    }
                                    Err(e) => {
                                        warn!(
                                            stream = %stream_label,
                                            "Failed to start 1050 Hz tone recording: {}",
                                            e
                                        );
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        };

                        if let Some((handle, output_path)) = tone_recording {
                            sustained_tone_samples = 0;
                            tone_rearm_until = Some(now + NWR_TONE_RECORDING_DURATION);
                            info!(
                                stream = %stream_label,
                                "Detected 1050 Hz tone. Recording for {} seconds.",
                                NWR_TONE_RECORDING_DURATION.as_secs()
                            );

                            let recording_state_for_timeout = Arc::clone(recording_state);
                            let stream_for_timeout = stream_label.to_string();
                            let (config_for_relay, filters_for_relay) = {
                                let config_snapshot =
                                    config.read().expect("audio config lock poisoned").clone();
                                let filters = config_snapshot.filters.clone();
                                (config_snapshot, filters)
                            };
                            let same_header_for_relay = current_same_header.clone();
                            runtime.spawn(async move {
                                tokio::time::sleep(NWR_TONE_RECORDING_DURATION).await;

                                let stopped = {
                                    let mut recorder = recording_state_for_timeout.lock().await;
                                    if recorder.as_ref().is_some_and(|state| {
                                        state.source_stream == stream_for_timeout
                                            && state.output_path == output_path
                                    }) {
                                        if let Some(RecordingState { audio_tx, .. }) =
                                            recorder.take()
                                        {
                                            drop(audio_tx);
                                            true
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    }
                                };

                                if stopped {
                                    info!(
                                        stream = %stream_for_timeout,
                                        "1050 Hz tone recording window ended after {} seconds.",
                                        NWR_TONE_RECORDING_DURATION.as_secs()
                                    );
                                }

                                match handle.await {
                                    Ok(Ok(())) => {}
                                    Ok(Err(e)) => warn!(
                                        stream = %stream_for_timeout,
                                        "1050 Hz recording task failed: {}",
                                        e
                                    ),
                                    Err(e) => warn!(
                                        stream = %stream_for_timeout,
                                        "1050 Hz recording task join error: {}",
                                        e
                                    ),
                                }

                                let relay_state = match RelayState::new(config_for_relay).await {
                                    Ok(state) => state,
                                    Err(err) => {
                                        warn!(
                                            stream = %stream_for_timeout,
                                            "Skipping 1050 Hz relay due to configuration error: {:?}",
                                            err
                                        );
                                        return;
                                    }
                                };

                                let julian_timestamp = Utc::now().format("%j%H%M").to_string();

                                let raw_header = nwr_tone_header_for_recording(
                                    same_header_for_relay.as_deref(),
                                    &julian_timestamp,
                                );

                                let tone_event_code =
                                    raw_header.get(9..12).unwrap_or("??W").to_string();
                                let tone_details = format!(
                                    "Detected 1050 Hz NOAA Weather Radio tone on stream {}.",
                                    stream_for_timeout
                                );
                                let tone_alert = ActiveAlert::new(
                                    EasAlertData {
                                        eas_text: tone_details.clone(),
                                        event_text: "1050".to_string(),
                                        event_code: tone_event_code,
                                        fips: vec!["000000".to_string()],
                                        locations: "Unknown".to_string(),
                                        originator: "WXR".to_string(),
                                    },
                                    raw_header.clone(),
                                    Duration::from_secs(15 * 60),
                                );

                                send_alert_webhook(
                                    &stream_for_timeout,
                                    &tone_alert,
                                    &tone_details,
                                    &raw_header,
                                    Some(output_path.clone()),
                                )
                                .await;

                                if let Err(err) = relay_state
                                    .start_relay(
                                        "??W",
                                        filters_for_relay.as_slice(),
                                        &output_path,
                                        Some(stream_for_timeout.as_str()),
                                        &raw_header,
                                    )
                                    .await
                                {
                                    warn!(
                                        stream = %stream_for_timeout,
                                        "1050 Hz relay failed: {:?}",
                                        err
                                    );
                                }
                            });
                        }
                    }
                    audio_buffer.drain(..CHUNK_SIZE);
                }
            }
            Err(e) => {
                warn!(stream = %stream_label, "Decode error: {}", e);
            }
        }
    }

    Ok(())
}
