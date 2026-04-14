use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS alerts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    raw_zczc        TEXT    NOT NULL,
    eas_text        TEXT    NOT NULL,
    event_code      TEXT    NOT NULL,
    event_text      TEXT    NOT NULL,
    originator_code TEXT    NOT NULL DEFAULT '',
    originator_name TEXT    NOT NULL DEFAULT '',
    fips            TEXT    NOT NULL DEFAULT '',
    locations       TEXT    NOT NULL DEFAULT '',
    description     TEXT,
    recording_name  TEXT,
    source_stream   TEXT,
    source_type     TEXT    NOT NULL DEFAULT 'same',
    urgency         TEXT,
    severity        TEXT,
    certainty       TEXT,
    instructions    TEXT,
    cap_identifier  TEXT,
    cap_sender      TEXT,
    duration_hhmm   TEXT,
    received_at     TEXT    NOT NULL,
    expires_at      TEXT,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_alerts_received_at ON alerts(received_at);
CREATE INDEX IF NOT EXISTS idx_alerts_event_code  ON alerts(event_code);
CREATE INDEX IF NOT EXISTS idx_alerts_raw_zczc    ON alerts(raw_zczc);
"#;

#[derive(Clone)]
pub struct DbHandle {
    conn: Arc<std::sync::Mutex<Connection>>,
}

impl DbHandle {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open alert database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("Failed to set WAL journal mode")?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")
            .context("Failed to set busy timeout")?;
        conn.execute_batch(SCHEMA_SQL)
            .context("Failed to initialize database schema")?;

