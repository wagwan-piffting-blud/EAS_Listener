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

-- What the CAP-CP processor has already handled, so a restart or reload does not handle it again.
CREATE TABLE IF NOT EXISTS cap_seen (
    key        TEXT PRIMARY KEY,
    expires_at TEXT NOT NULL
);
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

    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per alerts column; a parameter struct would duplicate the schema without checking it"
    )]
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

    #[allow(
        clippy::too_many_arguments,
        reason = "see insert_same_alert: one argument per alerts column"
    )]
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

    /// The keys still in force, as `(key, expires_at)` in RFC 3339 UTC. Lapsed ones are deleted
    /// on the way, which is all the pruning the table needs.
    pub async fn load_cap_seen(&self, now: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.clone();
        let now = now.to_string();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;
            guard.execute("DELETE FROM cap_seen WHERE expires_at <= ?1", params![now])?;
            let mut statement = guard.prepare("SELECT key, expires_at FROM cap_seen")?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
            Ok(rows)
        })
        .await
        .context("DB read task panicked")?
    }

    pub async fn record_cap_seen(&self, key: &str, expires_at: &str) -> Result<()> {
        let conn = self.conn.clone();
        let key = key.to_string();
        let expires_at = expires_at.to_string();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;
            guard.execute(
                "INSERT INTO cap_seen (key, expires_at) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET expires_at = excluded.expires_at",
                params![key, expires_at],
            )?;
            Ok(())
        })
        .await
        .context("DB write task panicked")?
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

    /// Archived alerts in ascending id order, keeping only rows `keep` accepts.
    ///
    /// `limit` bounds how many rows are *kept*, not how many are examined: the scan walks newest
    /// first and stops once it has enough. Applying the limit in SQL instead would let rejected
    /// rows eat the budget and hand back fewer alerts than asked for -- sometimes none at all.
    pub async fn fetch_alerts_where<F>(
        &self,
        limit: Option<usize>,
        keep: F,
    ) -> Result<Vec<AlertRow>>
    where
        F: Fn(&AlertRow) -> bool + Send + 'static,
    {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;

            let sql = format!("SELECT {ALERT_COLUMNS} FROM alerts ORDER BY id DESC");
            let mut stmt = guard.prepare(&sql)?;
            let mut cursor = stmt.query_map([], |row| {
                Ok(AlertRow {
                    raw_zczc: row.get(0)?,
                    eas_text: row.get(1)?,
                    event_code: row.get(2)?,
                    event_text: row.get(3)?,
                    originator_name: row.get(4)?,
                    fips: row.get(5)?,
                    locations: row.get(6)?,
                    description: row.get(7)?,
                    recording_name: row.get(8)?,
                    source_type: row.get(9)?,
                    severity: row.get(10)?,
                    instructions: row.get(11)?,
                    duration_hhmm: row.get(12)?,
                    received_at: row.get(13)?,
                    expires_at: row.get(14)?,
                })
            })?;

            let mut kept: Vec<AlertRow> = Vec::new();
            while let Some(row) = cursor.next().transpose()? {
                if !keep(&row) {
                    continue;
                }
                kept.push(row);
                if limit.is_some_and(|limit| kept.len() >= limit) {
                    break;
                }
            }

            // Collected newest first; the dashboard renders oldest to newest.
            kept.reverse();
            Ok::<Vec<AlertRow>, anyhow::Error>(kept)
        })
        .await
        .context("Alert fetch task panicked")?
    }

    /// Deletes every archived alert except those whose raw header is still live, and reports which
    /// recordings the survivors still reference.
    ///
    /// The delete and the survivors' read share one transaction so a caller cannot archive away a
    /// recording that a row acquired in between.
    pub async fn vacuum_alerts(&self, keep_headers: Vec<String>) -> Result<VacuumOutcome> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("DB mutex poisoned: {}", e))?;

            let tx = guard.transaction()?;

            let alerts_deleted = if keep_headers.is_empty() {
                tx.execute("DELETE FROM alerts", [])?
            } else {
                let placeholders = std::iter::repeat_n("?", keep_headers.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!("DELETE FROM alerts WHERE raw_zczc NOT IN ({placeholders})");
                tx.execute(&sql, rusqlite::params_from_iter(keep_headers.iter()))?
            };

            let referenced_recordings = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT recording_name FROM alerts \
                     WHERE recording_name IS NOT NULL AND recording_name != ''",
                )?;
                let names = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                names
            };

            tx.commit()?;

            // VACUUM cannot run inside a transaction, and reclaiming space is a bonus rather than
            // the point, so a failure here does not fail the operation.
            let reclaimed = match guard.execute_batch("VACUUM;") {
                Ok(_) => true,
                Err(err) => {
                    warn!("Could not reclaim database space after vacuum: {}", err);
                    false
                }
            };

            Ok::<VacuumOutcome, anyhow::Error>(VacuumOutcome {
                alerts_deleted,
                referenced_recordings,
                reclaimed,
            })
        })
        .await
        .context("Alert vacuum task panicked")?
    }
}

