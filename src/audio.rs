use crate::config::Config;
use crate::monitoring::MonitoringHub;
use crate::recording::{self, RecordingState};
use crate::relay::RelayState;
use crate::state::{ActiveAlert, AppState, EasAlertData};
use crate::webhook::send_alert_webhook;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use chrono::{Local, Utc};
use rubato::{Resampler, SincFixedIn};
use sameold::{Message as SameMessage, SameReceiverBuilder};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Result as IoResult};
use std::sync::atomic::{AtomicBool, Ordering};
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
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender as TokioSender;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
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

fn nwr_tone_header_for_recording(
    current_same_header: Option<&str>,
    julian_timestamp: &str,
) -> String {
    if let Some(header) =
        current_same_header.filter(|header| header.starts_with("ZCZC-") && header.ends_with('-'))
    {
        header.to_string()
    } else {
        format!("ZCZC-WXR-??S-099999+0015-{julian_timestamp}-NOAA1050-")
    }
}

struct ChannelReader {
    rx: crossbeam_channel::Receiver<Bytes>,
    buffer: Bytes,
    pos: usize,
}

struct StreamWorkerHandle {
    stop_signal: Arc<AtomicBool>,
    task: JoinHandle<()>,
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
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    nnnn_tx: BroadcastSender<String>,
    monitoring: MonitoringHub,
    app_state: Arc<Mutex<AppState>>,
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
    let mut stream_tasks: HashMap<String, StreamWorkerHandle> = HashMap::new();
    for stream_url in config.icecast_stream_urls.clone() {
        if stream_tasks.contains_key(&stream_url) {
            warn!(
                stream = %stream_url,
                "Duplicate stream URL in ICECAST_STREAM_URL_ARRAY; only one worker will run for this URL."
            );
            continue;
        }

        let handle = spawn_stream_worker(
            current_config.clone(),
            stream_url.clone(),
            client.clone(),
            tx.clone(),
            recording_state.clone(),
            nnnn_tx.clone(),
            monitoring.clone(),
            app_state.clone(),
        );
        stream_tasks.insert(stream_url, handle);
    }

