// This file is part of E2T-NG, a tool to convert EAS messages to text. See the full repository here for more information: https://github.com/wagwan-piffting-blud/E2T-NG
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

const INVALID_HEADER_FORMAT: &str = "Invalid EAS header format";

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_ABBR_UPPER: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

static EAS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^ZCZC-([A-Z]{3})-([A-Z\?]{3})-((?:\d{6}(?:-?)){1,31})\+(\d{4})-(\d{7})-([A-Za-z0-9/ ]{1,8})-$")
        .expect("valid EAS regex")
});
static RE_REMOVE_COUNTY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s*County\b").expect("valid county regex"));
static RE_LEADING_AND: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^and\s+").expect("valid and regex"));
static RE_DAS_PART: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(City of )?(.*?)( County| Parish)?, (\w{2})$").expect("valid DAS regex")
});
static RE_DAS_STATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^State of (.+)$").expect("valid DAS state regex"));
static RE_CITY_WITH_STATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)City of (.*?), ([A-Z]{2})").expect("valid city-state regex"));
static RE_STATE_OF_CAPTURE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)State of (.*?)").expect("valid state regex"));
static RE_MONTH_DAY_ZERO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"([A-Z]{3}) 0(\d)").expect("valid month-day regex"));
static RE_HOUR_ZERO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"0(\d:\d\d [AP]M)").expect("valid hour regex"));
static RE_BANNED_CHARS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[\(\),!]").expect("valid banned-char regex"));
static RE_CITY_OF_CITY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"City of (.*?)( \(city\))?,").expect("valid city-of regex"));
static RE_LOCS_ARR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^;]+?, [A-Z]{2}").expect("valid loc regex"));

#[derive(Debug, Deserialize)]
struct SameResource {
    #[serde(rename = "SAME")]
    same: HashMap<String, String>,
    #[serde(rename = "SUBDIV")]
    subdiv: HashMap<String, String>,
    #[serde(rename = "ORGS")]
    orgs: HashMap<String, String>,
    #[serde(rename = "EVENTS")]
    events: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct EndecModesResource {
    #[serde(rename = "TEMPLATES")]
    templates: HashMap<String, String>,
}

static SAME_US: Lazy<SameResource> = Lazy::new(|| {
    serde_json::from_str(include_str!("../include/same-us.json")).expect("parse same-us.json")
});
static SAME_CA: Lazy<SameResource> = Lazy::new(|| {
    serde_json::from_str(include_str!("../include/same-ca.json")).expect("parse same-ca.json")
});
static ENDEC_MODES: Lazy<EndecModesResource> = Lazy::new(|| {
    serde_json::from_str(include_str!("../include/endec-modes.json"))
        .expect("parse endec-modes.json")
});

#[derive(Debug, Clone)]
pub struct DurationParts {
    pub hours: i64,
    pub minutes: i64,
}

#[derive(Debug, Clone)]
pub struct ParsedEas {
    pub originator: String,
    pub event_code: String,
    // Backward-compatible alias for callers that expect fips_codes naming.
    pub fips_codes: Vec<String>,
    pub locations: Vec<String>,
    pub duration: DurationParts,
    pub start_time: DateTime<Utc>,
    pub sender_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ParsedEasSerialized {
    pub originator: String,
    pub event_code: String,
    pub fips_codes: Vec<String>,
    pub locations: Vec<String>,
    pub duration_hours: i64,
    pub duration_minutes: i64,
    pub start_time_utc: String,
    pub sender_id: String,
}

impl ParsedEas {
    pub fn to_serialized(&self) -> ParsedEasSerialized {
        ParsedEasSerialized {
            originator: self.originator.clone(),
            event_code: self.event_code.clone(),
            fips_codes: self.fips_codes.clone(),
            locations: self.locations.clone(),
            duration_hours: self.duration.hours,
            duration_minutes: self.duration.minutes,
            start_time_utc: self.start_time.to_rfc3339(),
            sender_id: self.sender_id.clone(),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_serialized())
    }

    #[allow(dead_code)]
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.to_serialized())
    }
}

#[derive(Debug, Clone)]
struct FipsContext {
    #[allow(dead_code)]
    codes: Vec<String>,
    fips_text: Vec<String>,
    fips_text_with_and: Vec<String>,
    str_fips: String,
}

#[derive(Debug, Clone)]
struct SplitLocation {
    location: String,
    state: String,
}

#[derive(Debug, Clone)]
struct DasResult {
    str_fips: String,
    #[allow(dead_code)]
    only_parishes: bool,
}

#[derive(Debug, Clone)]
struct BaseRangeTime {
    start: String,
    end: String,
}

#[derive(Debug, Clone)]
struct ZonedParts {
    year: i32,
    month: u32,
    month_index: usize,
    day: u32,
    hour24: u32,
    minute: u32,
    weekday: usize,
    date_key: String,
}
#[derive(Debug, Clone)]
enum OutputTimeZone {
    Utc,
    Local,
    Named(Tz),
}

thread_local! {
    static TZ_OVERRIDE: RefCell<Option<OutputTimeZone>> = RefCell::new(None);
}

fn parse_output_timezone(spec: &str) -> Option<OutputTimeZone> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.eq_ignore_ascii_case("utc") {
        return Some(OutputTimeZone::Utc);
    }

    if trimmed.eq_ignore_ascii_case("local") {
        return Some(OutputTimeZone::Local);
    }

    Tz::from_str(trimmed).ok().map(OutputTimeZone::Named)
}

fn detect_default_output_timezone() -> OutputTimeZone {
    if let Ok(spec) = std::env::var("E2T_TIMEZONE") {
        if let Some(parsed) = parse_output_timezone(&spec) {
            return parsed;
        }
    }

    if std::panic::catch_unwind(Local::now).is_ok() {
        OutputTimeZone::Local
    } else {
        OutputTimeZone::Utc
    }
}