        info!("Alert database opened at {}", path.display());

        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    pub async fn insert_same_alert(
        &self,
        raw_zczc: &str,
        eas_text: &str,
        event_code: &str,
        event_text: &str,
        originator_code: &str,
        originator_name: &str,
        fips: &[String],
        locations: &str,
        source_stream: Option<&str>,
        duration_hhmm: Option<&str>,
        received_at: &str,
        expires_at: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let raw_zczc = raw_zczc.to_string();
        let eas_text = eas_text.to_string();
        let event_code = event_code.to_string();
        let event_text = event_text.to_string();
        let originator_code = originator_code.to_string();
        let originator_name = originator_name.to_string();
        let fips_json = serde_json::to_string(fips).unwrap_or_else(|_| "[]".to_string());
        let locations = locations.to_string();
        let source_stream = source_stream.map(|s| s.to_string());
        let duration_hhmm = duration_hhmm.map(|s| s.to_string());
        let received_at = received_at.to_string();
        let expires_at = expires_at.map(|s| s.to_string());

        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;
            guard.execute(
                "INSERT INTO alerts (raw_zczc, eas_text, event_code, event_text, originator_code, originator_name, fips, locations, source_stream, source_type, duration_hhmm, received_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'same', ?10, ?11, ?12)",
                params![
                    raw_zczc,
                    eas_text,
                    event_code,
                    event_text,
                    originator_code,
                    originator_name,
                    fips_json,
                    locations,
                    source_stream,
                    duration_hhmm,
                    received_at,
                    expires_at,
                ],
            )?;
            Ok(guard.last_insert_rowid())
        })
        .await
        .context("DB insert task panicked")?
    }

    pub async fn insert_cap_alert(
        &self,
        raw_zczc: &str,
        eas_text: &str,
        event_code: &str,
        event_text: &str,
        originator_code: &str,
        originator_name: &str,
        fips: &[String],
        locations: &str,
        description: Option<&str>,
        source_stream: &str,
        urgency: Option<&str>,
        severity: Option<&str>,
        certainty: Option<&str>,
        instructions: Option<&str>,
        cap_identifier: &str,
        cap_sender: &str,
        duration_hhmm: Option<&str>,
        received_at: &str,
        expires_at: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let raw_zczc = raw_zczc.to_string();
        let eas_text = eas_text.to_string();
        let event_code = event_code.to_string();
        let event_text = event_text.to_string();
        let originator_code = originator_code.to_string();
        let originator_name = originator_name.to_string();
        let fips_json = serde_json::to_string(fips).unwrap_or_else(|_| "[]".to_string());
        let locations = locations.to_string();
        let description = description.map(|s| s.to_string());
        let source_stream = source_stream.to_string();
        let urgency = urgency.map(|s| s.to_string());
        let severity = severity.map(|s| s.to_string());
        let certainty = certainty.map(|s| s.to_string());
        let instructions = instructions.map(|s| s.to_string());
        let cap_identifier = cap_identifier.to_string();
        let cap_sender = cap_sender.to_string();
        let duration_hhmm = duration_hhmm.map(|s| s.to_string());
        let received_at = received_at.to_string();
        let expires_at = expires_at.map(|s| s.to_string());

        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;
            guard.execute(
                "INSERT INTO alerts (raw_zczc, eas_text, event_code, event_text, originator_code, originator_name, fips, locations, description, source_stream, source_type, urgency, severity, certainty, instructions, cap_identifier, cap_sender, duration_hhmm, received_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'cap', ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    raw_zczc,
                    eas_text,
                    event_code,
                    event_text,
                    originator_code,
                    originator_name,
                    fips_json,
                    locations,
                    description,
                    source_stream,
                    urgency,
                    severity,
                    certainty,
                    instructions,
                    cap_identifier,
                    cap_sender,
                    duration_hhmm,
                    received_at,
                    expires_at,
                ],
            )?;
            Ok(guard.last_insert_rowid())
        })
        .await
        .context("DB insert task panicked")?
    }

    pub async fn update_recording_name(&self, raw_zczc: &str, recording_name: &str) {
        let conn = self.conn.clone();
        let raw_zczc_owned = raw_zczc.to_string();
        let recording_name = recording_name.to_string();

        let raw_zczc_for_log = raw_zczc_owned.clone();
        let result = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;
            let updated = guard.execute(
                "UPDATE alerts SET recording_name = ?1 WHERE id = (SELECT id FROM alerts WHERE raw_zczc = ?2 ORDER BY id DESC LIMIT 1)",
                params![recording_name, raw_zczc_owned],
            )?;
            Ok::<usize, anyhow::Error>(updated)
        })
        .await;

        match result {
            Ok(Ok(count)) => {
                if count == 0 {
                    warn!(
                        "No alert row found to update recording_name for raw_zczc: {}",
                        raw_zczc_for_log
                    );
                }
            }
            Ok(Err(err)) => warn!("Failed to update recording_name in DB: {}", err),
            Err(err) => warn!("Recording name update task panicked: {}", err),
        }
    }

    pub fn migrate_legacy_log(
        &self,
        legacy_log_path: &Path,
        recording_dir: &Path,
    ) -> Result<usize> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;

        let row_count: i64 =
            guard.query_row("SELECT COUNT(*) FROM alerts", [], |row| row.get(0))?;
        if row_count > 0 {
            return Ok(0);
        }

        if !legacy_log_path.exists() {
            return Ok(0);
        }

        let raw_payload = std::fs::read_to_string(legacy_log_path).with_context(|| {
            format!(
                "Failed to read legacy alert log: {}",
                legacy_log_path.display()
            )
        })?;
        let raw_payload = raw_payload.trim();
        if raw_payload.is_empty() {
            return Ok(0);
        }

        let re_header = Regex::new(
            r"(?m)^(ZCZC-[A-Z]{3}-[A-Z]{3}-(?:\d{6}(?:-?)){1,31}\+\d{4}-\d{7}-[A-Za-z0-9/ ]{1,8}?-)",
        )
        .unwrap();
        let re_received = Regex::new(r"\(Received @ (.*?)\)").unwrap();
        let re_duration = Regex::new(r"\+(\d{4})-").unwrap();
        let re_loc = Regex::new(r"for (.*?); beginning").unwrap();

        let header_starts: Vec<usize> = re_header
            .find_iter(raw_payload)
            .map(|m| m.start())
            .collect();
        if header_starts.is_empty() {
            return Ok(0);
        }

        let mut entries: Vec<&str> = Vec::with_capacity(header_starts.len());
        for (i, &start) in header_starts.iter().enumerate() {
            let end = header_starts
                .get(i + 1)
                .copied()
                .unwrap_or(raw_payload.len());
            let entry = raw_payload[start..end].trim();
            if !entry.is_empty() {
                entries.push(entry);
            }
        }

        let recording_lookup = build_recording_lookup(recording_dir);

        info!(
            "Migrating {} legacy alert log entries into database ({} recording files found)...",
            entries.len(),
            recording_lookup.len()
        );

        let tx = guard.unchecked_transaction()?;
        let mut imported = 0usize;
        let mut recordings_matched = 0usize;

        for entry in &entries {
            let Some((raw_zczc, _rest)) = entry.split_once(": ") else {
                continue;
            };
            let raw_zczc = raw_zczc.trim();

            let received_ndt = re_received.captures(entry).and_then(|caps| {
                let ts_str = caps.get(1)?.as_str();
                chrono::NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %l:%M:%S %p").ok()
            });

            let received_at_iso = received_ndt
                .map(|ndt| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                })
                .unwrap_or_else(|| {
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                });

            let duration_hhmm: Option<String> = re_duration
                .captures(raw_zczc)
                .map(|caps| caps.get(1).unwrap().as_str().to_string());

            let parsed = crate::e2t_ng::parse_header_json(raw_zczc)
                .ok()
                .and_then(|json| {
                    serde_json::from_str::<crate::e2t_ng::ParsedEasSerialized>(&json).ok()
                });

            let (event_code, originator_code, fips_json, event_text, originator_name) =
                if let Some(ref p) = parsed {
                    let fips =
                        serde_json::to_string(&p.fips_codes).unwrap_or_else(|_| "[]".to_string());
                    let event_title = crate::webhook::determine_event_title(&p.event_code);
                    let org_name = crate::webhook::determine_originator_name(&p.originator);
                    (
                        p.event_code.clone(),
                        p.originator.clone(),
                        fips,
                        event_title,
                        org_name,
                    )
                } else {
                    let ec = raw_zczc
                        .strip_prefix("ZCZC-")
                        .and_then(|s| s.get(4..7))
                        .unwrap_or("")
                        .to_string();
                    (
                        ec,
                        String::new(),
                        "[]".to_string(),
                        String::new(),
                        String::new(),
                    )
                };

            let recording_name = received_ndt.and_then(|ndt| {
                let key = format!("{}_{}", ndt.format("%Y-%m-%d_%H-%M-%S"), event_code);
                recording_lookup.get(&key).cloned()
            });
            if recording_name.is_some() {
                recordings_matched += 1;
            }

            let eas_text = entry
                .find("-: ")
                .and_then(|start| {
                    let after = &entry[start + 3..];
                    after
                        .rfind(" (Received")
                        .map(|end| after[..end].to_string())
                })
                .unwrap_or_default();

            let locations = re_loc
                .captures(&eas_text)
                .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
                .unwrap_or_default();

            guard.execute(
                "INSERT INTO alerts (raw_zczc, eas_text, event_code, event_text, originator_code, originator_name, fips, locations, recording_name, duration_hhmm, received_at, source_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'same')",
                params![
                    raw_zczc,
                    eas_text,
                    event_code,
                    event_text,
                    originator_code,
                    originator_name,
                    fips_json,
                    locations,
                    recording_name,
                    duration_hhmm,
                    received_at_iso,
                ],
            )?;
            imported += 1;
        }

        tx.commit()?;

        info!(
            "Legacy alert log migration complete: {} entries imported, {} recordings matched.",
            imported, recordings_matched
        );
        Ok(imported)
    }
}

