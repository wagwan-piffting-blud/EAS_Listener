//! The archive view the dashboard reads: stored alerts and their recordings.
//!
//! This is the port of the old web_server/archive.php. The legacy branch that parsed
//! dedicated-alerts.log with regexes is gone -- db.rs migrates that file into SQLite on startup
//! and always opens a database, so the fallback was unreachable.

use crate::config::Config;
use crate::db::{AlertRow, DbHandle};
use anyhow::Result;
use chrono::DateTime;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tracing::warn;

const RECORDING_PREFIX: &str = "EAS_Recording_";
const RECORDING_EXTENSIONS: [&str; 3] = ["wav", "mp3", "ogg"];
/// Vacuumed recordings move here rather than being deleted.
const OLD_RECORDINGS_DIR: &str = "__old__";

#[derive(Debug, Clone, Serialize)]
pub struct ArchivedAlertData {
    pub event_code: String,
    pub event_text: String,
    pub originator: String,
    pub locations: String,
    pub alert_severity: String,
    pub length: Option<String>,
    pub raw_zczc: String,
    pub eas_text: String,
    /// Bare filename. The dashboard builds the authenticated /api/recordings URL from it, so
    /// this stays independent of whatever host the dashboard is reached on.
    pub recording_name: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub source_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchivedAlert {
    pub received_at: Option<i64>,
    pub expired_at: Option<i64>,
    pub data: ArchivedAlertData,
}

/// Recordings in the order the dashboard indexes them: oldest first, so a recording's position is
/// its id. Mirrors the old PHP manifest without persisting one -- a directory scan is cheap next
/// to the audio itself, and a stale manifest was its own source of bugs.
pub fn scan_recordings(recording_dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(recording_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(RECORDING_PREFIX) {
            continue;
        }

        let matches_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| RECORDING_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !matches_ext {
            continue;
        }

        let mtime = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        found.push((mtime, path));
    }

    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, path)| path).collect()
}

pub fn latest_recording_id(recording_dir: &Path) -> i64 {
    scan_recordings(recording_dir).len() as i64 - 1
}

pub fn resolve_recording_id(recording_dir: &Path, id: usize) -> Option<PathBuf> {
    scan_recordings(recording_dir).into_iter().nth(id)
}

/// Resolves by bare filename. Anything carrying a path separator is rejected rather than
/// normalised, so a name from the query string can never walk out of the recording directory.
pub fn resolve_recording_name(recording_dir: &Path, name: &str) -> Option<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty() || Path::new(trimmed).file_name().map(|n| n != trimmed) != Some(false) {
        return None;
    }

    scan_recordings(recording_dir)
        .into_iter()
        .find(|path| path.file_name().and_then(|n| n.to_str()) == Some(trimmed))
}

/// Whether a recording has finished being written. A file still being captured would otherwise be
/// served as a truncated, unplayable download.
pub fn is_finalized_recording(file: &Path) -> bool {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => is_finalized_mp3(file),
        Some("ogg") => is_finalized_ogg(file),
        _ => is_finalized_wav(file),
    }
}

fn read_at(file: &Path, offset: u64, len: usize) -> Option<Vec<u8>> {
    let mut handle = std::fs::File::open(file).ok()?;
    handle.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match handle.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buffer.truncate(filled);
    Some(buffer)
}

fn is_finalized_mp3(file: &Path) -> bool {
    let Some(head) = read_at(file, 0, 3) else {
        return false;
    };
    if head.len() < 3 {
        return false;
    }
    if &head[..3] == b"ID3" {
        return true;
    }
    head[0] == 0xFF && (head[1] & 0xE0) == 0xE0
}

fn is_finalized_ogg(file: &Path) -> bool {
    read_at(file, 0, 4)
        .map(|head| head == b"OggS")
        .unwrap_or(false)
}

/// A WAV is finished when its RIFF and data chunk sizes agree with the bytes actually on disk.
/// The recorder writes placeholder sizes while capturing and patches them on close.
fn is_finalized_wav(file: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(file) else {
        return false;
    };
    let filesize = metadata.len();
    if filesize < 44 {
        return false;
    }

    let Some(header) = read_at(file, 0, 12) else {
        return false;
    };
    if header.len() < 12 || &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return false;
    }

    let riff_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as u64;
    if riff_size + 8 != filesize {
        return false;
    }

    let mut offset: u64 = 12;
    while offset + 8 <= filesize {
        let Some(chunk) = read_at(file, offset, 8) else {
            return false;
        };
        if chunk.len() < 8 {
            return false;
        }

        let chunk_size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        if &chunk[0..4] == b"data" {
            return offset + 8 + chunk_size <= filesize && chunk_size > 0;
        }

        // Chunks are word-aligned, so an odd size is followed by a pad byte.
        offset += 8 + chunk_size + (chunk_size % 2);
    }

    false
}