fn active_output_timezone() -> OutputTimeZone {
    TZ_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(detect_default_output_timezone)
}

fn with_timezone_override<T>(timezone: Option<OutputTimeZone>, func: impl FnOnce() -> T) -> T {
    TZ_OVERRIDE.with(|slot| {
        let previous = slot.replace(timezone);
        let result = func();
        slot.replace(previous);
        result
    })
}

fn explicit_timezone_override(spec: Option<&str>) -> Option<OutputTimeZone> {
    spec.map(|value| parse_output_timezone(value).unwrap_or(OutputTimeZone::Utc))
}

fn build_zoned_parts<TzImpl: TimeZone>(date: &DateTime<TzImpl>) -> ZonedParts {
    let year = date.year();
    let month = date.month();
    let day = date.day();
    let hour24 = date.hour();
    let minute = date.minute();
    let weekday = date.weekday().num_days_from_sunday() as usize;

    ZonedParts {
        year,
        month,
        month_index: (month.saturating_sub(1)) as usize,
        day,
        hour24,
        minute,
        weekday,
        date_key: format!("{:04}-{:02}-{:02}", year, month, day),
    }
}

fn build_time_zone_name<TzImpl: TimeZone>(date: &DateTime<TzImpl>) -> String
where
    TzImpl::Offset: std::fmt::Display,
{
    let name = date.format("%Z").to_string();
    if name.trim().is_empty() {
        "UTC".to_string()
    } else {
        name
    }
}

pub fn parse_header(header: &str) -> Option<ParsedEas> {
    let captures = EAS_REGEX.captures(header)?;
    let originator = captures.get(1)?.as_str().to_string();
    let event_code = captures.get(2)?.as_str().to_string();
    let locations = captures
        .get(3)?
        .as_str()
        .split('-')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let duration = parse_eas_duration(captures.get(4)?.as_str())?;
    let start_time = parse_eas_time(captures.get(5)?.as_str())?;
    let sender_id = captures.get(6)?.as_str().to_string();

    Some(ParsedEas {
        originator,
        event_code,
        fips_codes: locations.clone(),
        locations,
        duration,
        start_time,
        sender_id,
    })
}

pub fn parse_header_json(header: &str) -> Result<String, String> {
    let parsed = parse_header(header).ok_or_else(|| INVALID_HEADER_FORMAT.to_string())?;
    parsed.to_json().map_err(|error| error.to_string())
}

#[allow(dead_code)]
pub fn parse_header_pretty_json(header: &str) -> Result<String, String> {
    let parsed = parse_header(header).ok_or_else(|| INVALID_HEADER_FORMAT.to_string())?;
    parsed.to_pretty_json().map_err(|error| error.to_string())
}

fn parse_eas_time(time_str: &str) -> Option<DateTime<Utc>> {
    if time_str.len() != 7 {
        return None;
    }
    let day_of_year = time_str.get(0..3)?.parse::<i64>().ok()?;
    let hours = time_str.get(3..5)?.parse::<i64>().ok()?;
    let minutes = time_str.get(5..7)?.parse::<i64>().ok()?;

    let year = Utc::now().year();
    let jan_1 = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).single()?;
    Some(
        jan_1
            + Duration::days(day_of_year - 1)
            + Duration::hours(hours)
            + Duration::minutes(minutes),
    )
}

fn parse_eas_duration(duration_str: &str) -> Option<DurationParts> {
    if duration_str.len() != 4 {
        return None;
    }
    Some(DurationParts {
        hours: duration_str.get(0..2)?.parse::<i64>().ok()?,
        minutes: duration_str.get(2..4)?.parse::<i64>().ok()?,
    })
}

fn lookup_section(resource: &SameResource, section_key: &str, item_key: &str) -> Option<String> {
    match section_key {
        "SAME" => resource.same.get(item_key).cloned(),
        "SUBDIV" => resource.subdiv.get(item_key).cloned(),
        "ORGS" => resource.orgs.get(item_key).cloned(),
        "EVENTS" => resource.events.get(item_key).cloned(),
        _ => None,
    }
}

fn lookup_same(section_key: &str, item_key: &str, canadian_mode: bool) -> Option<String> {
    if canadian_mode {
        return lookup_section(&SAME_CA, section_key, item_key);
    }
    lookup_section(&SAME_US, section_key, item_key)
}

fn lookup_same_us(section_key: &str, item_key: &str) -> Option<String> {
    lookup_section(&SAME_US, section_key, item_key)
}

fn apply_mode_template(mode_key: &str, replacements: &[(&str, String)]) -> String {
    let mut template = match ENDEC_MODES.templates.get(mode_key) {
        Some(value) => value.clone(),
        None => return String::new(),
    };
    for (key, value) in replacements {
        template = template.replace(&format!("__{}__", key), value);
    }
    template
}

fn all_endec_modes() -> Vec<String> {
    let mut modes = ENDEC_MODES.templates.keys().cloned().collect::<Vec<_>>();
    modes.sort_unstable();
    modes
}

fn ordinal_day(day: u32) -> String {
    let mod_100 = day % 100;
    if (11..=13).contains(&mod_100) {
        return format!("{}th", day);
    }
    match day % 10 {
        1 => format!("{}st", day),
        2 => format!("{}nd", day),
        3 => format!("{}rd", day),
        _ => format!("{}th", day),
    }
}

fn get_zoned_parts(date: &DateTime<Utc>) -> ZonedParts {
    match active_output_timezone() {
        OutputTimeZone::Utc => build_zoned_parts(date),
        OutputTimeZone::Local => {
            if let Ok(local_date) = std::panic::catch_unwind(|| date.with_timezone(&Local)) {
                build_zoned_parts(&local_date)
            } else {
                build_zoned_parts(date)
            }
        }
        OutputTimeZone::Named(tz) => {
            let zoned = date.with_timezone(&tz);
            build_zoned_parts(&zoned)
        }
    }
}

