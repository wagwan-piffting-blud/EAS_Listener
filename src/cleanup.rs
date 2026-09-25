use crate::config::Config;
use anyhow::Result;
use chrono::{Duration, Utc};
use tokio::time::interval;
use tracing::{info, warn};

pub async fn run_log_cleanup(config: Config) -> Result<()> {
    info!("Log cleanup task started. Will run every 24 hours.");
    let mut timer = interval(std::time::Duration::from_secs(24 * 60 * 60));

    loop {
        timer.tick().await;
        info!("Running daily log cleanup...");

        let retention_period = Duration::days(3);
        let now = Utc::now().date_naive();

        let mut entries = match tokio::fs::read_dir(&config.shared_state_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Log cleanup failed to read directory: {}", e);
                continue;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if let Some(filename_str) = path.file_name().and_then(|s| s.to_str()) {
                if filename_str.starts_with(&config.alert_log_file) {
                    if let Some(date_part) = filename_str.split('.').next_back() {
                        if let Ok(file_date) =
                            chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
                        {
                            if now.signed_duration_since(file_date) > retention_period {
                                info!("Deleting old log file: {}", filename_str);
                                if let Err(e) = tokio::fs::remove_file(&path).await {
                                    warn!("Failed to delete log file {}: {}", filename_str, e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