/// Raw headers of alerts that are still live. They are shown on the dashboard's active list, so
/// the archive leaves them out to avoid listing the same alert twice.
pub fn active_raw_headers(shared_state_dir: &Path) -> HashSet<String> {
    let mut lookup = HashSet::new();
    let path = shared_state_dir.join("active_alerts.json");

    let Ok(payload) = std::fs::read_to_string(&path) else {
        return lookup;
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&payload) else {
        return lookup;
    };

    let now = chrono::Utc::now().timestamp();
    for entry in entries {
        let raw_header = entry
            .get("raw_header")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if raw_header.is_empty() {
            continue;
        }

        if let Some(expires_at) = entry.get("expires_at").and_then(Value::as_i64) {
            if expires_at <= now {
                continue;
            }
        }

        lookup.insert(raw_header.to_string());
    }

    lookup
}

fn hhmm_to_seconds(value: &str) -> Option<i64> {
    if value.len() != 4 || !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = value[0..2].parse().ok()?;
    let minutes: i64 = value[2..4].parse().ok()?;
    Some(hours * 3600 + minutes * 60)
}

fn parse_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp())
        .ok()
}

/// Severity stored on the row when the source provided one, otherwise inferred from the event
/// wording the way the dashboard's colour coding expects.
fn severity_for(row: &AlertRow) -> String {
    if let Some(severity) = row.severity.as_deref().map(str::trim) {
        if !severity.is_empty() {
            return severity.to_ascii_lowercase();
        }
    }

    let text = row.event_text.to_ascii_lowercase();
    for (needle, severity) in [
        ("emergency", "emergency"),
        ("warning", "warning"),
        ("watch", "watch"),
        ("advisory", "advisory"),
        ("test", "test"),
        ("demo", "test"),
        ("statement", "statement"),
    ] {
        if text.contains(needle) {
            return severity.to_string();
        }
    }

    "warning".to_string()
}

fn matches_watched_fips(row: &AlertRow, watched: &HashSet<String>) -> bool {
    let fips: Vec<String> = serde_json::from_str(&row.fips).unwrap_or_default();
    fips.iter().any(|fip| watched.contains(fip.trim()))
}

pub struct ArchiveQuery {
    pub limit: Option<usize>,
    pub filter_watched_fips: bool,
}

pub async fn archived_alerts(
    db: &DbHandle,
    config: &Config,
    query: &ArchiveQuery,
) -> Result<Vec<ArchivedAlert>> {
    let active = active_raw_headers(&config.shared_state_dir);

    // An empty watch list would filter everything away, so it disables the filter instead.
    let watched = config.watched_fips.clone();
    let filter_watched = query.filter_watched_fips && !watched.is_empty();

    let rows = db
        .fetch_alerts_where(query.limit, move |row| {
            if !row.raw_zczc.is_empty() && active.contains(&row.raw_zczc) {
                return false;
            }
            if filter_watched && !matches_watched_fips(row, &watched) {
                return false;
            }
            true
        })
        .await?;

    let mut alerts = Vec::with_capacity(rows.len());
    for row in rows {
        let received_at = parse_timestamp(&row.received_at);
        let duration = row
            .duration_hhmm
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());

        let expired_at = row
            .expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .or_else(|| match (received_at, duration.and_then(hhmm_to_seconds)) {
                (Some(received), Some(seconds)) => Some(received + seconds),
                _ => None,
            });

        let recording_name = row
            .recording_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);

        alerts.push(ArchivedAlert {
            received_at,
            expired_at,
            data: ArchivedAlertData {
                event_code: row.event_code.clone(),
                event_text: row.event_text.clone(),
                originator: row.originator_name.clone(),
                locations: row.locations.clone(),
                alert_severity: severity_for(&row),
                length: duration.map(str::to_string),
                raw_zczc: row.raw_zczc.clone(),
                eas_text: row.eas_text.clone(),
                recording_name,
                description: row.description.clone(),
                instructions: row.instructions.clone(),
                source_type: row.source_type.clone(),
            },
        });
    }

    Ok(alerts)
}