/// Outcome of [`DbHandle::vacuum_alerts`].
pub struct VacuumOutcome {
    pub alerts_deleted: usize,
    /// Recording names the surviving rows still reference, which must not be archived away.
    pub referenced_recordings: Vec<String>,
    pub reclaimed: bool,
}

/// Column order must match the `row.get` indices in `fetch_alerts`.
const ALERT_COLUMNS: &str = "raw_zczc, eas_text, event_code, event_text, originator_name, \
     fips, locations, description, recording_name, source_type, severity, instructions, \
     duration_hhmm, received_at, expires_at";

#[derive(Debug, Clone)]
pub struct AlertRow {
    pub raw_zczc: String,
    pub eas_text: String,
    pub event_code: String,
    pub event_text: String,
    pub originator_name: String,
    pub fips: String,
    pub locations: String,
    pub description: Option<String>,
    pub recording_name: Option<String>,
    pub source_type: String,
    pub severity: Option<String>,
    pub instructions: Option<String>,
    pub duration_hhmm: Option<String>,
    pub received_at: String,
    pub expires_at: Option<String>,
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

    #[tokio::test]
    async fn cap_seen_survives_a_reopen_and_forgets_what_lapsed() {
        let (handle, dir) = test_db();
        handle
            .record_cap_seen("naad:live", "2026-09-20T00:00:00Z")
            .await
            .unwrap();
        handle
            .record_cap_seen("naad:old", "2026-09-18T00:00:00Z")
            .await
            .unwrap();
        // Recording again moves the expiry rather than failing on the key.
        handle
            .record_cap_seen("naad:live", "2026-09-21T00:00:00Z")
            .await
            .unwrap();
        drop(handle);

        let reopened = DbHandle::open(&dir.path().join("test_alerts.db")).unwrap();
        let seen = reopened
            .load_cap_seen("2026-09-19T12:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            seen,
            vec![("naad:live".to_string(), "2026-09-21T00:00:00Z".to_string())]
        );
    }

    async fn seed_alerts(handle: &DbHandle, codes: &[&str]) {
        for code in codes {
            handle
                .insert_same_alert(
                    &format!("ZCZC-EAS-{code}-000000+0015-0010000-TEST-"),
                    &format!("{code} body"),
                    code,
                    &format!("{code} event"),
                    "EAS",
                    "Test Originator",
                    &["000000".to_string()],
                    "Everywhere",
                    None,
                    Some("0015"),
                    "2026-09-17T18:00:00Z",
                    None,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn fetch_alerts_where_returns_oldest_first() {
        let (handle, _dir) = test_db();
        seed_alerts(&handle, &["AAA", "BBB", "CCC"]).await;

        let rows = handle.fetch_alerts_where(None, |_| true).await.unwrap();
        let codes: Vec<_> = rows.iter().map(|r| r.event_code.as_str()).collect();
        assert_eq!(codes, vec!["AAA", "BBB", "CCC"]);
    }

    #[tokio::test]
    async fn the_limit_counts_kept_rows_not_scanned_rows() {
        let (handle, _dir) = test_db();
        seed_alerts(&handle, &["AAA", "BBB", "CCC", "SKIP"]).await;

        // "SKIP" is the newest row. Applying the limit in SQL would spend the whole budget on it
        // and hand back nothing; the limit has to bound what survives the filter instead.
        let rows = handle
            .fetch_alerts_where(Some(1), |row| row.event_code != "SKIP")
            .await
            .unwrap();
        let codes: Vec<_> = rows.iter().map(|r| r.event_code.as_str()).collect();
        assert_eq!(codes, vec!["CCC"]);

        let rows = handle
            .fetch_alerts_where(Some(2), |row| row.event_code != "SKIP")
            .await
            .unwrap();
        let codes: Vec<_> = rows.iter().map(|r| r.event_code.as_str()).collect();
        assert_eq!(codes, vec!["BBB", "CCC"]);
    }

    #[tokio::test]
    async fn a_limit_larger_than_the_table_returns_everything_that_matches() {
        let (handle, _dir) = test_db();
        seed_alerts(&handle, &["AAA", "BBB"]).await;

        let rows = handle.fetch_alerts_where(Some(50), |_| true).await.unwrap();
        assert_eq!(rows.len(), 2);

        let rows = handle
            .fetch_alerts_where(Some(50), |_| false)
            .await
            .unwrap();
        assert!(rows.is_empty());
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