fn build_recording_lookup(dir: &Path) -> HashMap<String, String> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return HashMap::new(),
    };
    let mut lookup = HashMap::new();

    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("EAS_Recording_") || !name_str.ends_with(".wav") {
            continue;
        }

        let stem = &name_str["EAS_Recording_".len()..name_str.len() - ".wav".len()];
        if stem.len() < 23 {
            continue;
        }
        let timestamp = &stem[..19];
        let after_ts = &stem[20..];
        let event_code = after_ts.split('_').next().unwrap_or("");
        if event_code.is_empty() {
            continue;
        }

        let key = format!("{}_{}", timestamp, event_code);
        lookup.entry(key).or_insert_with(|| name_str.into_owned());
    }

    lookup
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (DbHandle, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_alerts.db");
        let handle = DbHandle::open(&db_path).unwrap();
        (handle, dir)
    }

    #[test]
    fn test_open_creates_database() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        assert!(!db_path.exists());
        let _handle = DbHandle::open(&db_path).unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn test_wal_mode_enabled() {
        let (handle, _dir) = test_db();
        let conn = handle.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn test_schema_tables_exist() {
        let (handle, _dir) = test_db();
        let conn = handle.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='alerts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_insert_same_alert() {
        let (handle, _dir) = test_db();
        let id = handle
            .insert_same_alert(
                "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-",
                "The National Weather Service has issued a Tornado Warning.",
                "TOR",
                "Tornado Warning",
                "WXR",
                "National Weather Service",
                &["031055".to_string()],
                "Douglas County",
                Some("http://stream.example.com"),
                Some("0030"),
                "2024-12-04T17:58:45Z",
                Some("2024-12-04T18:28:45Z"),
            )
            .await
            .unwrap();

        assert!(id > 0);

        let conn = handle.conn.lock().unwrap();
        let (raw, eas, src_type): (String, String, String) = conn
            .query_row(
                "SELECT raw_zczc, eas_text, source_type FROM alerts WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(raw, "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-");
        assert!(eas.contains("Tornado Warning"));
        assert_eq!(src_type, "same");
    }

    #[tokio::test]
    async fn test_insert_cap_alert() {
        let (handle, _dir) = test_db();
        let id = handle
            .insert_cap_alert(
                "ZCZC-CIV-NUT-031055+0100-1231645-IPAWSCAP-",
                "A National Terrorism Advisory System alert.",
                "NUT",
                "National Terrorism Advisory",
                "CIV",
                "Department of Homeland Security",
                &["031055".to_string(), "031153".to_string()],
                "Douglas County, Sarpy County",
                Some("This is a test CAP description."),
                "https://cap.example.com/feed",
                Some("Immediate"),
                Some("Extreme"),
                Some("Observed"),
                Some("Take shelter immediately."),
                "CAP-ID-12345",
                "cap-sender@example.com",
                Some("0100"),
                "2024-12-04T17:58:45Z",
                Some("2024-12-04T18:58:45Z"),
            )
            .await
            .unwrap();

        assert!(id > 0);

        let conn = handle.conn.lock().unwrap();
        let (src_type, cap_id, sev): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT source_type, cap_identifier, severity FROM alerts WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(src_type, "cap");
        assert_eq!(cap_id.as_deref(), Some("CAP-ID-12345"));
        assert_eq!(sev.as_deref(), Some("Extreme"));
    }

    #[tokio::test]
    async fn test_update_recording_name() {
        let (handle, _dir) = test_db();
        handle
            .insert_same_alert(
                "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-",
                "Tornado Warning text.",
                "TOR",
                "Tornado Warning",
                "WXR",
                "NWS",
                &["031055".to_string()],
                "Douglas County",
                None,
                Some("0030"),
                "2024-12-04T17:58:45Z",
                None,
            )
            .await
            .unwrap();

        handle
            .update_recording_name(
                "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-",
                "EAS_Recording_TOR_20241204_175845.wav",
            )
            .await;

        let conn = handle.conn.lock().unwrap();
        let name: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE raw_zczc = ?1",
                params!["ZCZC-WXR-TOR-031055+0030-1231645-KWO35-"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            name.as_deref(),
            Some("EAS_Recording_TOR_20241204_175845.wav")
        );
    }

    #[tokio::test]
    async fn test_update_recording_name_targets_latest() {
        let (handle, _dir) = test_db();
        let header = "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-";

        handle
            .insert_same_alert(
                header,
                "First alert.",
                "TOR",
                "Tornado Warning",
                "WXR",
                "NWS",
                &["031055".to_string()],
                "Douglas County",
                None,
                Some("0030"),
                "2024-12-04T17:00:00Z",
                None,
            )
            .await
            .unwrap();

        let second_id = handle
            .insert_same_alert(
                header,
                "Second alert.",
                "TOR",
                "Tornado Warning",
                "WXR",
                "NWS",
                &["031055".to_string()],
                "Douglas County",
                None,
                Some("0030"),
                "2024-12-04T18:00:00Z",
                None,
            )
            .await
            .unwrap();

        handle
            .update_recording_name(header, "EAS_Recording_latest.wav")
            .await;

        let conn = handle.conn.lock().unwrap();
        let name: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE id = ?1",
                params![second_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name.as_deref(), Some("EAS_Recording_latest.wav"));

        let first_name: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE id = ?1",
                params![second_id - 1],
                |row| row.get(0),
            )
            .unwrap();
        assert!(first_name.is_none());
    }

    #[test]
    fn test_migrate_legacy_log_imports_entries() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let log_path = dir.path().join("dedicated-alerts.log");
        let rec_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&rec_dir).unwrap();

        std::fs::write(
            &log_path,
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-: The National Weather Service has issued a Tornado Warning for Douglas County; beginning at 11:45 AM and ending at 12:15 PM. (Received @ 2024-12-04 11:58:45 AM)\n\n\
             ZCZC-WXR-SVR-031055+0100-1231700-KWO35-: The National Weather Service has issued a Severe Thunderstorm Warning for Douglas County; beginning at 12:00 PM and ending at 1:00 PM. (Received @ 2024-12-04 12:00:00 PM)\n\n",
        )
        .unwrap();

        let handle = DbHandle::open(&db_path).unwrap();
        let imported = handle.migrate_legacy_log(&log_path, &rec_dir).unwrap();
        assert_eq!(imported, 2);

        let conn = handle.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM alerts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let (raw, event_code, duration, locations): (String, String, Option<String>, String) = conn
            .query_row(
                "SELECT raw_zczc, event_code, duration_hhmm, locations FROM alerts ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(raw, "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-");
        assert_eq!(event_code, "TOR");
        assert_eq!(duration.as_deref(), Some("0030"));
        assert_eq!(locations, "Douglas County");
    }

    #[test]
    fn test_migrate_legacy_log_matches_recordings_by_timestamp_and_event() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let log_path = dir.path().join("dedicated-alerts.log");
        let rec_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&rec_dir).unwrap();

        std::fs::write(
            &log_path,
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-: Tornado Warning for Douglas County; beginning at 11:45 AM. (Received @ 2024-12-04 11:58:45 AM)\n\n\
             ZCZC-WXR-SVR-031055+0100-1231700-KWO35-: Severe Thunderstorm Warning for Douglas County; beginning at 12:00 PM. (Received @ 2024-12-04 12:00:00 PM)\n\n\
             ZCZC-WXR-FFW-031055+0100-1231800-KWO35-: Flash Flood Warning for Douglas County; beginning at 1:00 PM. (Received @ 2024-12-04  1:00:00 PM)\n\n",
        )
        .unwrap();

        let rec_tor = rec_dir.join("EAS_Recording_2024-12-04_11-58-45_TOR_stream1.wav");
        let rec_svr = rec_dir.join("EAS_Recording_2024-12-04_12-00-00_SVR_stream1.wav");
        std::fs::write(&rec_tor, b"RIFF").unwrap();
        std::fs::write(&rec_svr, b"RIFF").unwrap();

        let handle = DbHandle::open(&db_path).unwrap();
        let imported = handle.migrate_legacy_log(&log_path, &rec_dir).unwrap();
        assert_eq!(imported, 3);

        let conn = handle.conn.lock().unwrap();

        let rec_name1: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE event_code = 'TOR'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rec_name1.as_deref(),
            Some("EAS_Recording_2024-12-04_11-58-45_TOR_stream1.wav")
        );

        let rec_name2: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE event_code = 'SVR'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rec_name2.as_deref(),
            Some("EAS_Recording_2024-12-04_12-00-00_SVR_stream1.wav")
        );

        let rec_name3: Option<String> = conn
            .query_row(
                "SELECT recording_name FROM alerts WHERE event_code = 'FFW'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(rec_name3.is_none());
    }

    #[test]
    fn test_migrate_legacy_log_skips_when_db_has_data() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let log_path = dir.path().join("dedicated-alerts.log");
        let rec_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&rec_dir).unwrap();

        std::fs::write(
            &log_path,
            "ZCZC-WXR-TOR-031055+0030-1231645-KWO35-: Tornado Warning. (Received @ 2024-12-04 11:58:45 AM)\n\n",
        )
        .unwrap();

        let handle = DbHandle::open(&db_path).unwrap();

        let first = handle.migrate_legacy_log(&log_path, &rec_dir).unwrap();
        assert_eq!(first, 1);

        let second = handle.migrate_legacy_log(&log_path, &rec_dir).unwrap();
        assert_eq!(second, 0);

        let conn = handle.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM alerts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_migrate_legacy_log_missing_file() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let log_path = dir.path().join("does-not-exist.log");
        let rec_dir = dir.path().join("recordings");

        let handle = DbHandle::open(&db_path).unwrap();
        let imported = handle.migrate_legacy_log(&log_path, &rec_dir).unwrap();
        assert_eq!(imported, 0);
    }
}