fn pad2(number: u32) -> String {
    format!("{:02}", number)
}

fn format_time12(date: &DateTime<Utc>, pad_hour: bool, lower_meridiem: bool) -> String {
    let parts = get_zoned_parts(date);
    let mut hour12 = parts.hour24 % 12;
    if hour12 == 0 {
        hour12 = 12;
    }
    let hour = if pad_hour {
        pad2(hour12)
    } else {
        hour12.to_string()
    };
    let meridiem = if lower_meridiem {
        if parts.hour24 >= 12 {
            "pm"
        } else {
            "am"
        }
    } else if parts.hour24 >= 12 {
        "PM"
    } else {
        "AM"
    };
    format!("{}:{} {}", hour, pad2(parts.minute), meridiem)
}

fn locale_helper(date: &DateTime<Utc>) -> String {
    let mut parts = get_zoned_parts(date);
    if parts.month_index == 11 && parts.day == 31 && date.month() == 1 && date.day() == 1 {
        parts.month_index = 0;
        parts.day = 1;
        parts.year = date.year();
    }
    format!(
        "{} on {} {}, {}",
        format_time12(date, false, false),
        MONTH_NAMES[parts.month_index],
        ordinal_day(parts.day),
        parts.year
    )
}

fn format_mon_day_year(
    date: &DateTime<Utc>,
    upper_month: bool,
    short_month: bool,
    include_year: bool,
) -> String {
    let parts = get_zoned_parts(date);
    let month_name = if short_month {
        if upper_month {
            MONTH_ABBR_UPPER[parts.month_index].to_string()
        } else {
            MONTH_ABBR[parts.month_index].to_string()
        }
    } else if upper_month {
        MONTH_NAMES[parts.month_index].to_uppercase()
    } else {
        MONTH_NAMES[parts.month_index].to_string()
    };
    let day_text = pad2(parts.day);

    if include_year {
        format!("{} {}, {}", month_name, day_text, parts.year)
    } else {
        format!("{} {}", month_name, day_text)
    }
}

fn get_zoned_time_zone_name(date: &DateTime<Utc>) -> String {
    match active_output_timezone() {
        OutputTimeZone::Utc => "UTC".to_string(),
        OutputTimeZone::Local => {
            if let Ok(local_date) = std::panic::catch_unwind(|| date.with_timezone(&Local)) {
                build_time_zone_name(&local_date)
            } else {
                "UTC".to_string()
            }
        }
        OutputTimeZone::Named(tz) => {
            let zoned = date.with_timezone(&tz);
            build_time_zone_name(&zoned)
        }
    }
}

fn format_slash_utc(date: &DateTime<Utc>) -> String {
    let parts = get_zoned_parts(date);
    let year_short = ((parts.year % 100) + 100) % 100;
    format!(
        "{}/{}/{} {}:{}:00 {}",
        pad2(parts.month),
        pad2(parts.day),
        pad2(year_short as u32),
        pad2(parts.hour24),
        pad2(parts.minute),
        get_zoned_time_zone_name(date)
    )
}

fn is_same_local_day(start_time: &DateTime<Utc>, end_time: &DateTime<Utc>) -> bool {
    get_zoned_parts(start_time).date_key == get_zoned_parts(end_time).date_key
}

fn format_base_range_time_text(
    start_time: &DateTime<Utc>,
    end_time: &DateTime<Utc>,
) -> BaseRangeTime {
    let start_parts = get_zoned_parts(start_time);
    let end_parts = get_zoned_parts(end_time);

    if start_parts.date_key == end_parts.date_key {
        return BaseRangeTime {
            start: format_time12(start_time, true, false),
            end: format_time12(end_time, true, false),
        };
    }

    if start_parts.year == end_parts.year {
        return BaseRangeTime {
            start: format!(
                "{} {}",
                format_time12(start_time, true, false),
                format_mon_day_year(start_time, false, false, false)
            ),
            end: format!(
                "{} {}",
                format_time12(end_time, true, false),
                format_mon_day_year(end_time, false, false, false)
            ),
        };
    }

    BaseRangeTime {
        start: format!(
            "{} {}",
            format_time12(start_time, true, false),
            format_mon_day_year(start_time, false, false, true)
        ),
        end: format!(
            "{} {}",
            format_time12(end_time, true, false),
            format_mon_day_year(end_time, false, false, true)
        ),
    }
}

fn state_name(abbr: &str) -> Option<&'static str> {
    match abbr {
        "AL" => Some("Alabama"),
        "AK" => Some("Alaska"),
        "AZ" => Some("Arizona"),
        "AR" => Some("Arkansas"),
        "CA" => Some("California"),
        "CO" => Some("Colorado"),
        "CT" => Some("Connecticut"),
        "DE" => Some("Delaware"),
        "FL" => Some("Florida"),
        "GA" => Some("Georgia"),
        "HI" => Some("Hawaii"),
        "ID" => Some("Idaho"),
        "IL" => Some("Illinois"),
        "IN" => Some("Indiana"),
        "IA" => Some("Iowa"),
        "KS" => Some("Kansas"),
        "KY" => Some("Kentucky"),
        "LA" => Some("Louisiana"),
        "ME" => Some("Maine"),
        "MD" => Some("Maryland"),
        "MA" => Some("Massachusetts"),
        "MI" => Some("Michigan"),
        "MN" => Some("Minnesota"),
        "MS" => Some("Mississippi"),
        "MO" => Some("Missouri"),
        "MT" => Some("Montana"),
        "NE" => Some("Nebraska"),
        "NV" => Some("Nevada"),
        "NH" => Some("New Hampshire"),
        "NJ" => Some("New Jersey"),
        "NM" => Some("New Mexico"),
        "NY" => Some("New York"),
        "NC" => Some("North Carolina"),
        "ND" => Some("North Dakota"),
        "OH" => Some("Ohio"),
        "OK" => Some("Oklahoma"),
        "OR" => Some("Oregon"),
        "PA" => Some("Pennsylvania"),
        "RI" => Some("Rhode Island"),
        "SC" => Some("South Carolina"),
        "SD" => Some("South Dakota"),
        "TN" => Some("Tennessee"),
        "TX" => Some("Texas"),
        "UT" => Some("Utah"),
        "VT" => Some("Vermont"),
        "VA" => Some("Virginia"),
        "WA" => Some("Washington"),
        "WV" => Some("West Virginia"),
        "WI" => Some("Wisconsin"),
        "WY" => Some("Wyoming"),
        _ => None,
    }
}

