use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;

static TZ_MAP: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("CDT", "Central Daylight Time");
    m.insert("CST", "Central Standard Time");
    m.insert("EDT", "Eastern Daylight Time");
    m.insert("EST", "Eastern Standard Time");
    m.insert("MDT", "Mountain Daylight Time");
    m.insert("MST", "Mountain Standard Time");
    m.insert("PDT", "Pacific Daylight Time");
    m.insert("PST", "Pacific Standard Time");
    m.insert("AKDT", "Alaska Daylight Time");
    m.insert("AKST", "Alaska Standard Time");
    m.insert("HST", "Hawaii Standard Time");
    m.insert("UTC", "Coordinated Universal Time");
    m.insert("Z", "Coordinated Universal Time");
    m
});

static LOWER_WORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "a",
        "an",
        "and",
        "as",
        "at",
        "between",
        "but",
        "by",
        "for",
        "from",
        "if",
        "in",
        "into",
        "near",
        "nor",
        "of",
        "off",
        "on",
        "onto",
        "or",
        "out",
        "over",
        "per",
        "the",
        "to",
        "up",
        "via",
        "with",
        "without",
        "north",
        "south",
        "east",
        "west",
        "northeast",
        "northwest",
        "southeast",
        "southwest",
        "northern",
        "southern",
        "eastern",
        "western",
        "northeastern",
        "northwestern",
        "southeastern",
        "southwestern",
        "central",
    ]
    .into_iter()
    .collect()
});

static US_STATES: &[&str] = &[
    "Alabama",
    "Alaska",
    "Arizona",
    "Arkansas",
    "California",
    "Colorado",
    "Connecticut",
    "Delaware",
    "Florida",
    "Georgia",
    "Hawaii",
    "Idaho",
    "Illinois",
    "Indiana",
    "Iowa",
    "Kansas",
    "Kentucky",
    "Louisiana",
    "Maine",
    "Maryland",
    "Massachusetts",
    "Michigan",
    "Minnesota",
    "Mississippi",
    "Missouri",
    "Montana",
    "Nebraska",
    "Nevada",
    "New Hampshire",
    "New Jersey",
    "New Mexico",
    "New York",
    "North Carolina",
    "North Dakota",
    "Ohio",
    "Oklahoma",
    "Oregon",
    "Pennsylvania",
    "Rhode Island",
    "South Carolina",
    "South Dakota",
    "Tennessee",
    "Texas",
    "Utah",
    "Vermont",
    "Virginia",
    "Washington",
    "West Virginia",
    "Wisconsin",
    "Wyoming",
    "District of Columbia",
    "Puerto Rico",
    "Guam",
    "American Samoa",
    "Virgin Islands",
    "Northern Mariana Islands",
];

static PROPER_ALWAYS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut s: HashSet<&'static str> = HashSet::new();
    s.insert("National Weather Service");
    s.insert("Interstate");
    s.insert("Doppler");
    for state in US_STATES {
        s.insert(state);
    }
    s
});

static PLACE_ENDERS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "REFUGE",
        "PARK",
        "AIRPORT",
        "LAKE",
        "RESERVOIR",
        "CREEK",
        "RIVER",
        "BAY",
        "HARBOR",
        "BEACH",
        "MOUNTAIN",
        "MOUNTAINS",
        "HILLS",
        "FOREST",
        "MONUMENT",
        "ISLAND",
        "CANYON",
        "DAM",
    ]
    .into_iter()
    .collect()
});

static RE_PRODUCT_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[A-Z][A-Z /()\-]+(?:WARNING|WATCH|ADVISORY|STATEMENT|EMERGENCY|OUTLOOK)$")
        .unwrap()
});

static RE_HEADER_TIME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\d{3,4}\s+[AP]M\s+[A-Z]{2,4}\s+\w{3}\s+\w{3}\s+\d{1,2}\s+\d{4}$").unwrap()
});

