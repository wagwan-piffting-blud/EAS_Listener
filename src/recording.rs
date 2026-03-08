use crate::config::Config;
use crate::header;
use anyhow::Result;
use chrono::Local;
use hound::{WavSpec, WavWriter};
use serde::Deserialize;
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::path::Path;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

const TARGET_SAMPLE_RATE: u32 = 48000;
const HEADER_AMPLITUDE: f64 = 0.42;
const SAME_MARK_FREQ_HZ: f32 = 2083.3;
const SAME_SPACE_FREQ_HZ: f32 = 1562.5;
const SAME_BIT_DURATION_SEC: f64 = 0.00192;
const SAME_PREAMBLE_BYTE: u8 = 0xD5;
const SAME_PREAMBLE_BYTES: usize = 16;
const NNNN_TAIL_BUFFER_SECONDS: usize = 10;
const NNNN_DETECT_SCAN_SECONDS: usize = 8;
const NNNN_OFFSET_STEP: usize = 2;
const NNNN_MIN_MATCH_BITS: usize = 128;
const NNNN_MIN_AVG_CONFIDENCE: f32 = 0.15;
const NNNN_MIN_FINAL_SCORE: f32 = 132.0;
const NNNN_TRIM_GUARD_MS: usize = 60;
const NNNN_ZERO_CROSS_LOOKBACK_MS: usize = 12;
const TAIL_FADE_OUT_MS: usize = 10;
const TRAILING_SILENCE_MIN_TRIM_MS: usize = 120;
const TRAILING_NEAR_SILENCE_WINDOW_MS: usize = 20;
const TRAILING_NEAR_SILENCE_HOP_MS: usize = 5;
const TRAILING_NEAR_SILENCE_FLOOR: i16 = 16;
const TRAILING_NEAR_SILENCE_PEAK_THRESHOLD: i16 = 1200;
const TRAILING_NEAR_SILENCE_RMS_THRESHOLD: f32 = 80.0;

#[derive(Debug, Clone)]
pub struct RecordingState {
    pub audio_tx: mpsc::Sender<Vec<f32>>,
    pub output_path: PathBuf,
    pub source_stream: String,
}

pub fn start_encoding_task(
    config: &Config,
    header_text: &str,
    source_stream: &str,
) -> Result<(tokio::task::JoinHandle<Result<()>>, RecordingState)> {
    start_encoding_task_with_timestamp(config, header_text, source_stream, None)
}

