//! Continuous Icecast alert stream.
//!
//! This module is entirely separate from the external Icecast relay in
//! [`crate::relay`] (which streams a single finished recording, once, to a
//! user-supplied *external* Icecast server via `SHOULD_RELAY_ICECAST`).
//!
//! Here we keep a single, persistent source client connected to the container's
//! *bundled* Icecast so a mountpoint stays up 24/7. When no alert is playing we
//! feed a faint comfort-noise floor (not digital silence — see
//! [`COMFORT_NOISE_PEAK`]) so the encoder keeps a steady bitrate and players stay
//! at the live edge; when alerts arrive they are queued and streamed
//! back-to-back with a short gap between them.
//!
//! The engine is a long-lived `ffmpeg` process reading raw 48 kHz mono s16le PCM
//! from stdin and publishing Ogg/Vorbis to the mount. A pacing loop writes one
//! ~100 ms chunk per wall-clock tick, so the input (and therefore the output)
//! stays real-time. Alert recordings are decoded to matching PCM with a
//! short-lived `ffmpeg` invocation and streamed from memory.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use once_cell::sync::OnceCell;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::broadcast::{self, Receiver as BroadcastReceiver};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 1;
const CHUNK_MS: u64 = 100;
const CHUNK_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * CHUNK_MS as usize;
const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2;
const INTER_ALERT_GAP_BYTES: usize = (SAMPLE_RATE as usize) * 2;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);
const COMFORT_NOISE_PEAK: i16 = 32;
const NOISE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

static ALERT_STREAM_TX: OnceCell<mpsc::UnboundedSender<PathBuf>> = OnceCell::new();

pub fn enqueue_alert_audio(path: PathBuf) {
    if let Some(tx) = ALERT_STREAM_TX.get() {
        if let Err(err) = tx.send(path) {
            warn!("Failed to enqueue alert audio for Icecast stream: {}", err);
        }
    }
}

async fn decode_to_pcm(path: &Path) -> Result<Vec<u8>> {
    let output = Command::new("ffmpeg")
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
    for pair in dst.chunks_exact_mut(2) {
        let sample = (next_rand(state) % span) as i32 - COMFORT_NOISE_PEAK as i32;
        pair.copy_from_slice(&(sample as i16).to_le_bytes());
    }
}

fn comfort_noise_chunk(state: &mut u64) -> Vec<u8> {
    let mut out = vec![0u8; CHUNK_BYTES];
    write_comfort_noise(&mut out, state);
    out
}

fn spawn_encoder(config: &Config) -> Result<(Child, ChildStdin)> {
    let url = format!(
        "icecast://{}:{}@{}:{}{}",
        config.icecast_alert_source_user,
        config.icecast_alert_source_password,
        config.icecast_alert_host,
        config.icecast_alert_port,
        config.icecast_alert_mount,
    );

    let ice_name = if config.eas_relay_name.trim().is_empty() {
        "EAS Listener".to_string()
    } else {
        config.eas_relay_name.clone()
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-nostdin")
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
        .arg("-c:a")
        .arg("libvorbis")
        .arg("-b:a")
        .arg("128k")
        .arg("-content_type")
        .arg("application/ogg")
        .arg("-ice_name")
        .arg(&ice_name)
        .arg("-ice_description")
        .arg("Live EAS alert audio stream")
        .arg("-f")
        .arg("ogg")
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("Failed to spawn ffmpeg Icecast source process")?;
    let stdin = child
        .stdin
        .take()
        .context("ffmpeg Icecast source process had no stdin")?;
    Ok((child, stdin))
}

pub async fn run_alert_stream(
    mut config: Config,
    mut reload_rx: BroadcastReceiver<Config>,
) -> Result<()> {
    let (path_tx, mut path_rx) = mpsc::unbounded_channel::<PathBuf>();
    if ALERT_STREAM_TX.set(path_tx).is_err() {
        warn!("Icecast alert stream channel was already initialized; ignoring duplicate task.");
        return Ok(());
    }

    let mut interval = tokio::time::interval(Duration::from_millis(CHUNK_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut noise_state: u64 = NOISE_SEED;

    let mut encoder: Option<(Child, ChildStdin)> = None;
    let mut last_spawn_attempt: Option<Instant> = None;
    let mut spawn_failures: u64 = 0;

    let mut current: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    let mut gap_remaining = 0usize;
    let mut logged_disabled = false;

    loop {
        loop {
            match reload_rx.try_recv() {
                Ok(new_config) => {
                    let restart_needed = new_config.icecast_alert_stream_enabled
                        != config.icecast_alert_stream_enabled
                        || new_config.icecast_alert_host != config.icecast_alert_host
                        || new_config.icecast_alert_port != config.icecast_alert_port
                        || new_config.icecast_alert_mount != config.icecast_alert_mount
                        || new_config.icecast_alert_source_user != config.icecast_alert_source_user
                        || new_config.icecast_alert_source_password
                            != config.icecast_alert_source_password;
                    config = new_config;
                    if restart_needed {
                        encoder = None;
                        last_spawn_attempt = None;
                        spawn_failures = 0;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty)
                | Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }

        if !config.icecast_alert_stream_enabled {
            if encoder.take().is_some() {
                info!("Icecast alert stream disabled; tearing down source.");
            }
            if !logged_disabled {
                info!("Icecast alert stream is disabled; standing by.");
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

        if let Some((child, _)) = encoder.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                warn!("Icecast source process exited; will reconnect.");
                encoder = None;
            }
        }

        if encoder.is_none() {
            let ready = last_spawn_attempt
                .map(|attempted| attempted.elapsed() >= RECONNECT_BACKOFF)
                .unwrap_or(true);
            if ready {
                last_spawn_attempt = Some(Instant::now());
                match spawn_encoder(&config) {
                    Ok(source) => {
                        info!(
                            "Icecast alert source connected: {}:{}{}",
                            config.icecast_alert_host,
                            config.icecast_alert_port,
                            config.icecast_alert_mount
                        );
                        spawn_failures = 0;
                        encoder = Some(source);
                    }
                    Err(err) => {
                        spawn_failures += 1;
                        if spawn_failures == 1 || spawn_failures.is_multiple_of(12) {
                            warn!(
                                "Failed to start Icecast alert source (attempt {}): {}. Retrying every {}s. Is the built-in Icecast running (START_ICECAST=true)?",
                                spawn_failures,
                                err,
                                RECONNECT_BACKOFF.as_secs()
                            );
                        }
                    }
                }
            }
        }

        interval.tick().await;

        let Some((_, stdin)) = encoder.as_mut() else {
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
                        info!("Streaming alert audio to Icecast mount: {}", path.display());
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

        if let Err(err) = stdin.write_all(&chunk).await {
            warn!("Icecast source write failed: {}. Reconnecting.", err);
            encoder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comfort_noise_is_bounded_nonsilent_and_advances() {
        let mut state = NOISE_SEED;
        let chunk = comfort_noise_chunk(&mut state);
        assert_eq!(chunk.len(), CHUNK_BYTES);

        let mut any_nonzero = false;
        for pair in chunk.chunks_exact(2) {
            let sample = i16::from_le_bytes([pair[0], pair[1]]);
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
}
