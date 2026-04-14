use std::f64::consts::PI;

const MARK_FREQ: f64 = 2083.3;
const SPACE_FREQ: f64 = 1562.5;
const MIN_SAMPLE_RATE: u32 = 8000;
const BIT_DURATION_SEC: f64 = 0.00192;
const PREAMBLE_BYTE: u8 = 0xD5;
const BURST_COUNT: usize = 3;

#[derive(Debug)]
pub enum HeaderError {
    InvalidConfig(&'static str),
    Io(std::io::Error),
}

impl From<std::io::Error> for HeaderError {
    fn from(err: std::io::Error) -> Self {
        HeaderError::Io(err)
    }
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::InvalidConfig(msg) => f.write_str(msg),
            HeaderError::Io(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for HeaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HeaderError::Io(err) => Some(err),
            HeaderError::InvalidConfig(_) => None,
        }
    }
}

pub fn generate_same_header_samples(
    header: &str,
    sr: u32,
    amp: f64,
) -> Result<Vec<i16>, HeaderError> {
    validate_header(header)?;
    validate_amplitude(amp)?;

    let sr = sr.max(MIN_SAMPLE_RATE);

    let bits = build_same_bits(header);

    let mut samples_per_bit = (sr as f64 * BIT_DURATION_SEC).floor() as usize;
    if samples_per_bit < 1 {
        samples_per_bit = 1;
    }

    let mark = make_tone_cycle(MARK_FREQ, sr, samples_per_bit, amp);
    let space = make_tone_cycle(SPACE_FREQ, sr, samples_per_bit, amp);

    let silence = vec![0i16; sr as usize];
    let mut out: Vec<i16> = Vec::with_capacity(
        (bits.len() * samples_per_bit * BURST_COUNT) + (silence.len() * BURST_COUNT),
    );

    for _ in 0..BURST_COUNT {
        for &bit in &bits {
            if bit == 1 {
                out.extend_from_slice(&mark);
            } else {
                out.extend_from_slice(&space);
            }
        }
        out.extend_from_slice(&silence);
    }

    Ok(out)
}

fn validate_header(header: &str) -> Result<(), HeaderError> {
    if header.chars().count() == 4 && header == "NNNN" {
        return Ok(());
    }
    if !header.starts_with("ZCZC-") {
        return Err(HeaderError::InvalidConfig("Header must start with 'ZCZC-'"));
    }
    if !header.ends_with('-') {
        return Err(HeaderError::InvalidConfig("Header must end with '-'"));
    }
    if !header.is_ascii() {
        return Err(HeaderError::InvalidConfig("Header must be ASCII"));
    }
    Ok(())
}

fn validate_amplitude(amp: f64) -> Result<(), HeaderError> {
    if !amp.is_finite() {
        return Err(HeaderError::InvalidConfig(
            "Amplitude must be a finite number",
        ));
    }
    if !(0.0..=1.0).contains(&amp) {
        return Err(HeaderError::InvalidConfig(
            "Amplitude must be between 0.0 and 1.0",
        ));
    }
    Ok(())
}

fn byte_to_bits_msb_first(b: u8) -> [u8; 8] {
    let mut bits = [0u8; 8];
    for j in (0..8).rev() {
        bits[7 - j] = ((b >> j) & 1) as u8;
    }
    bits
}

fn byte_to_bits_lsb_first(b: u8) -> [u8; 8] {
    let mut bits = [0u8; 8];
    for i in 0..8 {
        bits[i] = ((b >> i) & 1) as u8;
    }
    bits
}

fn build_same_bits(header: &str) -> Vec<u8> {
    let mut bits = Vec::with_capacity((16 + header.len()) * 8);
    for _ in 0..16 {
        bits.extend_from_slice(&byte_to_bits_msb_first(PREAMBLE_BYTE));
    }
    for &b in header.as_bytes() {
        bits.extend_from_slice(&byte_to_bits_lsb_first(b));
    }
    bits
}

fn make_tone_cycle(freq: f64, sr: u32, samples_per_bit: usize, amp: f64) -> Vec<i16> {
    let sr_f = sr as f64;
    (0..samples_per_bit)
        .map(|i| {
            let t = i as f64 / sr_f;
            let s = (2.0 * PI * freq * t).sin() * amp;
            let v = (s * i16::MAX as f64).clamp(i16::MIN as f64, i16::MAX as f64);
            v as i16
        })
        .collect()
}

pub fn generate_attention_tone(sr: u32, amp: f64) -> Result<Vec<i16>, HeaderError> {
    validate_amplitude(amp)?;

    let sr = sr.max(MIN_SAMPLE_RATE);
    let duration_sec = 8.0;
    let total_samples = (sr as f64 * duration_sec).floor() as usize;

    let mut samples = Vec::with_capacity(total_samples);
    for i in 0..total_samples {
        let t = i as f64 / sr as f64;
        let s1 = (2.0 * PI * 853.0 * t).sin();
        let s2 = (2.0 * PI * 960.0 * t).sin();
        let s = (s1 + s2) * 0.5 * amp;
        let v = (s * i16::MAX as f64).clamp(i16::MIN as f64, i16::MAX as f64);
        samples.push(v as i16);
    }
    Ok(samples)
}

pub fn generate_silence_for_duration(sr: u32, duration_sec: f64) -> Vec<i16> {
    let sr = sr.max(MIN_SAMPLE_RATE);
    let total_samples = (sr as f64 * duration_sec).floor() as usize;
    vec![0i16; total_samples]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_same_header_samples_for_nnnn_is_sized_from_clamped_sample_rate() {
        let samples = generate_same_header_samples("NNNN", 2000, 0.5).expect("samples");
        let sr = MIN_SAMPLE_RATE as usize;
        let samples_per_bit = (MIN_SAMPLE_RATE as f64 * BIT_DURATION_SEC).floor() as usize;
        let bits_len = (16 + 4) * 8;
        let per_burst = (bits_len * samples_per_bit) + sr;
        let expected = per_burst * BURST_COUNT;
        assert_eq!(samples.len(), expected);
    }

    #[test]
    fn generate_same_header_samples_rejects_bad_input() {
        let err = generate_same_header_samples("BAD", 48_000, 0.5).expect_err("bad header");
        match err {
            HeaderError::InvalidConfig(msg) => assert!(msg.contains("start with 'ZCZC-'")),
            _ => panic!("unexpected error"),
        }

        let err = generate_attention_tone(48_000, 1.5).expect_err("bad amp");
        match err {
            HeaderError::InvalidConfig(msg) => assert!(msg.contains("between 0.0 and 1.0")),
            _ => panic!("unexpected error"),
        }
    }

    #[test]
    fn generate_attention_tone_and_silence_have_expected_lengths() {
        let tone = generate_attention_tone(4_000, 0.4).expect("tone");
        assert_eq!(tone.len(), MIN_SAMPLE_RATE as usize * 8);

        let silence = generate_silence_for_duration(4_000, 1.5);
        assert_eq!(
            silence.len(),
            MIN_SAMPLE_RATE as usize + MIN_SAMPLE_RATE as usize / 2
        );
        assert!(silence.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn generate_same_header_samples_for_standard_header_is_not_silent() {
        let header = "ZCZC-WXR-RWT-031055+0015-1231645-KWO35-";
        let samples = generate_same_header_samples(header, 48_000, 0.8).expect("samples");
        assert!(!samples.is_empty());
        assert!(samples.iter().any(|sample| *sample != 0));
    }
}
