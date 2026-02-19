use crate::filter::{self, FilterRule};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EasAlertData {
    pub eas_text: String,
    pub event_text: String,
    pub event_code: String,
    pub fips: Vec<String>,
    pub locations: String,
    pub originator: String,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct ActiveAlert {
    pub data: EasAlertData,
    pub raw_header: String,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub received_at: DateTime<Utc>,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub expires_at: DateTime<Utc>,
    pub purge_time: Duration,
}

impl ActiveAlert {
    pub fn new(data: EasAlertData, raw_header: String, purge_time: Duration) -> Self {
        let received_at = Utc::now();
        let expires_at = received_at + purge_time;
        Self {
            data,
            raw_header,
            received_at,
            expires_at,
            purge_time,
        }
    }
}

pub struct AppState {
    pub active_alerts: Vec<ActiveAlert>,
    filters: Vec<FilterRule>,
}

impl AppState {
    pub fn new(filters: Vec<FilterRule>) -> Self {
        filter::install_filters(filters.clone());
        Self {
            active_alerts: Vec::new(),
            filters,
        }
    }

    pub fn cloned_filters(&self) -> Vec<FilterRule> {
        self.filters.clone()
    }

    pub fn update_filters(&mut self, filters: Vec<FilterRule>) {
        filter::install_filters(filters.clone());
        self.filters = filters;
    }
}
