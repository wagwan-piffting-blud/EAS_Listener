use crate::state::ActiveAlert;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub id: u64,
    #[serde(with = "chrono::serde::ts_milliseconds")]
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStatusPayload {
    pub stream_url: String,
    pub is_connected: bool,
    pub is_receiving_audio: bool,
    pub connection_attempts: u64,
    pub alerts_received: u64,
    #[serde(with = "chrono::serde::ts_seconds_option")]
    pub connected_since: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option")]
    pub last_activity: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option")]
    pub last_disconnect: Option<DateTime<Utc>>,
    #[serde(with = "chrono::serde::ts_seconds_option")]
    pub last_alert_received: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub uptime_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum MonitoringEvent {
    Log(LogEntry),
    Stream(StreamStatusPayload),
    Alerts(Vec<ActiveAlert>),
}

struct StreamTelemetry {
    stream_url: String,
    is_connected: bool,
    connected_since: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
    last_disconnect: Option<DateTime<Utc>>,
    last_error: Option<String>,
    attempts: u64,
    alerts_received: u64,
    last_alert_received: Option<DateTime<Utc>>,
}

impl StreamTelemetry {
    fn new(stream_url: String) -> Self {
        Self {
            stream_url,
            is_connected: false,
            connected_since: None,
            last_activity: None,
            last_disconnect: None,
            last_error: None,
            attempts: 0,
            alerts_received: 0,
            last_alert_received: None,
        }
    }
}

struct MonitoringState {
    logs: VecDeque<LogEntry>,
    streams: HashMap<String, StreamTelemetry>,
}

impl MonitoringState {
    fn new() -> Self {
        Self {
            logs: VecDeque::new(),
            streams: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct MonitoringHub {
    inner: Arc<RwLock<MonitoringState>>,
    events_tx: Sender<MonitoringEvent>,
    next_log_id: Arc<AtomicU64>,
    max_logs: usize,
    inactivity_timeout: Duration,
}

impl MonitoringHub {
    pub fn new(max_logs: usize, inactivity_timeout: Duration) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self {
            inner: Arc::new(RwLock::new(MonitoringState::new())),
            events_tx: tx,
            next_log_id: Arc::new(AtomicU64::new(1)),
            max_logs,
            inactivity_timeout,
        }
    }

    pub fn subscribe(&self) -> Receiver<MonitoringEvent> {
        self.events_tx.subscribe()
    }

    pub fn max_logs(&self) -> usize {
        self.max_logs
    }

    pub fn broadcast_alerts(&self, alerts: Vec<ActiveAlert>, source_stream: Option<&str>) {
        if let Some(stream) = source_stream {
            self.update_stream(stream, |state| {
                state.alerts_received = state.alerts_received.saturating_add(1);
                state.last_alert_received = Some(Utc::now());
            });
        }
        let _ = self.events_tx.send(MonitoringEvent::Alerts(alerts));
    }

    pub fn record_log(
        &self,
        level: Level,
        target: &str,
        message: String,
        fields: Map<String, Value>,
    ) {
        let entry = LogEntry {
            id: self.next_log_id.fetch_add(1, Ordering::Relaxed),
            timestamp: Utc::now(),
            level: level.to_string(),
            target: target.to_string(),
            message,
            fields,
        };
        {
            let mut guard = self.inner.write();
            guard.logs.push_back(entry.clone());
            while guard.logs.len() > self.max_logs {
                guard.logs.pop_front();
            }
        }
        let _ = self.events_tx.send(MonitoringEvent::Log(entry));
    }

    pub fn note_connecting(&self, stream: &str) {
        self.update_stream(stream, |state| {
            state.attempts = state.attempts.saturating_add(1);
            state.is_connected = false;
            state.connected_since = None;
            state.last_activity = None;
            state.last_error = None;
        });
    }

    pub fn note_connected(&self, stream: &str) {
        let now = Utc::now();
        self.update_stream(stream, |state| {
            state.is_connected = true;
            state.connected_since = Some(now);
            state.last_activity = Some(now);
            state.last_disconnect = None;
            state.last_error = None;
        });
    }

    pub fn note_activity(&self, stream: &str) {
        let now = Utc::now();
        self.update_stream(stream, |state| {
            state.last_activity = Some(now);
        });
    }

    pub fn note_error(&self, stream: &str, error: String) {
        self.update_stream(stream, move |state| {
            state.is_connected = false;
            state.connected_since = None;
            state.last_disconnect = Some(Utc::now());
            state.last_error = Some(error.clone());
        });
    }

    pub fn note_disconnected(&self, stream: &str) {
        let now = Utc::now();
        self.update_stream(stream, |state| {
            state.is_connected = false;
            state.connected_since = None;
            state.last_disconnect = Some(now);
        });
    }

    pub fn recent_logs(&self, count: usize) -> Vec<LogEntry> {
        let guard = self.inner.read();
        guard.logs.iter().rev().take(count).cloned().collect()
    }

    pub fn stream_snapshots(&self) -> Vec<StreamStatusPayload> {
        let guard = self.inner.read();
        let mut snapshots: Vec<_> = guard
            .streams
            .values()
            .map(|state| self.make_snapshot(state))
            .collect();
        snapshots.sort_by(|a, b| a.stream_url.cmp(&b.stream_url));
        snapshots
    }

    #[allow(dead_code)]
    pub fn stream_snapshot(&self, stream: &str) -> Option<StreamStatusPayload> {
        let guard = self.inner.read();
        guard
            .streams
            .get(stream)
            .map(|state| self.make_snapshot(state))
    }

    fn update_stream<F>(&self, stream: &str, mut update_fn: F)
    where
        F: FnMut(&mut StreamTelemetry),
    {
        let payload = {
            let mut guard = self.inner.write();
            let state = guard
                .streams
                .entry(stream.to_string())
                .or_insert_with(|| StreamTelemetry::new(stream.to_string()));
            update_fn(state);
            self.make_snapshot(state)
        };
        let _ = self.events_tx.send(MonitoringEvent::Stream(payload));
    }

    fn make_snapshot(&self, state: &StreamTelemetry) -> StreamStatusPayload {
        let now = Utc::now();
        let is_receiving_audio = state
            .last_activity
            .map(|ts| {
                now.signed_duration_since(ts)
                    .to_std()
                    .map(|dur| dur <= self.inactivity_timeout)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let uptime_seconds = if state.is_connected {
            state
                .connected_since
                .map(|since| (now - since).num_seconds().max(0))
        } else {
            None
        };
        StreamStatusPayload {
            stream_url: state.stream_url.clone(),
            is_connected: state.is_connected,
            is_receiving_audio,
            connection_attempts: state.attempts,
            alerts_received: state.alerts_received,
            connected_since: state.connected_since,
            last_activity: state.last_activity,
            last_disconnect: state.last_disconnect,
            last_alert_received: state.last_alert_received,
            last_error: state.last_error.clone(),
            uptime_seconds,
        }
    }
}

#[derive(Default)]
struct LogVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl LogVisitor {
    fn finish(self) -> (String, Map<String, Value>) {
        let message = self.message.unwrap_or_default();
        (message, self.fields)
    }
}

impl Visit for LogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let formatted = format!("{value:?}");
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(formatted);
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(formatted));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(value.into()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
}

pub struct MonitoringLayer {
    hub: MonitoringHub,
}

impl MonitoringLayer {
    pub fn new(hub: MonitoringHub) -> Self {
        Self { hub }
    }
}

impl<S> Layer<S> for MonitoringLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        let (message, fields) = visitor.finish();
        self.hub.record_log(
            *event.metadata().level(),
            event.metadata().target(),
            message,
            fields,
        );
    }
}