pub fn start_encoding_task_with_timestamp(
    config: &Config,
    header_text: &str,
    source_stream: &str,
    filename_timestamp: Option<&str>,
) -> Result<(tokio::task::JoinHandle<Result<()>>, RecordingState)> {
    std::fs::create_dir_all(&config.recording_dir)?;
    let timestamp = filename_timestamp
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d_%H-%M-%S").to_string());
    let event_code = event_code_from_header(header_text);
    let stream_label = stream_label_from_source(source_stream);
    let output_path = next_available_recording_path(
        &config.recording_dir,
        event_code.as_str(),
        &timestamp,
        stream_label.as_str(),
    );
    let output_path_clone = output_path.clone();

    let header_samples =
        header::generate_same_header_samples(header_text, TARGET_SAMPLE_RATE, HEADER_AMPLITUDE)?;
    let header_sample_count = header_samples.len();

    let nnnn_samples =
        header::generate_same_header_samples("NNNN", TARGET_SAMPLE_RATE, HEADER_AMPLITUDE)?;
    let nnnn_sample_count = nnnn_samples.len();
    let nnnn_burst_cycle_samples = nnnn_sample_count / 3;
    let nnnn_tail_buffer_samples = TARGET_SAMPLE_RATE as usize * NNNN_TAIL_BUFFER_SECONDS;

    let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>(32);

    let handle = tokio::spawn(async move {
        let spec = WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let writer = WavWriter::create(&output_path, spec)?;

        let samples_written = tokio::task::spawn_blocking(move || {
            let mut blocking_writer = writer;
            let mut audio_rx = audio_rx;
            for &sample in &header_samples {
                blocking_writer.write_sample(sample)?;
            }

            let mut samples_written = header_sample_count;
            let amplitude = i16::MAX as f32;
            let mut trailing_buffer: VecDeque<i16> =
                VecDeque::with_capacity(nnnn_tail_buffer_samples + 8192);
            while let Some(samples) = audio_rx.blocking_recv() {
                for sample in samples {
                    trailing_buffer.push_back((sample * amplitude) as i16);
                }

                let overflow = trailing_buffer
                    .len()
                    .saturating_sub(nnnn_tail_buffer_samples);
                for _ in 0..overflow {
                    if let Some(sample) = trailing_buffer.pop_front() {
                        blocking_writer.write_sample(sample)?;
                        samples_written += 1;
                    }
                }
            }

            let mut trailing_samples: Vec<i16> = trailing_buffer.into_iter().collect();
            if let Some(trim_from) =
                detect_trailing_nnnn_start(&trailing_samples, nnnn_burst_cycle_samples)
            {
                let guard_samples = (TARGET_SAMPLE_RATE as usize * NNNN_TRIM_GUARD_MS) / 1000;
                let zero_cross_lookback =
                    (TARGET_SAMPLE_RATE as usize * NNNN_ZERO_CROSS_LOOKBACK_MS) / 1000;
                let trim_from = trim_from.saturating_sub(guard_samples);
                let trim_from = snap_trim_to_zero_crossing(
                    &trailing_samples,
                    trim_from,
                    zero_cross_lookback,
                );
                trailing_samples.truncate(trim_from);
            }
            let min_silence_trim_samples =
                (TARGET_SAMPLE_RATE as usize * TRAILING_SILENCE_MIN_TRIM_MS) / 1000;
            let near_silence_window_samples =
                (TARGET_SAMPLE_RATE as usize * TRAILING_NEAR_SILENCE_WINDOW_MS) / 1000;
            let near_silence_hop_samples =
                (TARGET_SAMPLE_RATE as usize * TRAILING_NEAR_SILENCE_HOP_MS) / 1000;
            trim_trailing_near_silence(
                &mut trailing_samples,
                TRAILING_NEAR_SILENCE_FLOOR,
                TRAILING_NEAR_SILENCE_PEAK_THRESHOLD,
                TRAILING_NEAR_SILENCE_RMS_THRESHOLD,
                near_silence_window_samples,
                near_silence_hop_samples,
                min_silence_trim_samples,
            );
            let fade_out_samples = (TARGET_SAMPLE_RATE as usize * TAIL_FADE_OUT_MS) / 1000;
            apply_fade_out(&mut trailing_samples, fade_out_samples);
            let trailing_len = trailing_samples.len();
            for sample in trailing_samples {
                blocking_writer.write_sample(sample)?;
            }
            samples_written += trailing_len;

            let silence_samples_before_nnnn = TARGET_SAMPLE_RATE as usize;
            for _ in 0..silence_samples_before_nnnn {
                blocking_writer.write_sample(0i16)?;
            }
            samples_written += silence_samples_before_nnnn;

            for &sample in &nnnn_samples {
                blocking_writer.write_sample(sample)?;
            }

            samples_written += nnnn_sample_count;
            blocking_writer.finalize()?;
            Ok::<_, anyhow::Error>(samples_written)
        })
        .await??;

        if samples_written == 0 {
            let _ = tokio::fs::remove_file(&output_path).await;
            info!("Deleted empty recording file: {:?}", output_path);
        } else {
            info!("Finished writing recording to: {:?}", output_path);
        }

        Ok(())
    });

    let state = RecordingState {
        audio_tx,
        output_path: output_path_clone,
        source_stream: source_stream.to_string(),
    };
    Ok((handle, state))
}

