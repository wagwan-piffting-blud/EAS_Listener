//! The 24/7 alert stream: comfort noise between alerts, and each alert's audio as it arrives,
//! encoded as Ogg Vorbis (Ogg Opus where ffmpeg lacks libvorbis) and served over HTTP by the
//! listener itself.
//!
//! A local Icecast server used to do the serving, with ffmpeg as its source. Its only job was
//! handing one stream to many listeners, which `StreamHub` does now: ffmpeg encodes to its stdout,
//! `OggPageReader` cuts that into pages, and every page goes out to whoever is connected. The
//! stream's header pages are kept, so a listener who joins mid-stream is sent them first and can
//! start decoding at the next page -- which is how Icecast serves Ogg as well. The URL is the one
//! Icecast served: `http://<host>:ICECAST_ALERT_PORT` followed by `ICECAST_ALERT_MOUNT`.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use once_cell::sync::{Lazy, OnceCell};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::broadcast::{self, Receiver as BroadcastReceiver};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{info, warn};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 1;
const CHUNK_MS: u64 = 100;
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * CHUNK_MS as usize;
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2;
const INTER_ALERT_GAP_BYTES: usize = (SAMPLE_RATE as usize) * 2;
const RESTART_BACKOFF: Duration = Duration::from_secs(5);
const COMFORT_NOISE_PEAK: i16 = 32;
const NOISE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Pages a slow listener may fall behind by before it skips ahead. ffmpeg writes a page about
/// every second at this bitrate, so this is minutes of slack.
const PAGE_BACKLOG: usize = 512;
const OGG_CAPTURE: &[u8; 4] = b"OggS";
const OGG_HEADER_LEN: usize = 27;
/// Set on the first page of a logical stream.
const OGG_BEGINNING_OF_STREAM: u8 = 0x02;
/// The granule position of a page on which no packet ends -- a header split across pages.
const OGG_NO_GRANULE: u64 = u64::MAX;

static ALERT_STREAM_TX: OnceCell<mpsc::UnboundedSender<PathBuf>> = OnceCell::new();
static HUB: Lazy<StreamHub> = Lazy::new(StreamHub::new);

pub fn enqueue_alert_audio(path: PathBuf) {
    if let Some(tx) = ALERT_STREAM_TX.get() {
        if let Err(err) = tx.send(path) {
            warn!(
                "Failed to enqueue alert audio for the alert stream: {}",
                err
            );
        }
    }
}

/// One stream to many listeners. `headers` is what a listener is sent before anything else; it
/// changes together with the broadcast, under one lock, so a listener joining while ffmpeg
/// restarts neither misses the new stream's header pages nor gets them twice.
struct StreamHub {
    state: Mutex<HubState>,
}

struct HubState {
    headers: Vec<Bytes>,
    collecting_headers: bool,
    pages: broadcast::Sender<Bytes>,
}

impl StreamHub {
    fn new() -> Self {
        Self {
            state: Mutex::new(HubState {
                headers: Vec::new(),
                collecting_headers: false,
                pages: broadcast::channel(PAGE_BACKLOG).0,
            }),
        }
    }

    /// A Vorbis or Opus stream opens with header pages at granule position 0, or none where a
    /// header runs onto another page; the first page with a real position is audio.
    fn publish(&self, page: Bytes) {
        let mut state = self.state.lock().expect("alert stream hub lock poisoned");
        if ogg_page_starts_stream(&page) {
            state.headers.clear();
            state.collecting_headers = true;
        }
        if state.collecting_headers {
            let granule = ogg_page_granule(&page);
            if granule == 0 || granule == OGG_NO_GRANULE {
                state.headers.push(page.clone());
            } else {
                state.collecting_headers = false;
            }
        }
        // Nobody listening is not a failure.
        let _ = state.pages.send(page);
    }

    /// The header pages to send first, and the pages that follow them.
    fn join(&self) -> (Vec<Bytes>, broadcast::Receiver<Bytes>) {
        let state = self.state.lock().expect("alert stream hub lock poisoned");
        (state.headers.clone(), state.pages.subscribe())
    }
}

/// Cuts a byte stream into whole Ogg pages, however the reads happen to split it.
#[derive(Default)]
struct OggPageReader {
    buffer: Vec<u8>,
}

