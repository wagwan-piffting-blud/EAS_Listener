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
    fn matches_exact(&self, normalized_code: &str) -> bool {
        self.matchers.iter().any(|matcher| match matcher {
            EventCodeMatcher::Exact(expected) => expected == normalized_code,
            EventCodeMatcher::Wildcard => false,
        })
    }

    fn has_wildcard(&self) -> bool {
        self.matchers
            .iter()
            .any(|matcher| matches!(matcher, EventCodeMatcher::Wildcard))
    }
}

lazy_static! {
    static ref GLOBAL_FILTERS: RwLock<Vec<FilterRule>> = RwLock::new(Vec::new());
}

pub fn parse_filters(config_json: &Value) -> Vec<FilterRule> {
    let mut filters = Vec::new();

    let filters_enabled = config_json
        .get("ENABLE_FILTERS")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !filters_enabled {
        return filters;
    }

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
            warn!("Filter '{}' missing action field; defaulting to log", name);
            filters.push(FilterRule {
                name: name.to_string(),
                action: FilterAction::Log,
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
    let mut wildcard_match: Option<&FilterRule> = None;

    for rule in filters {
        if rule.matches_exact(&normalized) {
            return Some(rule);
        }

        if wildcard_match.is_none() && rule.has_wildcard() {
            wildcard_match = Some(rule);
        }
    }

    wildcard_match
}

#[allow(dead_code)]
pub fn should_relay_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    matches!(resolve_action(&filters, event_code), FilterAction::Relay)
}

#[allow(dead_code)]
pub fn should_log_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    matches!(
        resolve_action(&filters, event_code),
        FilterAction::Log | FilterAction::Forward | FilterAction::Relay
    )
}

#[allow(dead_code)]
pub fn should_forward_alert(event_code: &str) -> bool {
    let filters = GLOBAL_FILTERS.read();
    matches!(
        resolve_action(&filters, event_code),
        FilterAction::Forward | FilterAction::Relay
    )
}

pub fn should_log_action(action: FilterAction) -> bool {
    matches!(
        action,
        FilterAction::Log | FilterAction::Forward | FilterAction::Relay
    )
}

pub fn should_forward_action(action: FilterAction) -> bool {
    matches!(action, FilterAction::Forward | FilterAction::Relay)
}

fn resolve_action(filters: &[FilterRule], event_code: &str) -> FilterAction {
    match_filter(filters, event_code)
        .map(|rule| rule.action)
        .unwrap_or(FilterAction::Relay)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_filters_returns_empty_when_disabled() {
        let cfg = json!({
            "ENABLE_FILTERS": false,
            "FILTERS": [
                {
                    "name": "Ignored",
                    "event_codes": ["TOR"],
                    "action": "ignore"
                }
            ]
        });
        let filters = parse_filters(&cfg);
        assert!(filters.is_empty());
    }

    #[test]
    fn parse_filters_prefers_exact_over_wildcard() {
        let cfg = json!({
            "FILTERS": [
                {
                    "name": "Default",
                    "event_codes": ["*"],
                    "action": "relay"
                },
                {
                    "name": "Tornado",
                    "event_codes": ["tor"],
                    "action": "ignore"
                }
            ]
        });
        let filters = parse_filters(&cfg);
        let matched = match_filter(&filters, "TOR").expect("match");
        assert_eq!(matched.name, "Tornado");
        assert_eq!(evaluate_action(&filters, "TOR"), FilterAction::Ignore);
        assert_eq!(evaluate_action(&filters, "SVR"), FilterAction::Relay);
    }

    #[test]
    fn parse_filters_invalid_action_defaults_to_relay() {
        let cfg = json!({
            "FILTERS": [
                {
                    "name": "Broken",
                    "event_codes": ["RWT"],
                    "action": "drop"
                }
            ]
        });
        let filters = parse_filters(&cfg);
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].action, FilterAction::Relay);
    }

    #[test]
    fn global_filters_drive_helper_functions() {
        let cfg = json!({
            "FILTERS": [
                {
                    "name": "RWT ignore",
                    "event_codes": ["RWT"],
                    "action": "ignore"
                },
                {
                    "name": "Fallback",
                    "event_codes": ["*"],
                    "action": "forward"
                }
            ]
        });
        let filters = parse_filters(&cfg);
        install_filters(filters.clone());

        assert_eq!(determine_filter_name("RWT"), "RWT ignore");
        assert!(!should_relay_alert("RWT"));
        assert!(!should_log_alert("RWT"));
        assert!(!should_forward_alert("RWT"));

        assert_eq!(determine_filter_name("TOR"), "Fallback");
        assert!(!should_relay_alert("TOR"));
        assert!(should_log_alert("TOR"));
        assert!(should_forward_alert("TOR"));

        assert!(should_log_action(FilterAction::Relay));
        assert!(should_forward_action(FilterAction::Forward));
    }
}