fn detect_trailing_nnnn_start(samples: &[i16], nnnn_burst_cycle_samples: usize) -> Option<usize> {
    let samples_per_bit =
        ((TARGET_SAMPLE_RATE as f64 * SAME_BIT_DURATION_SEC).floor() as usize).max(1);
    let expected_bits = build_nnnn_expected_bits();
    let bits_per_burst = expected_bits.len();
    let burst_tone_samples = bits_per_burst * samples_per_bit;
    if samples.len() < burst_tone_samples {
        return None;
    }

    let search_window_samples = TARGET_SAMPLE_RATE as usize * NNNN_DETECT_SCAN_SECONDS;
    let search_start = samples.len().saturating_sub(search_window_samples);
    let search_samples = &samples[search_start..];
    if search_samples.len() < burst_tone_samples {
        return None;
    }

    let mark_coeff = goertzel_coeff(
        SAME_MARK_FREQ_HZ,
        TARGET_SAMPLE_RATE as f32,
        samples_per_bit,
    );
    let space_coeff = goertzel_coeff(
        SAME_SPACE_FREQ_HZ,
        TARGET_SAMPLE_RATE as f32,
        samples_per_bit,
    );

    let mut candidates: Vec<(usize, f32)> = Vec::new();
    for offset in (0..samples_per_bit).step_by(NNNN_OFFSET_STEP) {
        let available = search_samples.len().saturating_sub(offset);
        let bit_count = available / samples_per_bit;
        if bit_count < bits_per_burst {
            continue;
        }

        let mut decoded_bits = Vec::with_capacity(bit_count);
        let mut confidences = Vec::with_capacity(bit_count);
        let mut bit_start = offset;
        for _ in 0..bit_count {
            let mark_power =
                goertzel_power_window(search_samples, bit_start, samples_per_bit, mark_coeff);
            let space_power =
                goertzel_power_window(search_samples, bit_start, samples_per_bit, space_coeff);
            let total_power = (mark_power + space_power).max(1e-9);
            decoded_bits.push(u8::from(mark_power >= space_power));
            confidences.push((mark_power - space_power).abs() / total_power);
            bit_start += samples_per_bit;
        }

        for start_bit in 0..=(bit_count - bits_per_burst) {
            let mut matched_bits = 0usize;
            let mut confidence_sum = 0.0f32;
            for (i, expected_bit) in expected_bits.iter().enumerate() {
                let idx = start_bit + i;
                if decoded_bits[idx] == *expected_bit {
                    matched_bits += 1;
                }
                confidence_sum += confidences[idx];
            }

            let avg_confidence = confidence_sum / bits_per_burst as f32;
            if matched_bits >= NNNN_MIN_MATCH_BITS && avg_confidence >= NNNN_MIN_AVG_CONFIDENCE {
                let local_start = offset + (start_bit * samples_per_bit);
                let sample_start = search_start + local_start;
                let score = matched_bits as f32 + (avg_confidence * 40.0);
                candidates.push((sample_start, score));
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by_key(|(start, _)| *start);
    let cluster_window = samples_per_bit * 3;
    let mut clusters: Vec<(usize, f32)> = Vec::new();
    for (start, score) in candidates {
        if let Some((cluster_start, cluster_score)) = clusters.last_mut() {
            if start.saturating_sub(*cluster_start) <= cluster_window {
                if score > *cluster_score {
                    *cluster_score = score;
                }
                if start < *cluster_start {
                    *cluster_start = start;
                }
                continue;
            }
        }
        clusters.push((start, score));
    }

    let period_tolerance = samples_per_bit * 8;
    let mut best_choice: Option<(usize, f32)> = None;
    for (start, base_score) in &clusters {
        let mut score = *base_score;
        for multiplier in 1..=2 {
            let target = start.saturating_add(nnnn_burst_cycle_samples * multiplier);
            if clusters
                .iter()
                .any(|(other_start, _)| other_start.abs_diff(target) <= period_tolerance)
            {
                score += 100.0;
            }
        }

        match best_choice {
            Some((best_start, best_score))
                if score < best_score || (score == best_score && *start >= best_start) => {}
            _ => best_choice = Some((*start, score)),
        }
    }

    best_choice.and_then(|(start, score)| {
        if score >= NNNN_MIN_FINAL_SCORE {
            Some(start)
        } else {
            None
        }
    })
}

fn snap_trim_to_zero_crossing(samples: &[i16], trim_from: usize, max_lookback: usize) -> usize {
    if trim_from == 0 || trim_from >= samples.len() {
        return trim_from.min(samples.len());
    }

    let search_start = trim_from.saturating_sub(max_lookback).max(1);
    for idx in (search_start..trim_from).rev() {
        let prev = samples[idx - 1];
        let curr = samples[idx];
        if (prev <= 0 && curr >= 0) || (prev >= 0 && curr <= 0) {
            return idx;
        }
    }
    trim_from
}

fn apply_fade_out(samples: &mut [i16], fade_len: usize) {
    let len = samples.len();
    let fade_len = fade_len.min(len);
    if fade_len == 0 {
        return;
    }

    let fade_start = len - fade_len;
    for (i, sample) in samples[fade_start..].iter_mut().enumerate() {
        let gain = (fade_len - i) as f32 / fade_len as f32;
        *sample = (*sample as f32 * gain) as i16;
    }
}

fn trim_trailing_near_silence(
    samples: &mut Vec<i16>,
    floor: i16,
    peak_threshold: i16,
    rms_threshold: f32,
    window_len: usize,
    hop_len: usize,
    min_silence_samples: usize,
) {
    if samples.len() <= min_silence_samples {
        return;
    }

    let window_len = window_len.max(1).min(samples.len());
    let hop_len = hop_len.max(1);

    let mut cursor = samples.len();
    let mut trim_to = 0usize;
    while cursor >= window_len {
        let start = cursor - window_len;
        let window = &samples[start..cursor];

        let mut peak = 0i32;
        let mut sum_sq = 0.0f64;
        for &sample in window {
            let v = sample as i32;
            let abs_v = v.abs();
            if abs_v > peak {
                peak = abs_v;
            }
            sum_sq += (v * v) as f64;
        }

        let rms = (sum_sq / window.len() as f64).sqrt() as f32;
        let meaningful = peak >= peak_threshold as i32 || rms >= rms_threshold;
        if meaningful {
            trim_to = cursor;
            break;
        }

        if start < hop_len {
            break;
        }
        cursor -= hop_len;
    }

    if trim_to == 0 {
        trim_to = 0;
    } else {
        let floor = floor as i32;
        let mut refined = trim_to;
        while refined > 0 && (samples[refined - 1] as i32).abs() <= floor {
            refined -= 1;
        }
        trim_to = refined;
    }

    if samples.len().saturating_sub(trim_to) >= min_silence_samples {
        samples.truncate(trim_to);
    }
}

fn build_nnnn_expected_bits() -> Vec<u8> {
    let mut bits = Vec::with_capacity((SAME_PREAMBLE_BYTES + 4) * 8);
    for _ in 0..SAME_PREAMBLE_BYTES {
        bits.extend_from_slice(&byte_to_bits_msb_first(SAME_PREAMBLE_BYTE));
    }
    for &byte in b"NNNN" {
        bits.extend_from_slice(&byte_to_bits_lsb_first(byte));
    }
    bits
}

fn byte_to_bits_msb_first(byte: u8) -> [u8; 8] {
    let mut bits = [0u8; 8];
    for bit in 0..8 {
        bits[bit] = ((byte >> (7 - bit)) & 1) as u8;
    }
    bits
}

fn byte_to_bits_lsb_first(byte: u8) -> [u8; 8] {
    let mut bits = [0u8; 8];
    for bit in 0..8 {
        bits[bit] = ((byte >> bit) & 1) as u8;
    }
    bits
}

fn goertzel_coeff(freq_hz: f32, sample_rate_hz: f32, window_len: usize) -> f32 {
    let n = window_len as f32;
    let k = (0.5 + (n * freq_hz / sample_rate_hz)).floor();
    let omega = (2.0 * PI * k) / n;
    2.0 * omega.cos()
}

fn goertzel_power_window(samples: &[i16], start: usize, window_len: usize, coeff: f32) -> f32 {
    let mut s_prev = 0.0f32;
    let mut s_prev2 = 0.0f32;
    for &sample in &samples[start..start + window_len] {
        let s = sample as f32 + (coeff * s_prev) - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    (s_prev2 * s_prev2) + (s_prev * s_prev) - (coeff * s_prev * s_prev2)
}

fn next_available_recording_path(
    recording_dir: &Path,
    event_code: &str,
    timestamp: &str,
    stream_label: &str,
) -> PathBuf {
    let base = format!("EAS_Recording_{timestamp}_{event_code}_{stream_label}");
    let mut index = 0usize;
    loop {
        let filename = if index == 0 {
            format!("{base}.wav")
        } else {
            format!("{base}_{index}.wav")
        };
        let candidate = recording_dir.join(filename);
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn event_code_from_header(header_text: &str) -> String {
    let trimmed = header_text.trim();
    #[derive(Deserialize)]
    struct ParsedHeaderEventCode {
        event_code: String,
    }

    crate::e2t_ng::parse_header_json(trimmed)
        .ok()
        .and_then(|json| serde_json::from_str::<ParsedHeaderEventCode>(&json).ok())
        .map(|parsed| sanitize_filename_label(parsed.event_code.as_str()))
        .unwrap_or_else(|| "UNK".to_string())
}

fn stream_label_from_source(source_stream: &str) -> String {
    let without_query_or_fragment = source_stream
        .split(['?', '#'])
        .next()
        .unwrap_or(source_stream);

    let segment = without_query_or_fragment
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("UNKNOWN");

    let label = match segment.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => stem,
        _ => segment,
    };

    sanitize_filename_label(label)
}

fn sanitize_filename_label(label: &str) -> String {
    let mut output = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            output.push(c.to_ascii_uppercase());
        } else if matches!(c, '-' | '_') {
            output.push(c);
        } else {
            output.push('_');
        }
    }

    let trimmed = output.trim_matches('_');
    if trimmed.is_empty() {
        "UNKNOWN".to_string()
    } else {
        trimmed.to_string()
    }
}