impl OggPageReader {
    fn push(&mut self, data: &[u8]) -> Vec<Bytes> {
        self.buffer.extend_from_slice(data);
        let mut pages = Vec::new();
        loop {
            // ffmpeg's output starts on a capture pattern, so this only matters if bytes are
            // ever lost: skip to the next one rather than emit a broken page.
            match self
                .buffer
                .windows(OGG_CAPTURE.len())
                .position(|window| window == OGG_CAPTURE)
            {
                Some(0) => {}
                Some(offset) => {
                    self.buffer.drain(..offset);
                }
                None => {
                    let keep = self.buffer.len().min(OGG_CAPTURE.len() - 1);
                    self.buffer.drain(..self.buffer.len() - keep);
                    break;
                }
            }
            if self.buffer.len() < OGG_HEADER_LEN {
                break;
            }
            let segments = self.buffer[OGG_HEADER_LEN - 1] as usize;
            let table_end = OGG_HEADER_LEN + segments;
            if self.buffer.len() < table_end {
                break;
            }
            let body: usize = self.buffer[OGG_HEADER_LEN..table_end]
                .iter()
                .map(|&lacing| lacing as usize)
                .sum();
            let page_len = table_end + body;
            if self.buffer.len() < page_len {
                break;
            }
            pages.push(Bytes::copy_from_slice(&self.buffer[..page_len]));
            self.buffer.drain(..page_len);
        }
        pages
    }
}

fn ogg_page_starts_stream(page: &[u8]) -> bool {
    page.get(5)
        .is_some_and(|flags| flags & OGG_BEGINNING_OF_STREAM != 0)
}

fn ogg_page_granule(page: &[u8]) -> u64 {
    page.get(6..14)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(OGG_NO_GRANULE)
}

async fn decode_to_pcm(path: &Path) -> Result<Vec<u8>> {
    let output = Command::new(crate::components::ffmpeg())
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-f")
        .arg("s16le")
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-ac")
        .arg(CHANNELS.to_string())
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .with_context(|| format!("Failed to run ffmpeg to decode {}", path.display()))?;

    if !output.status.success() {
        bail!(
            "ffmpeg decode of {} exited with status {:?}",
            path.display(),
            output.status.code()
        );
    }

    let mut bytes = output.stdout;
    if bytes.len() % 2 == 1 {
        bytes.pop();
    }
    Ok(bytes)
}

#[inline]
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn write_comfort_noise(dst: &mut [u8], state: &mut u64) {
    let span = COMFORT_NOISE_PEAK as u64 * 2 + 1;
    for pair in dst.as_chunks_mut::<2>().0 {
        let sample = (next_rand(state) % span) as i32 - COMFORT_NOISE_PEAK as i32;
        *pair = (sample as i16).to_le_bytes();
    }
}

fn comfort_noise_chunk(state: &mut u64) -> Vec<u8> {
    let mut out = vec![0u8; CHUNK_BYTES];
    write_comfort_noise(&mut out, state);
    out
}

/// What goes inside the Ogg stream: Vorbis wherever ffmpeg has libvorbis, since every player
/// takes it; Opus otherwise. Homebrew's ffmpeg, for one, is built without libvorbis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCodec {
    Vorbis,
    Opus,
}

impl StreamCodec {
    fn ffmpeg_args(self) -> [&'static str; 4] {
        match self {
            StreamCodec::Vorbis => ["-c:a", "libvorbis", "-b:a", "128k"],
            StreamCodec::Opus => ["-c:a", "libopus", "-b:a", "96k"],
        }
    }

    fn from_encoder_list(list: &str) -> Self {
        let has = |name: &str| {
            list.lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name))
        };
        if !has("libvorbis") && has("libopus") {
            StreamCodec::Opus
        } else {
            StreamCodec::Vorbis
        }
    }
}

static CODEC: Mutex<Option<(PathBuf, StreamCodec)>> = Mutex::new(None);