#[derive(Debug, Clone, Serialize)]
pub struct VacuumReport {
    pub alerts_deleted: usize,
    pub recordings_archived: usize,
    pub recordings_kept: usize,
    pub database_reclaimed: bool,
    pub log_backed_up: bool,
    pub log_entries_retained: usize,
    pub archive_dir: String,
}

/// Clears out everything the dashboard no longer needs: archived alert rows, their recordings, and
/// the legacy alert log.
///
/// Nothing is deleted outright. Recordings move to an `__old__` directory beside them and the log
/// is copied to a `.bak` before being rewritten, so a mistaken vacuum is recoverable by hand.
pub async fn vacuum(db: &DbHandle, config: &Config) -> Result<VacuumReport> {
    let recording_dir = &config.recording_dir;
    let old_dir = recording_dir.join(OLD_RECORDINGS_DIR);
    let active = active_raw_headers(&config.shared_state_dir);

    let recordings = scan_recordings(recording_dir);

    // A recording still being captured has no finished row yet, so keep it regardless.
    let mut keep: HashSet<String> = recordings
        .iter()
        .filter(|file| !is_finalized_recording(file))
        .filter_map(|file| {
            file.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .collect();

    let outcome = db
        .vacuum_alerts(active.iter().cloned().collect::<Vec<_>>())
        .await?;
    keep.extend(outcome.referenced_recordings.iter().cloned());

    let mut recordings_archived = 0;
    let mut recordings_kept = 0;
    for file in &recordings {
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if keep.contains(name) {
            recordings_kept += 1;
            continue;
        }

        if let Err(err) = std::fs::create_dir_all(&old_dir) {
            warn!(
                "Could not create {}: {}. Recordings were left in place.",
                old_dir.display(),
                err
            );
            break;
        }

        match std::fs::rename(file, old_dir.join(name)) {
            Ok(_) => recordings_archived += 1,
            Err(err) => warn!("Could not archive recording {}: {}", file.display(), err),
        }
    }

    let (log_backed_up, log_entries_retained) =
        vacuum_alert_log(&config.dedicated_alert_log_file, &active)?;

    Ok(VacuumReport {
        alerts_deleted: outcome.alerts_deleted,
        recordings_archived,
        recordings_kept,
        database_reclaimed: outcome.reclaimed,
        log_backed_up,
        log_entries_retained,
        archive_dir: old_dir.to_string_lossy().into_owned(),
    })
}

/// Rewrites the legacy alert log to just the entries that are still live, after appending the
/// current contents to a `.bak` alongside it.
fn vacuum_alert_log(log_path: &Path, active: &HashSet<String>) -> Result<(bool, usize)> {
    if log_path.as_os_str().is_empty() || !log_path.exists() {
        return Ok((false, 0));
    }

    let contents = match std::fs::read_to_string(log_path) {
        Ok(contents) => contents,
        Err(err) => {
            warn!(
                "Could not read alert log {}: {}. It was left untouched.",
                log_path.display(),
                err
            );
            return Ok((false, 0));
        }
    };

    let retained: Vec<&str> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            // Entries are "<raw header>: <text>"; anything without a live header is dropped.
            line.split_once(": ")
                .map(|(header, _)| active.contains(header.trim()))
                .unwrap_or(false)
        })
        .collect();

    let mut backup_path = log_path.as_os_str().to_os_string();
    backup_path.push(".bak");
    let backup_path = PathBuf::from(backup_path);

    let backed_up = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&backup_path)
    {
        Ok(mut backup) => match std::io::Write::write_all(&mut backup, contents.as_bytes()) {
            Ok(_) => true,
            Err(err) => {
                warn!("Could not append to {}: {}", backup_path.display(), err);
                false
            }
        },
        Err(err) => {
            warn!("Could not open {}: {}", backup_path.display(), err);
            false
        }
    };

    // Without a backup the original is the only copy, so it stays as it is.
    if !backed_up {
        return Ok((false, retained.len()));
    }

    let mut payload = retained.join("\n");
    if !payload.is_empty() {
        payload.push('\n');
    }
    if let Err(err) = std::fs::write(log_path, payload) {
        warn!("Could not rewrite {}: {}", log_path.display(), err);
        return Ok((true, retained.len()));
    }

    Ok((true, retained.len()))
}