    let mut reload_enabled = true;
    while reload_enabled {
        match reload_rx.recv().await {
            Ok(new_config) => {
                let old_stream_urls = current_config
                    .read()
                    .expect("audio config lock poisoned")
                    .icecast_stream_urls
                    .clone();

                let old_stream_set: HashSet<String> = old_stream_urls.into_iter().collect();
                let mut new_stream_set: HashSet<String> = HashSet::new();
                for stream_url in &new_config.icecast_stream_urls {
                    if !new_stream_set.insert(stream_url.clone()) {
                        warn!(
                            stream = %stream_url,
                            "Duplicate stream URL in ICECAST_STREAM_URL_ARRAY; only one worker will run for this URL."
                        );
                    }
                }

                *current_config.write().expect("audio config lock poisoned") = new_config;

                let mut removed_count = 0usize;
                for stream_url in old_stream_set.difference(&new_stream_set) {
                    if let Some(handle) = stream_tasks.remove(stream_url) {
                        let mut handle = handle;
                        handle.stop_signal.store(true, Ordering::Relaxed);
                        match tokio::time::timeout(Duration::from_secs(5), &mut handle.task).await {
                            Ok(join_result) => {
                                if let Err(join_err) = join_result {
                                    if !join_err.is_cancelled() {
                                        warn!(
                                            stream = %stream_url,
                                            "Stream worker ended with join error while stopping: {}",
                                            join_err
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                handle.task.abort();
                                if let Err(join_err) = handle.task.await {
                                    if !join_err.is_cancelled() {
                                        warn!(
                                            stream = %stream_url,
                                            "Stream worker did not stop cleanly after timeout: {}",
                                            join_err
                                        );
                                    }
                                }
                            }
                        }
                        monitoring.remove_stream(stream_url);
                        info!(
                            stream = %stream_url,
                            "Stopped Icecast stream worker after configuration reload."
                        );
                        removed_count += 1;
                    } else {
                        monitoring.remove_stream(stream_url);
                    }
                }

                let mut added_count = 0usize;
                for stream_url in new_stream_set.difference(&old_stream_set) {
                    if stream_tasks.contains_key(stream_url) {
                        continue;
                    }
                    let handle = spawn_stream_worker(
                        current_config.clone(),
                        stream_url.clone(),
                        client.clone(),
                        tx.clone(),
                        recording_state.clone(),
                        nnnn_tx.clone(),
                        monitoring.clone(),
                        app_state.clone(),
                    );
                    stream_tasks.insert(stream_url.clone(), handle);
                    info!(
                        stream = %stream_url,
                        "Started Icecast stream worker after configuration reload."
                    );
                    added_count += 1;
                }

                if added_count > 0 || removed_count > 0 {
                    info!(
                        "Audio processor applied stream hot reload: {} added, {} removed.",
                        added_count, removed_count
                    );
                }

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

fn spawn_stream_worker(
    config: Arc<RwLock<Config>>,
    stream_url: String,
    client: reqwest::Client,
    tx: TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    nnnn_tx: BroadcastSender<String>,
    monitoring: MonitoringHub,
    app_state: Arc<Mutex<AppState>>,
) -> StreamWorkerHandle {
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_signal_for_worker = Arc::clone(&stop_signal);

    let task = tokio::spawn(async move {
        let stream_for_log = stream_url.clone();
        if let Err(e) = run_stream_task(
            config,
            stream_url,
            client,
            tx,
            recording_state,
            nnnn_tx,
            monitoring,
            app_state,
            stop_signal_for_worker,
        )
        .await
        {
            error!(stream = %stream_for_log, "Stream task terminated: {e:?}");
        }
    });

    StreamWorkerHandle { stop_signal, task }
}

async fn run_stream_task(
    config: Arc<RwLock<Config>>,
    stream_url: String,
    client: reqwest::Client,
    tx: TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: Arc<Mutex<HashMap<String, RecordingState>>>,
    nnnn_tx: BroadcastSender<String>,
    monitoring: MonitoringHub,
    app_state: Arc<Mutex<AppState>>,
    stop_signal: Arc<AtomicBool>,
) -> Result<()> {
    let mut last_log_time = Instant::now() - Duration::from_secs(61);
    let mut last_log_time2 = Instant::now() - Duration::from_secs(61);
    let mut last_connect_error_log = Instant::now() - Duration::from_secs(61);
    let mut connect_retry_attempt: u32 = 0;
    let mut suppressed_connect_errors: u32 = 0;

    loop {
        if stop_signal.load(Ordering::Relaxed) {
            break;
        }

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
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }

                if !response.status().is_success() {
                    connect_retry_attempt = connect_retry_attempt.saturating_add(1);
                    let retry_delay_secs = (1u64 << connect_retry_attempt.min(6)).min(60);
                    let retry_delay = Duration::from_secs(retry_delay_secs);
                    monitoring.note_error(
                        &stream_url,
                        format!("unexpected status: {}", response.status()),
                    );
                    if last_log_time2.elapsed() > Duration::from_secs(60) {
                        error!(
                            stream = %stream_url,
                            status = %response.status(),
                            retry_in_secs = retry_delay_secs,
                            attempt = connect_retry_attempt,
                            "Received non-success status code; retrying with exponential backoff"
                        );
                        last_log_time2 = Instant::now();
                    }
                    tokio::time::sleep(retry_delay).await;
                    continue;
                }

                connect_retry_attempt = 0;
                suppressed_connect_errors = 0;
                last_connect_error_log = Instant::now() - Duration::from_secs(61);
                monitoring.note_connected(&stream_url);
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);

                let (byte_tx, byte_rx) = crossbeam_channel::bounded::<Bytes>(256);

                let stream_for_reader = stream_url.clone();
                let monitoring_reader = monitoring.clone();
                let stop_signal_for_reader = Arc::clone(&stop_signal);
                tokio::spawn(async move {
                    let mut response = response;

                    let mut last_warn = std::time::Instant::now();

                    loop {
                        if stop_signal_for_reader.load(Ordering::Relaxed) {
                            break;
                        }

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
                let stop_signal_for_decode = Arc::clone(&stop_signal);
                let app_state_for_decode = app_state.clone();
                let monitoring_for_decode = monitoring.clone();
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
                        &stop_signal_for_decode,
                        &app_state_for_decode,
                        &monitoring_for_decode,
                    )
                });
                if let Err(e) = decoding_task.await? {
                    if !stop_signal.load(Ordering::Relaxed) {
                        monitoring.note_error(&stream_url, format!("decode error: {e}"));
                        error!(
                            stream = %stream_url,
                            "Error processing audio stream: {}. Reconnecting...",
                            e
                        );
                    }
                }
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }
                monitoring.note_disconnected(&stream_url);
            }
            Err(e) => {
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }
                connect_retry_attempt = connect_retry_attempt.saturating_add(1);
                let retry_delay_secs = (1u64 << connect_retry_attempt.min(6)).min(60);
                let retry_delay = Duration::from_secs(retry_delay_secs);
                if last_connect_error_log.elapsed() > Duration::from_secs(60) {
                    error!(
                        stream = %stream_url,
                        retry_in_secs = retry_delay_secs,
                        attempt = connect_retry_attempt,
                        suppressed_errors = suppressed_connect_errors,
                        "Failed to connect to Icecast stream: {}. Retrying with exponential backoff.",
                        e
                    );
                    last_connect_error_log = Instant::now();
                    suppressed_connect_errors = 0;
                } else {
                    suppressed_connect_errors = suppressed_connect_errors.saturating_add(1);
                }
                monitoring.note_error(&stream_url, format!("connect error: {e}"));
                tokio::time::sleep(retry_delay).await;
                continue;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

fn process_stream(
    mss: MediaSourceStream,
    content_type: Option<String>,
    config: &Arc<RwLock<Config>>,
    tx: &TokioSender<(String, String, String, String, Duration, String)>,
    recording_state: &Arc<Mutex<HashMap<String, RecordingState>>>,
    nnnn_tx: &BroadcastSender<String>,
    stream_label: &str,
    stop_signal: &Arc<AtomicBool>,
    app_state: &Arc<Mutex<AppState>>,
    monitoring: &MonitoringHub,
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
    const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 8;
    let mut consecutive_decode_errors: u32 = 0;

    loop {
        if stop_signal.load(Ordering::Relaxed) {
            break;
        }

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
                consecutive_decode_errors = 0;

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
                    if stop_signal.load(Ordering::Relaxed) {
                        break;
                    }

                    let chunk_to_process = audio_buffer[..CHUNK_SIZE].to_vec();
                    let resampled = rs.process(&[chunk_to_process], None)?;
                    let samples_f32 = resampled[0].clone();
                    let tone_present = tone_detector.detect(&samples_f32);

                    if let Some(audio_tx) = {
                        let recorder = recording_state.blocking_lock();
                        recorder
                            .get(stream_label)
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
                                if let Err(e) = nnnn_tx.send(stream_label.to_string()) {
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
                            if !recorder.contains_key(stream_label) {
                                let julian_timestamp = Utc::now().format("%j%H%M").to_string();
                                let full_timestamp =
                                    Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
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
                                        recorder.insert(stream_label.to_string(), new_state);
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
                            let app_state_for_tone = Arc::clone(app_state);
                            let monitoring_for_tone = monitoring.clone();
                            runtime.spawn(async move {
                                tokio::time::sleep(NWR_TONE_RECORDING_DURATION).await;

                                let stopped = {
                                    let mut recorder = recording_state_for_timeout.lock().await;
                                    if recorder
                                        .get(&stream_for_timeout)
                                        .is_some_and(|state| state.output_path == output_path)
                                    {
                                        if let Some(RecordingState { audio_tx, .. }) = recorder
                                            .remove(&stream_for_timeout)
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

                                let julian_timestamp = Utc::now().format("%j%H%M").to_string();

                                let raw_header = nwr_tone_header_for_recording(
                                    same_header_for_relay.as_deref(),
                                    &julian_timestamp,
                                );

                                let parsed_header =
                                    crate::e2t_ng::parse_header_json(&raw_header)
                                    .ok()
                                    .and_then(|json| {
                                        serde_json::from_str::<crate::e2t_ng::ParsedEasSerialized>(
                                            &json,
                                        )
                                        .ok()
                                    });
                                let tone_event_code = parsed_header
                                    .as_ref()
                                    .map(|parsed| parsed.event_code.clone())
                                    .unwrap_or_else(|| "??W".to_string());
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
                                        description: None,
                                        parsed_header,
                                    },
                                    raw_header.clone(),
                                    Duration::from_secs(15 * 60),
                                )
                                .with_source_stream_url(stream_for_timeout.clone());

                                send_alert_webhook(
                                    &stream_for_timeout,
                                    &tone_alert,
                                    &tone_details,
                                    &raw_header,
                                    Some(output_path.clone()),
                                )
                                .await;

                                {
                                    let active_snapshot = {
                                        let mut app_state_guard =
                                            app_state_for_tone.lock().await;
                                        let now_utc = Utc::now();
                                        app_state_guard.active_alerts.retain(|existing| {
                                            existing.expires_at > now_utc
                                                && existing.raw_header != raw_header
                                        });
                                        app_state_guard.active_alerts.push(tone_alert.clone());

                                        if let Err(e) = crate::alerts::update_alert_files(
                                            &config_for_relay.shared_state_dir,
                                            &app_state_guard,
                                        )
                                        .await
                                        {
                                            error!(
                                                stream = %stream_for_timeout,
                                                "Failed to update alert files for 1050 Hz tone: {}",
                                                e
                                            );
                                        }

                                        app_state_guard.active_alerts.clone()
                                    };
                                    monitoring_for_tone.broadcast_alerts(
                                        active_snapshot,
                                        Some(stream_for_timeout.as_str()),
                                        Some(tone_alert.data.event_code.as_str()),
                                    );
                                }

                                {
                                    let received_at = Utc::now();
                                    let local_time = received_at.with_timezone(&config_for_relay.timezone);
                                    let timestamp = local_time.format("%Y-%m-%d %l:%M:%S %p");
                                    let log_line = format!(
                                        "{}: {} (Received @ {})\n\n",
                                        raw_header, tone_details, timestamp
                                    );

                                    match OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open(&config_for_relay.dedicated_alert_log_file)
                                        .await
                                    {
                                        Ok(mut file) => {
                                            if let Err(e) = file.write_all(log_line.as_bytes()).await {
                                                warn!(
                                                    stream = %stream_for_timeout,
                                                    "Failed to write 1050 Hz tone to dedicated alert log: {}",
                                                    e
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                stream = %stream_for_timeout,
                                                "Failed to open dedicated alert log for 1050 Hz tone: {}",
                                                e
                                            );
                                        }
                                    }
                                }

                                if config_for_relay.should_relay
                                    && (config_for_relay.should_relay_icecast
                                        || config_for_relay.should_relay_dasdec)
                                {
                                    let relay_state =
                                        match RelayState::new(config_for_relay).await {
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
                                }
                            });
                        }
                    }
                    audio_buffer.drain(..CHUNK_SIZE);
                }
            }
            Err(e) => {
                consecutive_decode_errors = consecutive_decode_errors.saturating_add(1);
                if consecutive_decode_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                    return Err(anyhow!(
                        "Too many consecutive decode errors ({}). Dropping stream for reconnect. Last decode error: {}",
                        consecutive_decode_errors,
                        e
                    ));
                }
            }
        }
    }

    Ok(())
}