fn province_name(abbr: &str) -> Option<&'static str> {
    match abbr {
        "AB" => Some("Alberta"),
        "BC" => Some("British Columbia"),
        "MB" => Some("Manitoba"),
        "NB" => Some("New Brunswick"),
        "NL" => Some("Newfoundland and Labrador"),
        "NS" => Some("Nova Scotia"),
        "NT" => Some("Northwest Territories"),
        "NU" => Some("Nunavut"),
        "ON" => Some("Ontario"),
        "PE" => Some("Prince Edward Island"),
        "QC" => Some("Quebec"),
        "SK" => Some("Saskatchewan"),
        "YT" => Some("Yukon"),
        _ => None,
    }
}

fn expand_state_abbreviation(name: &str) -> String {
    if name.len() < 2 {
        return name.to_string();
    }
    let suffix = &name[name.len() - 2..];
    if suffix.chars().all(|ch| ch.is_ascii_uppercase()) {
        if let Some(full) = state_name(suffix) {
            return format!("{}{}", &name[..name.len() - 2], full);
        }
    }
    name.to_string()
}

fn remove_county_word(text: &str) -> String {
    if text.contains("County") {
        RE_REMOVE_COUNTY.replace_all(text, "").to_string()
    } else {
        text.to_string()
    }
}

fn replace_case_insensitive_all(input: &str, needle: &str, replacement: &str) -> String {
    let pattern = format!("(?i){}", regex::escape(needle));
    Regex::new(&pattern)
        .expect("valid replacement pattern")
        .replace_all(input, replacement)
        .to_string()
}

fn contains_case_insensitive(input: &str, needle: &str) -> bool {
    let pattern = format!("(?i){}", regex::escape(needle));
    Regex::new(&pattern)
        .expect("valid match pattern")
        .is_match(input)
}

fn build_fips_context(location_codes: &[String], canadian_mode: bool) -> FipsContext {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for code in location_codes {
        if seen.insert(code.clone()) {
            deduped.push(code.clone());
        }
    }

    let fips_text = deduped
        .iter()
        .map(|code| {
            let subdiv = code
                .get(0..1)
                .and_then(|key| lookup_same_us("SUBDIV", key))
                .unwrap_or_default();
            let same_name = code
                .get(1..6)
                .and_then(|key| lookup_same("SAME", key, canadian_mode))
                .unwrap_or_else(|| format!("FIPS Code {}", code));

            if subdiv.is_empty() {
                same_name
            } else {
                format!("{} {}", subdiv, same_name)
            }
        })
        .collect::<Vec<_>>();

    let mut fips_text_with_and = fips_text.clone();
    if fips_text_with_and.len() > 1 {
        let last_index = fips_text_with_and.len() - 1;
        fips_text_with_and[last_index] = format!("and {}", fips_text_with_and[last_index]);
    }

    FipsContext {
        codes: deduped,
        fips_text,
        fips_text_with_and: fips_text_with_and.clone(),
        str_fips: format!("{};", fips_text_with_and.join("; ")),
    }
}

fn split_location_state(text: &str) -> SplitLocation {
    let parts = text
        .split(", ")
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    if parts.len() <= 1 {
        return SplitLocation {
            location: parts.first().copied().unwrap_or(text).to_string(),
            state: String::new(),
        };
    }

    SplitLocation {
        location: parts[..parts.len() - 1].join(", "),
        state: parts[parts.len() - 1].to_string(),
    }
}

fn filter_trilithic_location(text: &str) -> String {
    let mut result = text.to_string();
    if result.starts_with("City of") {
        let stripped = result.trim_start_matches("City of").trim_start();
        result = format!("{} city", stripped);
    }
    if result.starts_with("State of") {
        result = result.replacen("State of", "All of", 1);
    }
    if result.starts_with("District of") {
        result = result.replacen("District of", "All of District of", 1);
    }
    if result.contains(" City of") {
        result = result.replace(" City of", "");
        result.push_str(" city");
    }
    if result.contains(" State of") && !result.contains("All of") {
        result = result.replace(" State of", " All of");
    }
    if result.contains(" District of") && !result.contains("All of") {
        result = result.replace(" District of", " All of District of");
    }
    if result.contains(" County") {
        result = result.replace(" County", "");
    }
    if result.starts_with("and ") {
        result = result.replacen("and ", "", 1);
    }
    result
}

fn filter_holly_or_gorman_location(text: &str, is_gorman: bool) -> String {
    let mut result = text.to_string();
    if result.starts_with("City of") {
        let stripped = result.trim_start_matches("City of").trim_start();
        result = format!("{} CITY", stripped);
    }
    if result.starts_with("State of") {
        result = if is_gorman {
            result.replacen("State of", "ALL", 1)
        } else {
            result.replacen("State of", "", 1)
        };
    }
    if result.contains(" City of") {
        result = result.replace(" City of", "");
        result.push_str(" CITY");
    }
    if result.contains(" State of") && !result.contains("All of") {
        result = result.replace(" State of", "");
    }
    if result.contains(" County") {
        result = result.replace(" County", "");
    }
    if result.starts_with("and ") {
        result = result.replacen("and ", "AND ", 1);
    }
    result
}