/// Asked of ffmpeg once per binary, since the configured one can change on reload.
fn stream_codec() -> StreamCodec {
    let ffmpeg = crate::components::ffmpeg();
    let mut cached = CODEC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((path, codec)) = cached.as_ref() {
        if *path == ffmpeg {
            return *codec;
        }
    }
    let codec = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|output| StreamCodec::from_encoder_list(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(StreamCodec::Vorbis);
    if codec == StreamCodec::Opus {
        info!(
            "{} has no libvorbis, so the alert stream is Ogg Opus instead of Ogg Vorbis.",
            ffmpeg.display()
        );
    }
    *cached = Some((ffmpeg, codec));
    codec
}

/// ffmpeg turning PCM on its stdin into an Ogg stream on its stdout.
struct Encoder {
    child: Child,
    stdin: ChildStdin,
}

fn spawn_encoder() -> Result<Encoder> {
    let mut child = Command::new(crate::components::ffmpeg())
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-f")
        .arg("s16le")
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-ac")
        .arg(CHANNELS.to_string())
        .arg("-i")
        .arg("pipe:0")
        .args(stream_codec().ffmpeg_args())
        // Without this ffmpeg holds pages in its output buffer, seconds behind live.
        .arg("-flush_packets")
        .arg("1")
        .arg("-f")
        .arg("ogg")
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to start ffmpeg for the alert stream")?;
    let stdin = child
        .stdin
        .take()
        .context("ffmpeg for the alert stream had no stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("ffmpeg for the alert stream had no stdout")?;
    tokio::spawn(read_pages(stdout));
    Ok(Encoder { child, stdin })
}

/// Ends by itself when ffmpeg exits, which dropping its `Encoder` makes happen.
async fn read_pages(mut stdout: ChildStdout) {
    let mut reader = OggPageReader::default();
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for page in reader.push(&buffer[..read]) {
                    HUB.publish(page);
                }
            }
        }
    }
}

/// What the HTTP side is bound to; a change to any of it means rebinding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerSpec {
    bind: SocketAddr,
    mount: String,
    name: String,
}

impl ServerSpec {
    fn from_config(config: &Config) -> Self {
        let name = if config.eas_relay_name.trim().is_empty() {
            "EAS Listener".to_string()
        } else {
            config.eas_relay_name.clone()
        };
        Self {
            // The dashboard's interface, so the stream is exactly as reachable as it is.
            bind: SocketAddr::new(config.monitoring_bind_addr.ip(), config.icecast_alert_port),
            mount: config.icecast_alert_mount.clone(),
            name,
        }
    }
}

async fn serve(spec: ServerSpec) {
    let listener = match crate::backend::bind_with_retry(spec.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            warn!(
                "Could not listen on {} for the alert stream: {}. Retrying every {}s.",
                spec.bind,
                err,
                RESTART_BACKOFF.as_secs()
            );
            return;
        }
    };
    info!(bind = %spec.bind, mount = %spec.mount, "Serving the alert stream");
    let name = spec.name.clone();
    let app = Router::new().route(&spec.mount, get(move || stream_response(name.clone())));
    if let Err(err) = axum::serve(listener, app).await {
        warn!("The alert stream's server stopped: {}", err);
    }
}

async fn stream_response(name: String) -> Response {
    let (headers, pages) = HUB.join();
    let first = tokio_stream::iter(headers.into_iter().map(Ok::<Bytes, std::io::Error>));
    // A listener too slow for the backlog skips ahead rather than being cut off; a Vorbis decoder
    // resynchronises at the next page.
    let live = BroadcastStream::new(pages).filter_map(|page| page.ok().map(Ok));
    let mut response = Body::from_stream(first.chain(live)).into_response();
    let response_headers = response.headers_mut();
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store"),
    );
    response_headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    if let Ok(value) = HeaderValue::from_str(&name) {
        response_headers.insert("icy-name", value);
    }
    response_headers.insert(
        "icy-description",
        HeaderValue::from_static("Live EAS alert audio stream"),
    );
    response
}

