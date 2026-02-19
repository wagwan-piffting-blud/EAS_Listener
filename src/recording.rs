use crate::config::Config;
use crate::header;
use anyhow::Result;
use chrono::Local;
use hound::{WavSpec, WavWriter};
use std::path::Path;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

const TARGET_SAMPLE_RATE: u32 = 48000;
const HEADER_AMPLITUDE: f64 = 0.79;

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
    let output_path = next_available_recording_path(&config.recording_dir, &timestamp);
    let output_path_clone = output_path.clone();

    let header_samples =
        header::generate_same_header_samples(header_text, TARGET_SAMPLE_RATE, HEADER_AMPLITUDE)?;
    let header_sample_count = header_samples.len();

    let nnnn_samples =
        header::generate_same_header_samples("NNNN", TARGET_SAMPLE_RATE, HEADER_AMPLITUDE)?;
    let nnnn_sample_count = nnnn_samples.len();

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
            while let Some(samples) = audio_rx.blocking_recv() {
                for sample in samples {
                    blocking_writer.write_sample((sample * amplitude) as i16)?;
                    samples_written += 1;
                }
            }

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

fn next_available_recording_path(recording_dir: &Path, timestamp: &str) -> PathBuf {
    let base = format!("EAS_Recording_{timestamp}");
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
