use lazy_static::lazy_static;
use parking_lot::RwLock;
use serde_json::Value;
use tracing::{error, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    Ignore,
    Relay,
    Log,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EventCodeMatcher {
    Exact(String),
    Wildcard,
}

#[derive(Debug, Clone)]
pub struct FilterRule {
    pub name: String,
    pub action: FilterAction,
    matchers: Vec<EventCodeMatcher>,
}

impl FilterRule {
    fn matches(&self, normalized_code: &str) -> bool {
        self.matchers.iter().any(|matcher| match matcher {
            EventCodeMatcher::Wildcard => true,
            EventCodeMatcher::Exact(expected) => expected == normalized_code,
        })
    }
}

lazy_static! {
    static ref GLOBAL_FILTERS: RwLock<Vec<FilterRule>> = RwLock::new(Vec::new());
}

pub fn parse_filters(config_json: &Value) -> Vec<FilterRule> {
    let mut filters = Vec::new();

    let Some(entries) = config_json.get("FILTERS").and_then(Value::as_array) else {
        return filters;
    };

    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str).map(str::trim) else {
            warn!("Skipping filter without a valid name: {:?}", entry);
            continue;
        };

        let Some(codes_value) = entry.get("event_codes").and_then(Value::as_array) else {
            warn!("Skipping filter '{}' due to missing event_codes", name);
            continue;
        };

        let mut matchers = Vec::with_capacity(codes_value.len());
        for code_value in codes_value {
            if let Some(pattern) = code_value.as_str() {
                let pattern = pattern.trim();
                if pattern == "*" {
                    matchers.push(EventCodeMatcher::Wildcard);
                } else if !pattern.is_empty() {
                    matchers.push(EventCodeMatcher::Exact(normalize_event_code(pattern)));
                }
            }
        }

        if matchers.is_empty() {
            warn!("Filter '{}' has no valid event codes; skipping", name);
            continue;
        }

        let Some(action_str) = entry.get("action").and_then(Value::as_str) else {
            warn!(
                "Filter '{}' missing action field; defaulting to relay",
                name
            );
            filters.push(FilterRule {
                name: name.to_string(),
                action: FilterAction::Relay,
                matchers,
            });
            continue;
        };

        let action = parse_action(action_str, name);

        filters.push(FilterRule {
            name: name.to_string(),
            action,
            matchers,
        });
    }

    filters
}

pub fn install_filters(filters: Vec<FilterRule>) {
    let mut global_filters = GLOBAL_FILTERS.write();
    *global_filters = filters;
}

#[allow(dead_code)]
pub fn evaluate_action(filters: &[FilterRule], event_code: &str) -> FilterAction {
    match_filter(filters, event_code)
        .map(|rule| rule.action)
        .unwrap_or(FilterAction::Relay)
}

pub fn determine_filter_name(event_code: &str) -> String {
    let filters = GLOBAL_FILTERS.read();
    match_filter(&filters, event_code)
        .map(|rule| rule.name.clone())
        .unwrap_or_else(|| "Default Filter".to_string())
}

pub fn match_filter<'a>(filters: &'a [FilterRule], event_code: &str) -> Option<&'a FilterRule> {
    let normalized = normalize_event_code(event_code);
    filters.iter().find(|rule| rule.matches(&normalized))
}

#[allow(dead_code)]
pub fn should_relay_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    match_filter(&filters, event_code)
        .map(|rule| rule.action != FilterAction::Ignore)
        .unwrap_or(true)
}

pub fn should_log_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    match_filter(&filters, event_code)
        .map(|rule| rule.action == FilterAction::Log || rule.action == FilterAction::Relay)
        .unwrap_or(false)
}

pub fn should_forward_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    match_filter(&filters, event_code)
        .map(|rule| rule.action == FilterAction::Forward)
        .unwrap_or(false)
}

fn parse_action(action: &str, filter_name: &str) -> FilterAction {
    match action.trim().to_ascii_lowercase().as_str() {
        "ignore" => FilterAction::Ignore,
        "relay" => FilterAction::Relay,
        "log" => FilterAction::Log,
        "forward" => FilterAction::Forward,
        other => {
            error!(
                "Filter '{}' has unsupported action '{}'; defaulting to relay",
                filter_name, other
            );
            FilterAction::Relay
        }
    }
}

fn normalize_event_code(code: &str) -> String {
    let mut normalized = code.trim().to_owned();
    normalized.make_ascii_uppercase();
    normalized
}