static RE_AWIPS_CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Z]{6}$").unwrap());

static RE_TIME_STRING: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^(\d{1,4})\s*([AP]M)\s*([A-Z]{1,4})?$").unwrap());

static RE_INLINE_TIMES: Lazy<Regex> = Lazy::new(|| {
    let abbrevs: Vec<&str> = TZ_MAP.keys().copied().collect();
    let joined = abbrevs
        .iter()
        .map(|a| regex::escape(a))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"(?i)\b(\d{{1,4}}\s*[AaPp][Mm]\s*(?:{joined}))\b")).unwrap()
});

static RE_MULTI_WORD_ENTITY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+)+)\b").unwrap());

static RE_SINGLE_ENTITY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?:,\s*|\band\s+|\bor\s+|\binclude\s+|\bof\s+|\bover\s+|\bnear\s+)([A-Z][a-z]{2,})\b",
    )
    .unwrap()
});

fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_product_line(text: &str) -> bool {
    RE_PRODUCT_LINE.is_match(text.trim())
}

fn is_header_time(text: &str) -> bool {
    RE_HEADER_TIME.is_match(text.trim())
}

fn titleish(phrase: &str) -> String {
    let phrase = collapse_spaces(phrase);
    let words: Vec<&str> = phrase.split(' ').collect();

    fn transform_token(token: &str) -> String {
        if LOWER_WORDS.contains(token.to_lowercase().as_str()) {
            return token.to_lowercase();
        }
        let mut chars = token.chars();
        match chars.next() {
            Some(first) => {
                let mut out = first.to_uppercase().to_string();
                for ch in chars {
                    out.push(ch.to_lowercase().next().unwrap_or(ch));
                }
                out
            }
            None => String::new(),
        }
    }

    words
        .iter()
        .map(|word| {
            if word.contains('/') {
                word.split('/')
                    .map(|part| transform_token(part))
                    .collect::<Vec<_>>()
                    .join("/")
            } else if word.contains('-') {
                word.split('-')
                    .map(|part| transform_token(part))
                    .collect::<Vec<_>>()
                    .join("-")
            } else {
                transform_token(word)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sentence_case_basic(text: &str) -> String {
    let text = collapse_spaces(text).to_lowercase();
    let mut chars: Vec<char> = text.chars().collect();
    let mut capitalize = true;
    for ch in &mut chars {
        if capitalize && ch.is_ascii_alphabetic() {
            *ch = ch.to_ascii_uppercase();
            capitalize = false;
        }
        if ".!?".contains(*ch) {
            capitalize = true;
        }
    }
    chars.into_iter().collect()
}

fn restore_phrases(text: &str, phrases: &[&str]) -> String {
    let mut sorted: Vec<&&str> = phrases.iter().collect();
    sorted.sort_by(|a, b| b.len().cmp(&a.len()));

    let mut result = text.to_string();
    for phrase in sorted {
        if phrase.is_empty() {
            continue;
        }
        let escaped = regex::escape(phrase);
        let prefix = if phrase.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            r"\b"
        } else {
            ""
        };
        let suffix = if phrase.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
            r"\b"
        } else {
            ""
        };
        let pat = format!("(?i){prefix}{escaped}{suffix}");
        if let Ok(re) = Regex::new(&pat) {
            result = re.replace_all(&result, *phrase).into_owned();
        }
    }
    result
}

fn oxford_join(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let last = &items[items.len() - 1];
            let rest = &items[..items.len() - 1];
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

fn expand_time_string(text: &str) -> String {
    let text = collapse_spaces(text)
        .to_uppercase()
        .trim_end_matches('.')
        .to_string();
    let caps = match RE_TIME_STRING.captures(&text) {
        Some(c) => c,
        None => return titleish(&text),
    };

    let hhmm = &caps[1];
    let ampm = &caps[2];

    let (hour, minute) = match hhmm.len() {
        0..=2 => (hhmm.parse::<u32>().unwrap_or(0), 0u32),
        3 => (
            hhmm[..1].parse::<u32>().unwrap_or(0),
            hhmm[1..].parse::<u32>().unwrap_or(0),
        ),
        _ => (
            hhmm[..hhmm.len() - 2].parse::<u32>().unwrap_or(0),
            hhmm[hhmm.len() - 2..].parse::<u32>().unwrap_or(0),
        ),
    };

    if minute != 0 {
        format!("{hour}:{minute:02} {ampm}")
    } else {
        format!("{hour} {ampm}")
    }
}

fn expand_inline_times(text: &str) -> String {
    RE_INLINE_TIMES
        .replace_all(text, |caps: &regex::Captures| expand_time_string(&caps[0]))
        .into_owned()
}

fn extract_mixed_case_entities(text: &str) -> HashSet<String> {
    if text == text.to_uppercase() {
        return HashSet::new();
    }
    let clean = text.replace("...", " ");
    let mut entities = HashSet::new();

    for m in RE_MULTI_WORD_ENTITY.find_iter(&clean) {
        let candidate = m.as_str();
        let words: Vec<&str> = candidate.split_whitespace().collect();
        if !LOWER_WORDS.contains(words[0].to_lowercase().as_str()) {
            entities.insert(candidate.to_string());
        }
        let trimmed: Vec<&str> = words
            .iter()
            .copied()
            .skip_while(|w| LOWER_WORDS.contains(w.to_lowercase().as_str()))
            .collect();
        if trimmed.len() >= 2 {
            entities.insert(trimmed.join(" "));
        }
    }

    for caps in RE_SINGLE_ENTITY.captures_iter(&clean) {
        let word = &caps[1];
        if !LOWER_WORDS.contains(word.to_lowercase().as_str()) {
            entities.insert(word.to_string());
        }
    }
    entities
}

fn split_area_items(text: &str) -> Vec<String> {
    let raw = collapse_spaces(&text.replace('\n', " "));
    raw.split("...")
        .map(|p| p.trim_matches(|c: char| c == ' ' || c == '.'))
        .filter(|p| !p.is_empty())
        .map(|p| titleish(p))
        .collect()
}

fn split_place_items(text: &str) -> Vec<String> {
    let raw = collapse_spaces(
        &text
            .replace('\n', " ")
            .trim_start_matches(|c: char| c == ' ' || c == '.')
            .trim_end_matches(|c: char| c == ' ' || c == '.')
            .to_string(),
    );
    if raw.is_empty() {
        return Vec::new();
    }

    let chunks: Vec<&str> = raw
        .split("...")
        .map(|c| c.trim_matches(|ch: char| ch == ' ' || ch == '.'))
        .filter(|c| !c.is_empty())
        .collect();

    let enders: Vec<&&str> = PLACE_ENDERS.iter().collect();
    let enders_pat = enders
        .iter()
        .map(|e| regex::escape(e))
        .collect::<Vec<_>>()
        .join("|");
    let split_re = Regex::new(&format!(
        r"(?i)^(.*\b(?:{enders_pat}))\s+AND\s+([A-Z0-9].+)$"
    ))
    .unwrap();

    let mut out = Vec::new();
    for chunk in chunks {
        if let Some(caps) = split_re.captures(chunk) {
            let second = &caps[2];
            if second.split_whitespace().count() >= 2 {
                out.push(caps[1].to_string());
                out.push(second.to_string());
                continue;
            }
        }
        out.push(chunk.to_string());
    }

    out.into_iter().map(|p| titleish(&p)).collect()
}

fn normalize_narrative(text: &str, entities: &HashSet<String>) -> String {
    let mut text = collapse_spaces(text);
    if text.ends_with("...") {
        text.truncate(text.len() - 3);
    }
    let re_near = Regex::new(r"(?i)\b(near|include)\s*\.\.\.").unwrap();
    text = re_near.replace_all(&text, "$1 ").into_owned();
    text = text.replace("...", ", ");
    text = Regex::new(r"\s*,\s*")
        .unwrap()
        .replace_all(&text, ", ")
        .into_owned();
    text = Regex::new(r"\s+\.\s*")
        .unwrap()
        .replace_all(&text, ". ")
        .into_owned();
    text = Regex::new(r"(?i)\b(\d+)\s*MPH\b")
        .unwrap()
        .replace_all(&text, "$1 miles per hour")
        .into_owned();
    text = Regex::new(r"(?i)\bNWS\b")
        .unwrap()
        .replace_all(&text, "National Weather Service")
        .into_owned();

    text = sentence_case_basic(&text);
    text = expand_inline_times(&text);

    let mut phrase_vec: Vec<&str> = PROPER_ALWAYS.iter().copied().collect();
    for e in entities {
        phrase_vec.push(e.as_str());
    }
    text = restore_phrases(&text, &phrase_vec);

    text = Regex::new(r"\s+,")
        .unwrap()
        .replace_all(&text, ",")
        .into_owned();
    text = Regex::new(r"\.\.+")
        .unwrap()
        .replace_all(&text, ".")
        .into_owned();
    text
}

#[derive(Debug)]
enum SegmentKind {
    Para,
    Bullet,
    Section,
}

#[derive(Debug)]
struct Segment {
    kind: SegmentKind,
    lines: Vec<String>,
}

fn strip_metadata(raw: &str) -> Vec<String> {
    let text = raw
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_start_matches('\u{FEFF}')
        .to_string();
    let lines: Vec<String> = text.split('\n').map(|l| l.trim_end().to_string()).collect();

    let mut start = 0;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if Regex::new(r"(?i)^(BULLETIN\s*-|URGENT\s*-|WATCH COUNTY NOTIFICATION)")
            .unwrap()
            .is_match(stripped)
            || is_product_line(stripped)
            || stripped.starts_with("THE NATIONAL WEATHER SERVICE")
        {
            start = i;
            break;
        }
    }

    let trimmed = &lines[start..];
    let mut out = Vec::new();
    for line in trimmed {
        let stripped = line.trim();
        if stripped == "&&" || stripped == "$$" {
            break;
        }
        if Regex::new(r"^(LAT\.\.\.LON|TIME\.\.\.MOT\.\.\.LOC|TIME\.\.\.)")
            .unwrap()
            .is_match(stripped)
        {
            break;
        }
        if Regex::new(r"^[A-Z]{2,}(?:/[A-Z]{2,})+$")
            .unwrap()
            .is_match(stripped)
        {
            break;
        }
        if RE_AWIPS_CODE.is_match(stripped) {
            continue;
        }
        out.push(line.clone());
    }
    out
}

fn parse_segments(lines: &[String]) -> (BulletinHeader, Vec<Segment>) {
    let mut i = 0;
    let mut header = BulletinHeader::default();

    if i < lines.len()
        && Regex::new(r"(?i)^BULLETIN\s*-")
            .unwrap()
            .is_match(lines[i].trim())
    {
        header.broadcast = Some(lines[i].trim().to_string());
        i += 1;
    }
    if i < lines.len() && is_product_line(lines[i].trim()) {
        header.product = Some(lines[i].trim().to_string());
        i += 1;
    }
    if i < lines.len()
        && lines[i]
            .trim()
            .to_uppercase()
            .starts_with("NATIONAL WEATHER SERVICE")
    {
        header.office = Some(lines[i].trim().to_string());
        i += 1;
    }
    if i < lines.len() && is_header_time(lines[i].trim()) {
        header.issued_at = Some(lines[i].trim().to_string());
        i += 1;
    }

    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut current: Option<Segment> = None;

    while i < lines.len() {
        let stripped = lines[i].trim().to_string();
        i += 1;

        if stripped.is_empty() {
            if let Some(seg) = current.take() {
                segments.push(seg);
            }
            continue;
        }

        if let Some(rest) = stripped.strip_prefix("* ") {
            if let Some(seg) = current.take() {
                segments.push(seg);
            }
            current = Some(Segment {
                kind: SegmentKind::Bullet,
                lines: vec![rest.to_string()],
            });
            continue;
        }

        if matches!(&current, Some(seg) if matches!(seg.kind, SegmentKind::Bullet)) {
            current.as_mut().unwrap().lines.push(stripped);
            continue;
        }

        if Regex::new(r"^[A-Z/ ]+\.\.\.$").unwrap().is_match(&stripped) {
            if let Some(seg) = current.take() {
                segments.push(seg);
            }
            current = Some(Segment {
                kind: SegmentKind::Section,
                lines: vec![stripped],
            });
            continue;
        }

        match &mut current {
            None => {
                current = Some(Segment {
                    kind: SegmentKind::Para,
                    lines: vec![stripped],
                });
            }
            Some(seg) => {
                seg.lines.push(stripped);
            }
        }
    }

    if let Some(seg) = current.take() {
        segments.push(seg);
    }

    (header, segments)
}

#[derive(Default)]
struct BulletinHeader {
    broadcast: Option<String>,
    product: Option<String>,
    office: Option<String>,
    issued_at: Option<String>,
}

pub struct NormalizeOptions {
    pub repeat: bool,
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self { repeat: false }
    }
}

pub fn normalize_nws_bulletin(raw: &str, options: &NormalizeOptions) -> String {
    let lines = strip_metadata(raw);
    let (header, segments) = parse_segments(&lines);

    let mut entities: HashSet<String> = PROPER_ALWAYS.iter().map(|s| s.to_string()).collect();
    let mut product: Option<String> = header.product.as_deref().map(titleish);
    let mut intro_city: Option<String> = None;
    let mut intro_action: Option<String> = None;
    let mut warning_for: Option<String> = None;
    let mut area_items: Vec<String> = Vec::new();
    let mut until: Option<String> = None;
    let mut output: Vec<String> = Vec::new();

    let re_intro = Regex::new(
        r"(?i)^THE NATIONAL WEATHER SERVICE IN (.+?) HAS (ISSUED A|EXTENDED THE|CONTINUED THE|CANCELLED THE|ALLOWED THE|REISSUED THE)\s*$",
    )
    .unwrap();
    let re_warning_for = Regex::new(r"(?i)^([A-Za-z /()\-]+?)\s+FOR\.\.\.(.*)$").unwrap();
    let re_until = Regex::new(r"(?i)^UNTIL\s+(.+)$").unwrap();
    let re_at_ellipsis = Regex::new(r"(?i)^AT\s+(.+?)\.\.\.(.*)$").unwrap();
    let re_at_time =
        Regex::new(r"(?i)^AT\s+(\d{1,4}\s*[AP]M\s*[A-Z]{2,4})\b[,.\s]\s*(.*)$").unwrap();
    let re_locations = Regex::new(r"(?i)^LOCATIONS IMPACTED INCLUDE\.\.\.(.*)$").unwrap();
    let re_key_value = Regex::new(r"^([A-Z][A-Z /\-]{1,40})\.\.\.(.*)$").unwrap();
    let re_place_near = Regex::new(
        r"(?i)\b(?:OF|NEAR)\s+([A-Z][A-Z0-9 /\-]{1,60}?)(?:\.\.\.|,| AND | OR | MOVING |$)",
    )
    .unwrap();

    for segment in &segments {
        let text = collapse_spaces(&segment.lines.join(" "));
        for e in extract_mixed_case_entities(&text) {
            entities.insert(e);
        }

        if matches!(segment.kind, SegmentKind::Para)
            && Regex::new(r"(?i)^THE NATIONAL WEATHER SERVICE IN ")
                .unwrap()
                .is_match(&text)
        {
            if let Some(caps) = re_intro.captures(&text) {
                intro_city = Some(titleish(&caps[1]));
                intro_action = Some(caps[2].to_lowercase());
                if let Some(city) = &intro_city {
                    entities.insert(city.clone());
                }
            } else {
                let mut sentence = normalize_narrative(&text, &entities);
                if !sentence.ends_with(|c: char| ".!?".contains(c)) {
                    sentence.push('.');
                }
                output.push(sentence);
            }
            continue;
        }

        if matches!(segment.kind, SegmentKind::Bullet) {
            if let Some(caps) = re_warning_for.captures(&text) {
                if product.is_none() {
                    product = Some(titleish(&caps[1]));
                }
                let items_text = if segment.lines.len() > 1 {
                    segment.lines[1..].join(" ")
                } else {
                    caps[2].to_string()
                };
                let items = split_area_items(&items_text);
                for it in &items {
                    entities.insert(it.clone());
                }
                warning_for = Some(oxford_join(&items));
                area_items = items;
                continue;
            }

            if let Some(caps) = re_until.captures(&text) {
                until = Some(expand_time_string(&caps[1].replace("...", " ")));
                continue;
            }

            let at_caps = re_at_ellipsis
                .captures(&text)
                .or_else(|| re_at_time.captures(&text));
            if let Some(caps) = at_caps {
                let at_time = expand_time_string(&caps[1]);
                let narrative = caps[2].trim().to_string();
                if narrative == narrative.to_uppercase() {
                    for caps in re_place_near.captures_iter(&narrative) {
                        entities.insert(titleish(&caps[1]));
                    }
                }
                let mut sentence = normalize_narrative(&narrative, &entities);
                if !sentence.is_empty()
                    && !entities.iter().any(|p| {
                        p.len() > 1 && sentence.to_lowercase().starts_with(&p.to_lowercase())
                    })
                {
                    let mut chars = sentence.chars();
                    if let Some(first) = chars.next() {
                        sentence = first.to_lowercase().to_string() + chars.as_str();
                    }
                }
                if !sentence.ends_with(|c: char| ".!?".contains(c)) {
                    sentence.push('.');
                }
                output.push(format!("At {at_time}, {sentence}"));
                continue;
            }

            if let Some(caps) = re_locations.captures(&text) {
                let items_text = if segment.lines.len() > 1 {
                    segment.lines[1..].join(" ")
                } else {
                    caps[1].to_string()
                };
                let items = split_place_items(&items_text);
                for it in &items {
                    entities.insert(it.clone());
                }
                output.push(format!(
                    "Locations impacted include: {}.",
                    oxford_join(&items)
                ));
                continue;
            }

            if let Some(caps) = re_key_value.captures(&text) {
                let raw_label = &caps[1];
                let label = if raw_label.split_whitespace().count() > 4 {
                    sentence_case_basic(raw_label)
                } else {
                    titleish(raw_label)
                };
                let value_text = if segment.lines.len() > 1 {
                    segment.lines[1..].join(" ")
                } else {
                    caps[2].trim().to_string()
                };
                output.push(format!(
                    "{}: {}.",
                    label,
                    normalize_narrative(&value_text, &entities)
                ));
                continue;
            }

            let mut sentence = normalize_narrative(&text, &entities);
            if !sentence.ends_with(|c: char| ".!?".contains(c)) {
                sentence.push('.');
            }
            output.push(sentence);
            continue;
        }

        if matches!(segment.kind, SegmentKind::Section) {
            continue;
        }

        if matches!(segment.kind, SegmentKind::Para) {
            let trimmed = text.trim();
            if trimmed.starts_with("...") && trimmed.ends_with("...") {
                continue;
            }
            let mut sentence = normalize_narrative(&text, &entities);
            if !sentence.ends_with(|c: char| ".!?".contains(c)) {
                sentence.push('.');
            }
            output.push(sentence);
        }
    }

    match (&intro_city, &intro_action, &product, &warning_for) {
        (Some(city), Some(action), Some(prod), Some(wf)) => {
            let mut first =
                format!("The National Weather Service in {city} has {action} {prod} for {wf}");
            if let Some(u) = &until {
                first.push_str(&format!(" until {u}"));
            }
            first.push('.');
            output.insert(0, first);
        }
        (Some(city), Some(action), _, _) => {
            let mut first = format!("The National Weather Service in {city} has {action}");
            if let Some(u) = &until {
                first.push_str(&format!(" until {u}"));
            }
            first.push('.');
            output.insert(0, first);
        }
        (_, _, Some(prod), Some(wf)) => {
            let mut first = format!("{prod} for {wf}");
            if let Some(u) = &until {
                first.push_str(&format!(" until {u}"));
            }
            first.push('.');
            output.insert(0, first);
        }
        _ => {
            if let Some(u) = &until {
                output.insert(0, format!("Until {u}."));
            }
        }
    }

    if let (Some(prod), Some(u)) = (&product, &until) {
        if options.repeat && !area_items.is_empty() {
            let short: Vec<String> = area_items
                .iter()
                .map(|item| {
                    Regex::new(r"(?i)\s+in\s+(?:\w+\s+)*\w+$")
                        .unwrap()
                        .replace(item, "")
                        .into_owned()
                })
                .collect();
            output.push(format!(
                "Repeating, a {prod} has been issued for {} until {u}.",
                oxford_join(&short)
            ));
        }
    }

    collapse_spaces(&output.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titleish_basic() {
        assert_eq!(titleish("TORNADO WARNING"), "Tornado Warning");
        assert_eq!(titleish("south of the river"), "south of the River");
    }

    #[test]
    fn sentence_case_basic_test() {
        assert_eq!(
            sentence_case_basic("THIS IS A TEST. AND ANOTHER."),
            "This is a test. And another."
        );
    }

    #[test]
    fn expand_time_1215pm() {
        assert_eq!(expand_time_string("1215 PM CDT"), "12:15 PM");
    }

    #[test]
    fn expand_time_7pm() {
        assert_eq!(expand_time_string("7 PM"), "7 PM");
    }

    #[test]
    fn oxford_join_variants() {
        assert_eq!(oxford_join(&[]), "");
        assert_eq!(oxford_join(&["Alpha".into()]), "Alpha");
        assert_eq!(
            oxford_join(&["Alpha".into(), "Beta".into()]),
            "Alpha and Beta"
        );
        assert_eq!(
            oxford_join(&["A".into(), "B".into(), "C".into()]),
            "A, B, and C"
        );
    }

    #[test]
    fn normalize_tornado_bulletin() {
        let raw = "\
WFUS53 KOAX 262041

TOROAX

BULLETIN - EAS ACTIVATION REQUESTED
TORNADO WARNING
NATIONAL WEATHER SERVICE OMAHA/VALLEY NE
341 PM CDT FRI APR 26 2024

THE NATIONAL WEATHER SERVICE IN OMAHA/VALLEY HAS ISSUED A

* TORNADO WARNING FOR...
  SOUTHEASTERN WASHINGTON COUNTY IN EAST CENTRAL NEBRASKA

* UNTIL 415 PM CDT

* AT 341 PM CDT...A CONFIRMED TORNADO WAS LOCATED NEAR FORT
  CALHOUN...MOVING NORTHEAST AT 30 MPH.

* LOCATIONS IMPACTED INCLUDE...
  FORT CALHOUN...BLAIR...ARLINGTON

&&
$$";
        let result = normalize_nws_bulletin(raw, &NormalizeOptions::default());
        assert!(result.starts_with(
            "The National Weather Service in Omaha/Valley has issued a Tornado Warning"
        ));
        assert!(result.contains("until 4:15 PM"));
        assert!(result.contains("30 miles per hour"));
        assert!(result.contains("Fort Calhoun"));
    }
}