pub fn content_type_for(file: &Path) -> &'static str {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        _ => "audio/wav",
    }
}

pub fn warn_unreadable(file: &Path, err: &std::io::Error) {
    warn!("Failed to read recording {}: {}", file.display(), err);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(bytes).expect("write");
        path
    }

    fn wav(data_len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, data_len));
        bytes
    }

    #[test]
    fn hhmm_parses_only_four_digit_values() {
        assert_eq!(hhmm_to_seconds("0130"), Some(5400));
        assert_eq!(hhmm_to_seconds("0015"), Some(900));
        assert_eq!(hhmm_to_seconds("015"), None);
        assert_eq!(hhmm_to_seconds("abcd"), None);
        assert_eq!(hhmm_to_seconds(""), None);
    }

    #[test]
    fn a_complete_wav_is_finalized_but_a_truncated_one_is_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let complete = write_file(dir.path(), "EAS_Recording_1.wav", &wav(128));
        assert!(is_finalized_recording(&complete));

        // The recorder leaves the declared sizes larger than the bytes present while capturing.
        let mut truncated = wav(128);
        truncated.truncate(truncated.len() - 32);
        let partial = write_file(dir.path(), "EAS_Recording_2.wav", &truncated);
        assert!(!is_finalized_recording(&partial));
    }

    #[test]
    fn mp3_and_ogg_are_recognised_by_their_magic_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(is_finalized_recording(&write_file(
            dir.path(),
            "EAS_Recording_1.mp3",
            b"ID3\x04\x00\x00\x00\x00\x00\x00"
        )));
        assert!(is_finalized_recording(&write_file(
            dir.path(),
            "EAS_Recording_2.mp3",
            &[0xFF, 0xFB, 0x90, 0x00]
        )));
        assert!(is_finalized_recording(&write_file(
            dir.path(),
            "EAS_Recording_3.ogg",
            b"OggS\x00\x02\x00\x00"
        )));
        assert!(!is_finalized_recording(&write_file(
            dir.path(),
            "EAS_Recording_4.ogg",
            b"nope"
        )));
    }

    #[test]
    fn recordings_are_indexed_oldest_first_and_ignore_unrelated_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_file(dir.path(), "EAS_Recording_a.wav", &wav(16));
        write_file(dir.path(), "EAS_Recording_b.mp3", b"ID3\x04\x00\x00");
        write_file(dir.path(), "notes.txt", b"ignore me");
        write_file(dir.path(), "other.wav", &wav(16));

        let found = scan_recordings(dir.path());
        assert_eq!(found.len(), 2);
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"EAS_Recording_a.wav"));
        assert!(names.contains(&"EAS_Recording_b.mp3"));
    }

    #[test]
    fn a_recording_name_cannot_escape_the_recording_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_file(dir.path(), "EAS_Recording_a.wav", &wav(16));

        assert!(resolve_recording_name(dir.path(), "EAS_Recording_a.wav").is_some());
        assert!(resolve_recording_name(dir.path(), "../EAS_Recording_a.wav").is_none());
        assert!(resolve_recording_name(dir.path(), "sub/EAS_Recording_a.wav").is_none());
        assert!(resolve_recording_name(dir.path(), "/etc/passwd").is_none());
        assert!(resolve_recording_name(dir.path(), "  ").is_none());
    }

    #[test]
    fn active_headers_exclude_entries_that_have_already_expired() {
        let dir = tempfile::tempdir().expect("temp dir");
        let future = chrono::Utc::now().timestamp() + 3600;
        let past = chrono::Utc::now().timestamp() - 3600;
        let payload = format!(
            r#"[{{"raw_header":"ZCZC-LIVE-","expires_at":{future}}},
                {{"raw_header":"ZCZC-GONE-","expires_at":{past}}},
                {{"raw_header":"  "}}]"#
        );
        write_file(dir.path(), "active_alerts.json", payload.as_bytes());

        let active = active_raw_headers(dir.path());
        assert!(active.contains("ZCZC-LIVE-"));
        assert!(!active.contains("ZCZC-GONE-"));
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn a_missing_active_alerts_file_yields_no_exclusions() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(active_raw_headers(dir.path()).is_empty());
    }
}