fn process_das_fips_string(str_fips: &str, combine_same_state: bool) -> DasResult {
    let parts = str_fips
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    let mut states: Vec<(String, Vec<String>)> = Vec::new();
    let mut state_index: HashMap<String, usize> = HashMap::new();
    let mut result = Vec::new();
    let mut only_parishes = false;

    for part_raw in &parts {
        let part = RE_LEADING_AND.replace(part_raw, "").to_string();

        if let Some(captures) = RE_DAS_PART.captures(&part) {
            let city_prefix = captures.get(1).map(|v| v.as_str()).unwrap_or("");
            let name = captures.get(2).map(|v| v.as_str()).unwrap_or("");
            let locality_type = captures.get(3).map(|v| v.as_str()).unwrap_or("");
            let state = captures.get(4).map(|v| v.as_str()).unwrap_or("");

            let mut clean_name = name.to_string();
            if locality_type == " Parish" {
                // Preserve original behavior.
            } else if !city_prefix.is_empty() {
                clean_name.push_str(" (city)");
                only_parishes = false;
            } else {
                only_parishes = false;
            }

            let idx = if let Some(existing) = state_index.get(state) {
                *existing
            } else {
                let new_idx = states.len();
                states.push((state.to_string(), Vec::new()));
                state_index.insert(state.to_string(), new_idx);
                new_idx
            };
            states[idx].1.push(clean_name);
        } else if let Some(captures) = RE_DAS_STATE.captures(&part) {
            result.push(
                captures
                    .get(1)
                    .map(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            );
        } else {
            result.push(part);
        }
    }

    for (state, entries) in states {
        let last_index = entries.len().saturating_sub(1);
        for (index, name) in entries.into_iter().enumerate() {
            if !combine_same_state || index == last_index {
                result.push(format!("{}, {}", name, state));
            } else {
                result.push(name);
            }
        }
    }

    let mut final_result = result.join("; ").replace(" and ", " ");
    final_result = RE_CITY_OF_CITY
        .replace_all(&final_result, "$1 (city),")
        .to_string();
    if final_result.is_empty() {
        final_result = parts
            .iter()
            .map(|part| RE_LEADING_AND.replace(part, "").to_string())
            .collect::<Vec<_>>()
            .join("; ");
    }

    DasResult {
        str_fips: format!("{};", final_result),
        only_parishes,
    }
}

fn format_location(location_code: &str, is_last_item: bool, total_locations: usize) -> String {
    let subdivision_code = location_code.get(0..1).unwrap_or_default();
    let same_code = location_code.get(1..6).unwrap_or_default();

    let location_name =
        lookup_same("SAME", same_code, false).unwrap_or_else(|| same_code.to_string());
    let subdivision_name = lookup_same_us("SUBDIV", subdivision_code);

    let described_location = if let Some(subdivision_name) = subdivision_name {
        if !subdivision_name.is_empty() {
            let base_location = if is_last_item && total_locations > 1 {
                expand_state_abbreviation(&location_name)
            } else {
                location_name.clone()
            };
            format!("{}ern {}", subdivision_name, base_location)
        } else if location_name.contains("All of") || location_name.contains("State of") {
            location_name.clone()
        } else {
            expand_state_abbreviation(&location_name)
        }
    } else if location_name.contains("All of") || location_name.contains("State of") {
        location_name
    } else {
        expand_state_abbreviation(&location_name)
    };

    if is_last_item && total_locations > 1 {
        format!("and {}", described_location)
    } else {
        described_location
    }
}

fn humanize_eas(eas: &ParsedEas, endec_emulation_mode: &str, canadian_mode: bool) -> String {
    let sender = eas.sender_id.trim().to_string();
    let mut normal_originator =
        lookup_same("ORGS", &eas.originator, false).unwrap_or_else(|| eas.originator.clone());
    let normal_event_code =
        lookup_same("EVENTS", &eas.event_code, false).unwrap_or_else(|| eas.event_code.clone());

    let mut fips_context = build_fips_context(&eas.locations, canadian_mode);

    let mut _location_str = eas
        .locations
        .iter()
        .enumerate()
        .map(|(idx, loc)| {
            format_location(
                loc,
                idx == eas.locations.len().saturating_sub(1),
                eas.locations.len(),
            )
        })
        .collect::<Vec<_>>()
        .join("; ");

    let end_time = eas.start_time
        + Duration::hours(eas.duration.hours)
        + Duration::minutes(eas.duration.minutes);
    let start_time_str = locale_helper(&eas.start_time);
    let end_time_str = locale_helper(&end_time);
    let base_range_time = format_base_range_time_text(&eas.start_time, &end_time);
    let mode = endec_emulation_mode.to_uppercase();

    if mode == "ALL" {
        return all_endec_modes()
            .into_iter()
            .filter(|mode_name| mode_name != "ALL")
            .map(|mode_name| {
                format!(
                    "{}: {}",
                    mode_name,
                    humanize_eas(eas, &mode_name, canadian_mode)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
    }

    _location_str = remove_county_word(&_location_str);
    fips_context.fips_text = fips_context
        .fips_text
        .iter()
        .map(|item| remove_county_word(item))
        .collect();
    fips_context.fips_text_with_and = fips_context
        .fips_text_with_and
        .iter()
        .map(|item| remove_county_word(item))
        .collect();
    fips_context.str_fips = remove_county_word(&fips_context.str_fips);

    if canadian_mode && eas.originator == "WXR" {
        normal_originator = "Environment Canada".to_string();
    }

    match mode.as_str() {
        "TFT" => {
            let mut str_fips = fips_context.str_fips.trim_end_matches(';').to_string();
            str_fips = str_fips.replace(',', "").replace(';', ",");
            str_fips = str_fips.replace("FIPS Code", "AREA");
            str_fips = str_fips.replace("State of ", "");
            str_fips = replace_case_insensitive_all(
                &str_fips,
                "All of The United States",
                "UNITED STATES",
            );

            let tft_start = format!(
                "{} ON {}",
                format_time12(&eas.start_time, true, false),
                format_mon_day_year(&eas.start_time, true, true, true)
            );
            let tft_end = if is_same_local_day(&eas.start_time, &end_time) {
                format_time12(&end_time, true, false)
            } else {
                format!(
                    "{} ON {}",
                    format_time12(&end_time, true, false),
                    format_mon_day_year(&end_time, true, true, true)
                )
            };

            let prefix =
                if eas.originator == "EAS" || eas.event_code == "NPT" || eas.event_code == "EAN" {
                    format!("{} has been issued", normal_event_code)
                } else {
                    format!("{} has issued {}", normal_originator, normal_event_code)
                };

            let tft_start = RE_MONTH_DAY_ZERO
                .replace_all(&tft_start, "$1 $2")
                .to_string();
            let tft_start = RE_HOUR_ZERO.replace(&tft_start, "$1").to_string();
            let tft_end = RE_MONTH_DAY_ZERO.replace_all(&tft_end, "$1 $2").to_string();
            let tft_end = RE_HOUR_ZERO.replace(&tft_end, "$1").to_string();

            apply_mode_template(
                "TFT",
                &[
                    ("PREFIX", prefix),
                    ("FIPS", str_fips),
                    ("START", tft_start),
                    ("END", tft_end),
                    ("SENDER", sender),
                ],
            )
            .to_uppercase()
        }
        "SAGE" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "CIV" {
                org_text = "The Civil Authorities".to_string();
            }
            if eas.originator == "EAS" {
                org_text = "An EAS Participant".to_string();
            }

            let start_parts = get_zoned_parts(&eas.start_time);
            let end_parts = get_zoned_parts(&end_time);
            let same_day = start_parts.date_key == end_parts.date_key;

            let sage_start = format!(
                "{}{}",
                format_time12(&eas.start_time, true, true),
                if same_day {
                    String::new()
                } else {
                    format!(
                        " {} {} {}",
                        WEEKDAY_ABBR[start_parts.weekday],
                        MONTH_ABBR[start_parts.month_index],
                        pad2(start_parts.day)
                    )
                }
            );
            let sage_end = format!(
                "{}{}",
                format_time12(&end_time, true, true),
                if same_day {
                    String::new()
                } else {
                    format!(
                        " {} {} {}",
                        WEEKDAY_ABBR[end_parts.weekday],
                        MONTH_ABBR[end_parts.month_index],
                        pad2(end_parts.day)
                    )
                }
            );

            let mut str_fips = fips_context
                .str_fips
                .trim_end_matches(';')
                .replace(';', ",");
            str_fips = replace_case_insensitive_all(
                &str_fips,
                "All of The United States",
                "all of the United States",
            );
            let str_fips = RE_CITY_WITH_STATE
                .replace_all(&str_fips, "$1 city, $2")
                .to_string();
            let str_fips = RE_STATE_OF_CAPTURE
                .replace_all(&str_fips, "all of $1")
                .to_string();

            apply_mode_template(
                "SAGE",
                &[
                    ("ORG", org_text),
                    (
                        "HAVEHAS",
                        if eas.originator == "CIV" {
                            "have".to_string()
                        } else {
                            "has".to_string()
                        },
                    ),
                    ("EVENTCODE", normal_event_code),
                    ("FIPS", str_fips),
                    ("START", sage_start),
                    ("END", sage_end),
                    ("SENDER", sender),
                ],
            )
        }
        "TRILITHIC6" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "CIV" {
                org_text = "Civil Authorities".to_string();
            }

            let trilithic_locations = fips_context
                .fips_text_with_and
                .iter()
                .map(|entry| {
                    let split = split_location_state(entry);
                    let clean = filter_trilithic_location(&split.location);
                    if split.state.is_empty() {
                        clean
                    } else {
                        format!("{} {}", clean, split.state)
                    }
                })
                .collect::<Vec<_>>()
                .join(" - ");

            let fips_text = if trilithic_locations.contains("All of The United States") {
                "the United States".to_string()
            } else {
                format!("the following counties: {}", trilithic_locations)
            };

            let output = apply_mode_template(
                "TRILITHIC6",
                &[
                    ("ORG", org_text),
                    (
                        "HAVEHAS",
                        if eas.originator == "CIV" {
                            "have".to_string()
                        } else {
                            "has".to_string()
                        },
                    ),
                    ("EVENTCODE", normal_event_code),
                    ("FIPS", fips_text),
                    ("END", format_slash_utc(&end_time)),
                ],
            );
            RE_BANNED_CHARS.replace_all(&output, " ").to_string()
        }
        "TRILITHIC8PLUS" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "CIV" {
                org_text = "The Civil Authorities".to_string();
            }

            let trilithic_locations = fips_context
                .fips_text_with_and
                .iter()
                .map(|entry| {
                    let split = split_location_state(entry);
                    let clean = filter_trilithic_location(&split.location);
                    if split.state.is_empty() {
                        clean
                    } else {
                        format!("{} {}", clean, split.state)
                    }
                })
                .collect::<Vec<_>>()
                .join(" - ");

            let fips_text = if trilithic_locations.contains("All of The United States") {
                "the United States".to_string()
            } else {
                format!("the following counties: {}", trilithic_locations)
            };

            let output = apply_mode_template(
                "TRILITHIC8PLUS",
                &[
                    ("ORG", org_text),
                    (
                        "HAVEHAS",
                        if eas.originator == "CIV" {
                            "have".to_string()
                        } else {
                            "has".to_string()
                        },
                    ),
                    ("EVENTCODE", normal_event_code),
                    ("FIPS", fips_text),
                    ("END", format_slash_utc(&end_time)),
                    ("SENDER", sender),
                ],
            );
            RE_BANNED_CHARS.replace_all(&output, " ").to_string()
        }
        "BURK" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "EAS" {
                org_text = "A Broadcast station or cable system".to_string();
            } else if eas.originator == "CIV" {
                org_text = "The Civil Authorities".to_string();
            }

            let str_fips = fips_context
                .str_fips
                .trim_end_matches(';')
                .replace(',', "")
                .replace(';', ",");
            let event_text = normal_event_code
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ")
                .to_uppercase();
            let burk_start = format!(
                "{} at {}",
                format_mon_day_year(&eas.start_time, true, false, true),
                format_time12(&eas.start_time, true, false)
            );
            let burk_end = format!(
                "{}, {}",
                format_time12(&end_time, true, false),
                format_mon_day_year(&end_time, true, false, true)
            )
            .to_uppercase();

            apply_mode_template(
                "BURK",
                &[
                    ("ORG", org_text),
                    (
                        "HAVEHAS",
                        if eas.originator == "CIV" {
                            "have".to_string()
                        } else {
                            "has".to_string()
                        },
                    ),
                    ("EVENTTEXT", event_text),
                    (
                        "FIPS",
                        if str_fips.contains("All of The United States") {
                            "the United States".to_string()
                        } else {
                            format!("for the following counties/areas: {}", str_fips)
                        },
                    ),
                    ("START", burk_start),
                    ("END", burk_end),
                ],
            )
        }
        "DAS1" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "EAS" {
                org_text = "A broadcast or cable system".to_string();
            } else if eas.originator == "CIV" {
                org_text = "A civil authority".to_string();
            } else if eas.originator == "PEP" {
                org_text = "THE PRIMARY ENTRY POINT EAS SYSTEM".to_string();
            }

            let das = process_das_fips_string(&fips_context.str_fips, false);
            let das_fips = replace_case_insensitive_all(
                &das.str_fips,
                "All of the United States",
                "United States",
            );
            let das_start = format!(
                "{} ON {}",
                format_time12(&eas.start_time, false, false).to_uppercase(),
                format_mon_day_year(&eas.start_time, true, true, true)
            );
            let das_end = if is_same_local_day(&eas.start_time, &end_time) {
                format_time12(&end_time, false, false).to_uppercase()
            } else {
                format!(
                    "{} {}",
                    format_time12(&end_time, false, false).to_uppercase(),
                    format_mon_day_year(&end_time, true, true, true)
                )
            };

            apply_mode_template(
                "DAS1",
                &[
                    ("ORG", org_text.to_uppercase()),
                    ("EVENTCODE", normal_event_code.to_uppercase()),
                    ("FIPS", das_fips),
                    ("START", das_start),
                    ("END", das_end),
                    ("SENDER", sender.to_uppercase()),
                ],
            )
            .to_uppercase()
        }
        "DAS2PLUS" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "EAS" {
                org_text = "A broadcast or cable system".to_string();
            } else if eas.originator == "CIV" {
                org_text = "A civil authority".to_string();
            } else if eas.originator == "PEP" {
                org_text = "THE PRIMARY ENTRY POINT EAS SYSTEM".to_string();
            }

            let das = process_das_fips_string(&fips_context.str_fips, true);
            let das_fips = replace_case_insensitive_all(
                &das.str_fips,
                "All of the United States",
                "United States",
            );
            let das_start = format!(
                "{} on {}",
                format_time12(&eas.start_time, false, false).to_uppercase(),
                format_mon_day_year(&eas.start_time, true, true, true)
            );
            let das_end = if is_same_local_day(&eas.start_time, &end_time) {
                format_time12(&end_time, false, false).to_uppercase()
            } else {
                format!(
                    "{} {}",
                    format_time12(&end_time, false, false).to_uppercase(),
                    format_mon_day_year(&end_time, true, true, true)
                )
            };

            let das_start = RE_MONTH_DAY_ZERO.replace(&das_start, "$1 $2").to_string();
            let das_end = RE_MONTH_DAY_ZERO.replace(&das_end, "$1 $2").to_string();

            apply_mode_template(
                "DAS2PLUS",
                &[
                    ("ORG", org_text),
                    ("EVENTCODE", normal_event_code.to_uppercase()),
                    ("FIPS", das_fips),
                    ("START", das_start),
                    ("END", das_end),
                    ("SENDER", sender),
                ],
            )
        }
        "HOLLYANNE" => {
            let mut org_text = normal_originator.clone();
            if eas.originator == "EAS" {
                org_text = "THE CABLE/BROADCAST SYSTEM".to_string();
            } else if eas.originator == "CIV" {
                org_text = "THE AUTHORITIES".to_string();
            }

            let states = fips_context
                .fips_text_with_and
                .iter()
                .map(|entry| split_location_state(entry).state)
                .filter(|state| !state.is_empty())
                .collect::<HashSet<_>>();

            let holly_locations = fips_context
                .fips_text_with_and
                .iter()
                .map(|entry| {
                    let split = split_location_state(entry);
                    let clean =
                        filter_holly_or_gorman_location(&split.location, false).to_uppercase();
                    if !split.state.is_empty() && states.len() != 1 {
                        format!("{} {}", clean, split.state.to_uppercase())
                    } else {
                        clean.trim().to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
                .replace(", AND ", " AND ")
                .trim()
                .to_string();

            let output = apply_mode_template(
                "HOLLYANNE",
                &[
                    ("ORG", org_text),
                    ("EVENTCODE", normal_event_code.to_uppercase()),
                    (
                        "FIPS",
                        if contains_case_insensitive(&holly_locations, "All of the United States") {
                            "For all of the United States".to_string()
                        } else {
                            format!("FOR THE FOLLOWING COUNTIES: {}", holly_locations)
                        },
                    ),
                    ("START", base_range_time.start.to_uppercase()),
                    ("END", base_range_time.end.to_uppercase()),
                    ("SENDER", sender),
                ],
            );
            output.to_uppercase()
        }
        "GORMAN" => {
            let gorman_start = format!(
                "{} ON {}",
                format_time12(&eas.start_time, false, false).to_uppercase(),
                format_mon_day_year(&eas.start_time, true, true, true)
            );
            let gorman_start = RE_MONTH_DAY_ZERO
                .replace(&gorman_start, "$1 $2")
                .to_string();

            let gorman_end = if is_same_local_day(&eas.start_time, &end_time) {
                format_time12(&end_time, false, false).to_uppercase()
            } else {
                let raw = format!(
                    "{} ON {}",
                    format_time12(&end_time, false, false).to_uppercase(),
                    format_mon_day_year(&end_time, true, true, true)
                );
                RE_MONTH_DAY_ZERO.replace(&raw, "$1 $2").to_string()
            };

            let gorman_locations = fips_context
                .fips_text_with_and
                .iter()
                .map(|entry| {
                    let split = split_location_state(entry);
                    let clean =
                        filter_holly_or_gorman_location(&split.location, true).to_uppercase();
                    if split.state.is_empty() {
                        clean
                    } else {
                        format!("{} {}", clean, split.state.to_uppercase())
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
                .replace(", AND ", " AND ")
                .trim()
                .to_string();

            apply_mode_template(
                "GORMAN",
                &[
                    ("EVENTCODE", normal_event_code.to_uppercase()),
                    (
                        "FIPS",
                        if contains_case_insensitive(&gorman_locations, "All of the United States")
                        {
                            "UNITED STATES".to_string()
                        } else {
                            gorman_locations
                        },
                    ),
                    ("START", gorman_start),
                    ("END", gorman_end),
                    ("SENDER", sender),
                ],
            )
            .to_uppercase()
        }
        _ => {
            let mut locs = fips_context.str_fips.trim_end_matches(';').to_string();
            let locs_arr = RE_LOCS_ARR
                .find_iter(&locs)
                .map(|m| m.as_str().to_string())
                .collect::<Vec<_>>();

            if !locs_arr.is_empty() {
                let expanded = locs_arr
                    .iter()
                    .map(|code| {
                        let mut parts = code.split(',');
                        let location_name = parts.next().unwrap_or_default().trim();
                        let region_abbr = parts.next().unwrap_or_default().trim();
                        let region_name = if canadian_mode {
                            province_name(region_abbr).unwrap_or(region_abbr)
                        } else {
                            state_name(region_abbr).unwrap_or(region_abbr)
                        };
                        format!("{}, {}", location_name, region_name)
                    })
                    .collect::<Vec<_>>();
                locs = expanded.join("; ");
            }

            format!(
                "{} has issued {} for {}; beginning at {} and ending at {}. Message from {}.",
                normal_originator, normal_event_code, locs, start_time_str, end_time_str, sender
            )
        }
    }
}

#[allow(non_snake_case)]
pub fn E2T(
    same_header: &str,
    endec_mode: &str,
    canadian_mode: bool,
    timezone: Option<&str>,
) -> String {
    let Some(parsed) = parse_header(same_header) else {
        return INVALID_HEADER_FORMAT.to_string();
    };

    let timezone_override = explicit_timezone_override(timezone);
    with_timezone_override(timezone_override, || {
        humanize_eas(&parsed, endec_mode, canadian_mode)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid_header() -> &'static str {
        include_str!("../tests/fixtures/eas_valid_header.txt").trim()
    }

    #[test]
    fn parse_header_extracts_core_fields() {
        let parsed = parse_header(valid_header()).expect("parsed header");
        assert_eq!(parsed.originator, "WXR");
        assert_eq!(parsed.event_code, "TOR");
        assert_eq!(parsed.fips_codes, vec!["031055", "031153"]);
        assert_eq!(parsed.duration.hours, 0);
        assert_eq!(parsed.duration.minutes, 30);
        assert_eq!(parsed.sender_id, "KWO35");
        assert_eq!(parsed.start_time.ordinal(), 123);
        assert_eq!(parsed.start_time.hour(), 16);
        assert_eq!(parsed.start_time.minute(), 45);
    }

    #[test]
    fn parse_header_json_serializes_result() {
        let json = parse_header_json(valid_header()).expect("json");
        let value: Value = serde_json::from_str(&json).expect("value");
        assert_eq!(value["originator"], "WXR");
        assert_eq!(value["event_code"], "TOR");
        assert_eq!(value["sender_id"], "KWO35");
    }

    #[test]
    fn parse_header_rejects_invalid_input() {
        let invalid = include_str!("../tests/fixtures/eas_invalid_header.txt").trim();
        assert!(parse_header(invalid).is_none());
        let err = parse_header_json(invalid).expect_err("invalid header should fail");
        assert_eq!(err, "Invalid EAS header format");
    }

    #[test]
    fn e2t_returns_error_for_invalid_header() {
        let text = E2T("not-a-header", "", false, None);
        assert_eq!(text, "Invalid EAS header format");
    }

    #[test]
    fn e2t_generates_humanized_text_for_valid_header() {
        let text = E2T(valid_header(), "", false, Some("UTC"));
        assert!(text.contains("The National Weather Service"));
        assert!(text.to_ascii_lowercase().contains("tornado warning"));
        assert!(text.contains("Message from KWO35"));
    }
}