pub async fn run_alert_stream(
    mut config: Config,
    mut reload_rx: BroadcastReceiver<Config>,
) -> Result<()> {
    let (path_tx, mut path_rx) = mpsc::unbounded_channel::<PathBuf>();
    if ALERT_STREAM_TX.set(path_tx).is_err() {
        warn!("The alert stream channel was already initialized; ignoring duplicate task.");
        return Ok(());
    }

    let mut interval = tokio::time::interval(Duration::from_millis(CHUNK_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut noise_state: u64 = NOISE_SEED;

    let mut encoder: Option<Encoder> = None;
    let mut last_encoder_attempt: Option<Instant> = None;
    let mut encoder_failures: u64 = 0;
    let mut server: Option<(ServerSpec, JoinHandle<()>, Instant)> = None;

    let mut current: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    let mut gap_remaining = 0usize;
    let mut logged_disabled = false;

    loop {
        loop {
            match reload_rx.try_recv() {
                Ok(new_config) => config = new_config,
                Err(broadcast::error::TryRecvError::Empty)
                | Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }

        if !config.icecast_alert_stream_enabled {
            if let Some((_, handle, _)) = server.take() {
                handle.abort();
            }
            if encoder.take().is_some() {
                info!("Alert stream disabled; stopped serving it.");
            }
            if !logged_disabled {
                info!("Alert stream is disabled; standing by.");
                logged_disabled = true;
            }
            current = None;
            pos = 0;
            gap_remaining = 0;

            tokio::select! {
                reload = reload_rx.recv() => {
                    match reload {
                        Ok(new_config) => config = new_config,
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            if path_rx.recv().await.is_none() {
                                return Ok(());
                            }
                        }
                    }
                }
                drained = path_rx.recv() => {
                    if drained.is_none() {
                        return Ok(());
                    }
                }
            }
            continue;
        }
        logged_disabled = false;

        // Rebind when the address or mount changed, and retry a bind that failed.
        let wanted = ServerSpec::from_config(&config);
        let rebind = match &server {
            None => true,
            Some((spec, _, _)) if *spec != wanted => true,
            Some((_, handle, started)) => {
                handle.is_finished() && started.elapsed() >= RESTART_BACKOFF
            }
        };
        if rebind {
            if let Some((_, handle, _)) = server.take() {
                handle.abort();
            }
            server = Some((wanted.clone(), tokio::spawn(serve(wanted)), Instant::now()));
        }

        if let Some(running) = encoder.as_mut() {
            if matches!(running.child.try_wait(), Ok(Some(_)) | Err(_)) {
                warn!("The alert stream's encoder exited; restarting it.");
                encoder = None;
            }
        }

        if encoder.is_none() {
            let ready = last_encoder_attempt
                .map(|attempted| attempted.elapsed() >= RESTART_BACKOFF)
                .unwrap_or(true);
            if ready {
                last_encoder_attempt = Some(Instant::now());
                match spawn_encoder() {
                    Ok(started) => {
                        encoder_failures = 0;
                        encoder = Some(started);
                    }
                    Err(err) => {
                        encoder_failures += 1;
                        if encoder_failures == 1 || encoder_failures.is_multiple_of(12) {
                            warn!(
                                "Failed to start the alert stream's encoder (attempt {}): {}. \
                                 Retrying every {}s.",
                                encoder_failures,
                                err,
                                RESTART_BACKOFF.as_secs()
                            );
                        }
                    }
                }
            }
        }

        interval.tick().await;

        let Some(running) = encoder.as_mut() else {
            continue;
        };

        let chunk: Vec<u8> = if let Some(buf) = current.as_ref() {
            let end = (pos + CHUNK_BYTES).min(buf.len());
            let mut out = buf[pos..end].to_vec();
            pos = end;
            if pos >= buf.len() {
                current = None;
                pos = 0;
                gap_remaining = INTER_ALERT_GAP_BYTES;
            }
            if out.len() < CHUNK_BYTES {
                let start = out.len();
                out.resize(CHUNK_BYTES, 0);
                write_comfort_noise(&mut out[start..], &mut noise_state);
            }
            out
        } else {
            if gap_remaining > 0 {
                gap_remaining = gap_remaining.saturating_sub(CHUNK_BYTES);
            } else if let Ok(path) = path_rx.try_recv() {
                match decode_to_pcm(&path).await {
                    Ok(pcm) if !pcm.is_empty() => {
                        info!(
                            "Playing alert audio on the alert stream: {}",
                            path.display()
                        );
                        current = Some(pcm);
                        pos = 0;
                    }
                    Ok(_) => warn!(
                        "Decoded alert audio was empty; skipping: {}",
                        path.display()
                    ),
                    Err(err) => warn!("Failed to decode alert audio {}: {}", path.display(), err),
                }
            }
            comfort_noise_chunk(&mut noise_state)
        };

        if let Err(err) = running.stdin.write_all(&chunk).await {
            warn!(
                "Writing to the alert stream's encoder failed: {}. Restarting it.",
                err
            );
            encoder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_is_used_only_where_libvorbis_is_missing() {
        let homebrew = " A..X.D vorbis               Vorbis\n A....D libopus              libopus Opus (codec opus)\n";
        assert_eq!(StreamCodec::from_encoder_list(homebrew), StreamCodec::Opus);
        let full = " A....D libvorbis            libvorbis (codec vorbis)\n A....D libopus              libopus Opus (codec opus)\n";
        assert_eq!(StreamCodec::from_encoder_list(full), StreamCodec::Vorbis);
        // Neither: ffmpeg's own error is more useful than a guess.
        assert_eq!(StreamCodec::from_encoder_list(""), StreamCodec::Vorbis);
    }

    #[test]
    fn comfort_noise_is_bounded_nonsilent_and_advances() {
        let mut state = NOISE_SEED;
        let chunk = comfort_noise_chunk(&mut state);
        assert_eq!(chunk.len(), CHUNK_BYTES);

        let mut any_nonzero = false;
        for pair in chunk.as_chunks::<2>().0 {
            let sample = i16::from_le_bytes(*pair);
            assert!(
                (-COMFORT_NOISE_PEAK..=COMFORT_NOISE_PEAK).contains(&sample),
                "sample {sample} out of comfort-noise bounds"
            );
            any_nonzero |= sample != 0;
        }
        assert!(any_nonzero, "comfort noise must not be pure silence");

        let next = comfort_noise_chunk(&mut state);
        assert_ne!(chunk, next);
    }

    /// A structurally valid page; the CRC is not checked here, so it is left zero.
    fn page(flags: u8, granule: u64, body: &[u8]) -> Vec<u8> {
        let mut lacing = vec![255u8; body.len() / 255];
        lacing.push((body.len() % 255) as u8);
        let mut out = OGG_CAPTURE.to_vec();
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&[0; 12]);
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn pages_are_cut_whole_however_the_reads_split_them() {
        let first = page(OGG_BEGINNING_OF_STREAM, 0, &[1; 30]);
        let second = page(0, 4800, &[2; 600]);
        let mut stream = first.clone();
        stream.extend_from_slice(&second);

        for split in [1, 5, 27, 29, first.len(), first.len() + 3, stream.len()] {
            let mut reader = OggPageReader::default();
            let mut pages = reader.push(&stream[..split]);
            pages.extend(reader.push(&stream[split..]));
            assert_eq!(
                pages,
                vec![Bytes::from(first.clone()), Bytes::from(second.clone())]
            );
        }
    }

    #[test]
    fn a_reader_resynchronises_after_garbage() {
        let good = page(0, 9600, &[3; 10]);
        let mut stream = b"garbage without a capture pattern".to_vec();
        stream.extend_from_slice(&good);
        assert_eq!(
            OggPageReader::default().push(&stream),
            vec![Bytes::from(good)]
        );
    }

    #[test]
    fn a_late_listener_gets_the_header_pages_first_and_then_only_live_ones() {
        let hub = StreamHub::new();
        let ident = Bytes::from(page(OGG_BEGINNING_OF_STREAM, 0, b"ident"));
        let comment = Bytes::from(page(0, 0, b"comment"));
        let setup_part = Bytes::from(page(0, OGG_NO_GRANULE, b"setup, continued"));
        let audio_one = Bytes::from(page(0, 4800, b"audio one"));
        for page in [&ident, &comment, &setup_part, &audio_one] {
            hub.publish(page.clone());
        }

        let (headers, mut live) = hub.join();
        assert_eq!(headers, vec![ident, comment, setup_part]);

        let audio_two = Bytes::from(page(0, 9600, b"audio two"));
        hub.publish(audio_two.clone());
        assert_eq!(live.try_recv().expect("the next page"), audio_two);

        // ffmpeg restarting begins a new stream, whose headers replace the old ones.
        let new_ident = Bytes::from(page(OGG_BEGINNING_OF_STREAM, 0, b"new ident"));
        hub.publish(new_ident.clone());
        assert_eq!(hub.join().0, vec![new_ident]);
    }
}
