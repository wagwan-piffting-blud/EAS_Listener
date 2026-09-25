//! 1050 Hz NOAA Weather Radio alert-tone detection.
//!
//! The stream is mixed down so 1050 Hz sits at DC and narrowed to about +-25 Hz, then that power is
//! compared with the whole 300-3000 Hz voice band. The tone counts as present while it carries at
//! least half of the in-band power, so the decision is independent of stream level and of hiss
//! above the voice band. `ToneSustain` then requires the tone for a minimum time, bridging short
//! dropouts instead of restarting on every missed chunk.

use std::f64::consts::{FRAC_1_SQRT_2, TAU};
use std::time::Duration;

const TONE_HALF_WIDTH_HZ: f64 = 25.0;
const BAND_LOW_HZ: f64 = 300.0;
const BAND_HIGH_HZ: f64 = 3000.0;
const POWER_TIME_CONSTANT_S: f64 = 0.1;
const MIN_BAND_POWER: f64 = 1e-6;
const MIN_TONE_FRACTION: f64 = 0.5;
// Pole Qs of a 4th-order Butterworth split into two biquads
const BUTTERWORTH4_Q: [f64; 2] = [0.541_196_100_146_197, 1.306_562_964_876_376_6];

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn from_rbj(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b0: b[0] / a[0],
            b1: b[1] / a[0],
            b2: b[2] / a[0],
            a1: a[1] / a[0],
            a2: a[2] / a[0],
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn lowpass(sample_rate_hz: f64, cutoff_hz: f64, q: f64) -> Self {
        let w0 = TAU * cutoff_hz / sample_rate_hz;
        let alpha = w0.sin() / (2.0 * q);
        let cos = w0.cos();
        Self::from_rbj(
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn highpass(sample_rate_hz: f64, cutoff_hz: f64, q: f64) -> Self {
        let w0 = TAU * cutoff_hz / sample_rate_hz;
        let alpha = w0.sin() / (2.0 * q);
        let cos = w0.cos();
        Self::from_rbj(
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub struct NwrToneDetector {
    phase: f64,
    phase_step: f64,
    tone_i: [Biquad; 2],
    tone_q: [Biquad; 2],
    band_high_pass: Biquad,
    band_low_pass: Biquad,
    alpha: f64,
    tone_power: f64,
    band_power: f64,
}

impl NwrToneDetector {
    pub fn new(sample_rate_hz: f64, tone_hz: f64) -> Self {
        let tone_filter =
            BUTTERWORTH4_Q.map(|q| Biquad::lowpass(sample_rate_hz, TONE_HALF_WIDTH_HZ, q));
        Self {
            phase: 0.0,
            phase_step: TAU * tone_hz / sample_rate_hz,
            tone_i: tone_filter,
            tone_q: tone_filter,
            band_high_pass: Biquad::highpass(sample_rate_hz, BAND_LOW_HZ, FRAC_1_SQRT_2),
            band_low_pass: Biquad::lowpass(sample_rate_hz, BAND_HIGH_HZ, FRAC_1_SQRT_2),
            alpha: 1.0 - (-1.0 / (POWER_TIME_CONSTANT_S * sample_rate_hz)).exp(),
            tone_power: 0.0,
            band_power: 0.0,
        }
    }

    /// Feeds a chunk and reports whether the tone dominates the voice band at its end.
    pub fn process(&mut self, samples: &[f32]) -> bool {
        for &sample in samples {
            let x = f64::from(sample);
            let (sin, cos) = self.phase.sin_cos();
            self.phase += self.phase_step;
            if self.phase >= TAU {
                self.phase -= TAU;
            }

            let mut i = x * cos;
            let mut q = -x * sin;
            for (fi, fq) in self.tone_i.iter_mut().zip(self.tone_q.iter_mut()) {
                i = fi.process(i);
                q = fq.process(q);
            }
            let band = self.band_low_pass.process(self.band_high_pass.process(x));

            // Mixing halves the tone's amplitude, so 2|z|^2 is its power.
            self.tone_power += self.alpha * (2.0 * (i * i + q * q) - self.tone_power);
            self.band_power += self.alpha * (band * band - self.band_power);
        }
        self.band_power >= MIN_BAND_POWER && self.tone_power >= MIN_TONE_FRACTION * self.band_power
    }
}

pub struct ToneSustain {
    required_samples: usize,
    max_gap_samples: usize,
    tone_samples: usize,
    gap_samples: usize,
}

impl ToneSustain {
    pub fn new(sample_rate_hz: u32, required: Duration, max_gap: Duration) -> Self {
        let to_samples = |d: Duration| (d.as_secs_f64() * f64::from(sample_rate_hz)) as usize;
        Self {
            required_samples: to_samples(required),
            max_gap_samples: to_samples(max_gap),
            tone_samples: 0,
            gap_samples: 0,
        }
    }

    pub fn update(&mut self, tone_present: bool, chunk_len: usize) {
        if tone_present {
            self.tone_samples = self.tone_samples.saturating_add(chunk_len);
            self.gap_samples = 0;
        } else {
            self.gap_samples = self.gap_samples.saturating_add(chunk_len);
            if self.gap_samples > self.max_gap_samples {
                self.reset();
            }
        }
    }

    pub fn is_satisfied(&self) -> bool {
        self.tone_samples >= self.required_samples
    }

    pub fn reset(&mut self) {
        self.tone_samples = 0;
        self.gap_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const CHUNK: usize = 2048;

    struct Lcg(u64);

    impl Lcg {
        fn uniform(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn gaussian(&mut self) -> f64 {
            (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
        }
    }

    fn tone(freq_hz: f64, amplitude: f64, seconds: f64) -> Vec<f64> {
        let n = (seconds * f64::from(RATE)) as usize;
        (0..n)
            .map(|i| amplitude * (TAU * freq_hz * i as f64 / f64::from(RATE)).sin())
            .collect()
    }

    fn mix(signals: &[Vec<f64>]) -> Vec<f64> {
        let len = signals.iter().map(Vec::len).max().unwrap_or(0);
        (0..len)
            .map(|i| {
                signals
                    .iter()
                    .map(|s| s.get(i).copied().unwrap_or(0.0))
                    .sum()
            })
            .collect()
    }

    // White noise whose power in a 3 kHz bandwidth is `snr_db` below a sine of `amplitude`
    fn white_noise(amplitude: f64, snr_db: f64, seconds: f64, seed: u64) -> Vec<f64> {
        let mut rng = Lcg(seed);
        let power = amplitude * amplitude / 2.0 / 10f64.powf(snr_db / 10.0) * f64::from(RATE)
            / 2.0
            / 3000.0;
        let sigma = power.sqrt();
        let n = (seconds * f64::from(RATE)) as usize;
        (0..n).map(|_| sigma * rng.gaussian()).collect()
    }

    // First difference twice: a PSD rising with f^2 like FM discriminator noise
    fn fm_hiss(rms: f64, seconds: f64, seed: u64) -> Vec<f64> {
        let mut rng = Lcg(seed);
        let n = (seconds * f64::from(RATE)) as usize;
        let white: Vec<f64> = (0..n + 2).map(|_| rng.gaussian()).collect();
        let shaped: Vec<f64> = (0..n)
            .map(|i| white[i + 2] - 2.0 * white[i + 1] + white[i])
            .collect();
        let actual = (shaped.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
        shaped.iter().map(|v| v * rms / actual).collect()
    }

    fn sustained_seconds(signal: &[f64]) -> f64 {
        let mut detector = NwrToneDetector::new(f64::from(RATE), 1050.0);
        let mut sustain =
            ToneSustain::new(RATE, Duration::from_secs(3600), Duration::from_millis(500));
        let mut longest = 0usize;
        for chunk in signal.chunks(CHUNK) {
            let samples: Vec<f32> = chunk.iter().map(|&v| v as f32).collect();
            sustain.update(detector.process(&samples), samples.len());
            longest = longest.max(sustain.tone_samples);
        }
        longest as f64 / f64::from(RATE)
    }

    #[test]
    fn detects_clean_tone_at_any_level() {
        for amplitude in [0.9, 0.1, 0.01, 0.003] {
            let held = sustained_seconds(&tone(1050.0, amplitude, 8.0));
            assert!(held > 7.5, "amplitude {amplitude}: held {held:.2} s");
        }
    }

    #[test]
    fn tolerates_frequency_error() {
        for freq in [1030.0, 1050.0, 1070.0] {
            let held = sustained_seconds(&tone(freq, 0.3, 8.0));
            assert!(held > 7.5, "{freq} Hz: held {held:.2} s");
        }
    }

    #[test]
    fn rejects_neighbouring_tones() {
        for freq in [853.0, 960.0, 1000.0, 1100.0, 1562.5, 2083.3] {
            let held = sustained_seconds(&tone(freq, 0.5, 8.0));
            assert!(held < 0.1, "{freq} Hz: held {held:.2} s");
        }
    }

    #[test]
    fn rejects_eas_attention_signal() {
        let attention = mix(&[tone(853.0, 0.35, 8.0), tone(960.0, 0.35, 8.0)]);
        assert!(sustained_seconds(&attention) < 0.1);
    }

    #[test]
    fn rejects_noise_and_voice_like_harmonics() {
        assert!(sustained_seconds(&white_noise(0.5, 0.0, 8.0, 7)) < 0.1);
        // 150 Hz fundamental with 1/n harmonics puts its 7th harmonic on 1050 Hz
        let harmonics: Vec<Vec<f64>> = (1..=20)
            .map(|n| tone(150.0 * f64::from(n), 0.3 / f64::from(n), 8.0))
            .collect();
        assert!(sustained_seconds(&mix(&harmonics)) < 0.1);
    }

    #[test]
    fn detects_tone_in_voice_band_noise() {
        let signal = mix(&[tone(1050.0, 0.3, 8.0), white_noise(0.3, 3.0, 8.0, 11)]);
        let held = sustained_seconds(&signal);
        assert!(held > 7.0, "held {held:.2} s");
    }

    #[test]
    fn ignores_hiss_above_the_voice_band() {
        let signal = mix(&[tone(1050.0, 0.05, 8.0), fm_hiss(0.2, 8.0, 3)]);
        let held = sustained_seconds(&signal);
        assert!(held > 7.0, "held {held:.2} s");
    }

    #[test]
    fn bridges_short_dropouts() {
        let mut signal = tone(1050.0, 0.3, 8.0);
        for start in (RATE as usize..signal.len()).step_by(2 * RATE as usize) {
            let end = (start + RATE as usize / 5).min(signal.len());
            signal[start..end].iter_mut().for_each(|v| *v = 0.0);
        }
        let held = sustained_seconds(&signal);
        assert!(held > 6.5, "held {held:.2} s");
    }

    #[test]
    fn sustain_resets_after_long_gap() {
        let mut sustain =
            ToneSustain::new(RATE, Duration::from_secs(5), Duration::from_millis(500));
        sustain.update(true, 4 * RATE as usize);
        sustain.update(false, RATE as usize);
        sustain.update(true, 2 * RATE as usize);
        assert!(!sustain.is_satisfied());
        sustain.update(true, 3 * RATE as usize);
        assert!(sustain.is_satisfied());
    }
}
