//! The SAGE 3644's HelloTTS text rewrite, for the Loquendo engine in SAGE ENDEC mode.
//!
//! HelloTTS sits between the ENDEC's user interface and Loquendo: it takes the sentence the UI
//! wrote and reworks it for speech before `ttsRead`. This follows the Rev96 binary
//! (`B417441_rev96/.../loq/HelloTTS`, built Jun 6 2023) routine by routine, not the rules as
//! summarised, because the summary leaves out what the code actually does with them: which text it
//! touches at all (`classify`, @0xb5e0), and that the parenthesised sender after the end time -- the
//! station's call sign -- is cut rather than spoken. Tables are its `.rodata`, byte for byte.
//!
//! Not reproduced: the device's `M` IPC opcode, which asks for a message to be spoken as it stands
//! (pause only). Its own user interface never sends it.

const PAUSE: &str = "\\p1000 !";

/// @0x15568. Rev96 reads "The United States Government has" where 89-30 had "A Primary Entry
/// Point System has".
const PREAMBLES: [&str; 5] = [
    "An EAS Participant has",
    "The Civil Authorities have",
    "The National Weather Service has",
    "The United States Government has",
    "The Emergency Action Notification Network has",
];

/// @0x150a8 joined to @0x15308 by SAME state number. States and DC are spelled out letter by letter
/// with a trailing comma; territories and marine areas are named, with no leading space.
const STATES: &[(&str, &str)] = &[
    (", AL", " A.L,"),
    (", AK", " A.K,"),
    (", AZ", " A.Z,"),
    (", AR", " A.R,"),
    (", CA", " C.A,"),
    (", CO", " C.O,"),
    (", CT", " C.T,"),
    (", DE", " D.E,"),
    (", DC", " D.C,"),
    (", FL", " F.L,"),
    (", GA", " G.A,"),
    (", HI", " H.I,"),
    (", ID", " I.D,"),
    (", IL", " I.L,"),
    (", IN", " I.N,"),
    (", IA", " I.A,"),
    (", KS", " K.S,"),
    (", KY", " K.Y,"),
    (", LA", " L.A,"),
    (", ME", " M.E,"),
    (", MD", " M.D,"),
    (", MA", " M.A,"),
    (", MI", " M.I,"),
    (", MN", " M.N,"),
    (", MS", " M.S,"),
    (", MO", " M.O,"),
    (", MT", " M.T,"),
    (", NE", " N.E,"),
    (", NV", " N.V,"),
    (", NH", " N.H,"),
    (", NJ", " N.J,"),
    (", NM", " N.M,"),
    (", NY", " N.Y,"),
    (", NC", " N.C,"),
    (", ND", " N.D,"),
    (", OH", " O.H,"),
    (", OK", " O.K,"),
    (", OR", " O.R,"),
    (", PA", " P.A,"),
    (", RI", " R.I,"),
    (", SC", " S.C,"),
    (", SD", " S.D,"),
    (", TN", " T.N,"),
    (", TX", " T.X,"),
    (", UT", " U.T,"),
    (", VT", " V.T,"),
    (", VA", " V.A,"),
    (", WA", " W.A,"),
    (", WV", " W.V,"),
    (", WI", " W.I,"),
    (", WY", " W.Y,"),
    (", AS", "American Samoa"),
    (", FM", "Micronesia"),
    (", GU", "Guam"),
    (", MH", "Marshall Islands"),
    (", MP", "Northern Mariana Is."),
    (", PW", "Palau"),
    (", PR", "Puerto Rico"),
    (", UM", "Minor Outlying Is."),
    (", VI", "Virgin Islands"),
    (", PZ", "Eastern N. Pacific Ocean"),
    (", PK", "N. Pacific Ocean Near Alaska"),
    (", PH", "Central Pacific Ocean"),
    (", PS", "S. Central Pacific Ocean"),
    (", PM", "Western Pacific Ocean"),
    (", AN", "Northwest N. Atlantic Ocean"),
    (", AM", "West N. Atlantic Ocean"),
    (", GM", "Gulf Of Mexico"),
    (", LS", "Lake Superior"),
    (", LM", "Lake Michigan"),
    (", LH", "Lake Huron"),
    (", LC", "Lake St. Clair"),
    (", LE", "Lake Erie"),
    (", LO", "Lake Ontario"),
    (", SL", "St. Lawrence River"),
];

/// @0x15010 and @0x1502c.
const WEEKDAYS: &[(&str, &str)] = &[
    ("Mon ", "Monday "),
    ("Tue ", "Tuesday "),
    ("Wed ", "Wednesday "),
    ("Thu ", "Thursday "),
    ("Fri ", "Friday "),
    ("Sat ", "Saturday "),
    ("Sun ", "Sunday "),
];

/// @0x15048 and @0x15078. "May " maps to itself.
const MONTHS: &[(&str, &str)] = &[
    ("Jan ", "January "),
    ("Feb ", "February "),
    ("Mar ", "March "),
    ("Apr ", "April "),
    ("May ", "May "),
    ("Jun ", "June "),
    ("Jul ", "July "),
    ("Aug ", "August "),
    ("Sep ", "September "),
    ("Oct ", "October "),
    ("Nov ", "November "),
    ("Dec ", "December "),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Passed to Loquendo exactly as given, with no pause in front.
    Other,
    /// The ENDEC's own SAME sentence: a known preamble, then "beginning at", then "and ending
    /// at", then "(" for the sender.
    Endec,
    /// Text built from a CAP message, which the UI marks "(fromcap)".
    Cap,
}

/// @0xb5e0.
fn classify(text: &str) -> Kind {
    if PREAMBLES.iter().any(|preamble| text.starts_with(preamble)) {
        let endec = text.find("beginning at").is_some_and(|begin| {
            text[begin..]
                .find("and ending at")
                .is_some_and(|end| text[begin + end..].contains('('))
        });
        if endec {
            return Kind::Endec;
        }
    }
    if text.contains("(fromcap)") {
        Kind::Cap
    } else {
        Kind::Other
    }
}

/// @0xb1b4.
///
/// The routine also swaps "An EAS Participant has" for "An E.A.S. participant) has", but compares
/// against the buffer after the pause has already been put in front, so the swap can never happen
/// and is left out here too.
pub fn rewrite(text: &str) -> String {
    let kind = classify(text);
    if kind == Kind::Other {
        return text.to_string();
    }

    let spoken = format!("{PAUSE}{text}");
    match kind {
        Kind::Endec => match rewrite_endec(&spoken) {
            Some(rewritten) => strip_parenthetical(&rewritten, "and ending at"),
            None => spoken,
        },
        _ => strip_parenthetical(&spoken, "until "),
    }
}

fn find_from(text: &str, from: usize, needle: char) -> Option<usize> {
    text.get(from..)?.find(needle).map(|index| from + index)
}

/// @0xb0dc and @0xb148: the four bytes at `at` expanded if the table knows them, and passed
/// through as they are if not.
fn expand(text: &str, at: usize, table: &[(&str, &'static str)]) -> String {
    let word = text.get(at..(at + 4).min(text.len())).unwrap_or_default();
    table
        .iter()
        .find(|(short, _)| *short == word)
        .map(|(_, long)| (*long).to_string())
        .unwrap_or_else(|| word.to_string())
}

/// The part of @0xb1b4 that rewrites the SAME sentence between " issued" and the times. `None`
/// when the sentence is not shaped for it, in which case the device speaks it as it came, sender
/// and all.
fn rewrite_endec(text: &str) -> Option<String> {
    let issued = text.find(" issued")?;
    let begin = text.find("beginning at")?;
    if issued >= begin {
        return None;
    }

    let mut out = text[..issued].to_string();
    let mut cursor = issued;
    let rest = |out: &mut String, from: usize| out.push_str(text.get(from..).unwrap_or_default());

    while let Some(comma) = find_from(text, cursor, ',').filter(|comma| *comma < begin) {
        out.push_str(&text[cursor..comma]);
        match STATES
            .iter()
            .find(|(code, _)| text.get(comma..comma + 4) == Some(*code))
        {
            Some((_, spoken)) => {
                out.push_str(spoken);
                cursor = comma + 4;
            }
            None => {
                out.push(',');
                cursor = comma + 1;
            }
        }
    }

    if begin <= cursor {
        rest(&mut out, cursor);
        return Some(out);
    }
    out.push_str(&text[cursor..begin]);
    out.push_str("beginning \\fAe-t");
    out.push(' ');
    // Past "beginning at" and the space after it.
    let mut cursor = begin + 13;

    // The start time is two words.
    let Some(end_of_time) =
        find_from(text, cursor, ' ').and_then(|space| find_from(text, space + 1, ' '))
    else {
        rest(&mut out, cursor);
        return Some(out);
    };
    out.push_str(&text[cursor..=end_of_time]);
    cursor = end_of_time + 1;

    // A date only follows the start time when the alert ends on another day.
    if text
        .get(cursor..)
        .is_some_and(|tail| tail.starts_with("and "))
    {
        rest(&mut out, cursor);
        return Some(out);
    }
    out.push_str(&expand(text, cursor, WEEKDAYS));
    cursor += 4;
    out.push_str(&expand(text, cursor, MONTHS));
    cursor += 4;

    // "DD and ending at HH:MM am " is six words to the end date.
    let mut space = Some(cursor);
    for _ in 0..6 {
        space = space.and_then(|at| find_from(text, at + 1, ' '));
    }
    let Some(space) = space else {
        rest(&mut out, cursor);
        return Some(out);
    };
    out.push_str(&text[cursor..=space]);
    cursor = space + 1;
    out.push_str(&expand(text, cursor, WEEKDAYS));
    cursor += 4;
    out.push_str(&expand(text, cursor, MONTHS));
    cursor += 4;
    rest(&mut out, cursor);
    Some(out)
}

/// The tail of @0xb1b4: drops the first parenthesis after `marker`, and the space before it, as
/// long as it closes within ten characters -- the sender after the end time, or "(fromcap)".
fn strip_parenthetical(text: &str, marker: &str) -> String {
    let Some(found) = text.find(marker) else {
        return text.to_string();
    };
    // The device does not check for this and would read past the end of the string.
    let Some(open) = find_from(text, found, '(') else {
        return text.to_string();
    };
    let start = if text[..open].ends_with(' ') {
        open - 1
    } else {
        open
    };

    let tail = &text.as_bytes()[start..];
    if tail.len() <= 1 {
        return text.to_string();
    }
    let reach = tail.len().min(10);
    match (0..=reach)
        .rev()
        .find(|&index| tail.get(index) == Some(&b')'))
    {
        Some(close) => format!("{}{}", &text[..start], &text[start + close + 1..]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_same_day_warning_is_paused_spelled_and_loses_its_sender() {
        assert_eq!(
            rewrite(
                "The National Weather Service has issued a Tornado Warning for Douglas, NE, \
                 Sarpy, NE beginning at 10:15 am and ending at 11:15 am (KWO35)"
            ),
            "\\p1000 !The National Weather Service has issued a Tornado Warning for Douglas \
             N.E,, Sarpy N.E, beginning \\fAe-t 10:15 am and ending at 11:15 am"
        );
    }

    #[test]
    fn dates_are_spelled_out_on_both_sides_when_the_alert_spans_days() {
        assert_eq!(
            rewrite(
                "The Civil Authorities have issued a Civil Emergency Message for Lancaster, NE \
                 beginning at 11:30 pm Mon Jan 05 and ending at 01:30 am Tue May 06 (EASLSTNR)"
            ),
            "\\p1000 !The Civil Authorities have issued a Civil Emergency Message for Lancaster \
             N.E, beginning \\fAe-t 11:30 pm Monday January 05 and ending at 01:30 am Tuesday \
             May 06"
        );
    }

    #[test]
    fn territories_and_marine_areas_are_named_rather_than_spelled() {
        assert_eq!(
            rewrite(
                "The National Weather Service has issued a Special Marine Warning for Lake \
                 Superior West, LS beginning at 02:00 pm and ending at 03:00 pm (KDLH)"
            ),
            "\\p1000 !The National Weather Service has issued a Special Marine Warning for Lake \
             Superior WestLake Superior beginning \\fAe-t 02:00 pm and ending at 03:00 pm"
        );
    }

    #[test]
    fn text_it_does_not_recognise_is_left_exactly_as_it_is() {
        for text in [
            "Environment Canada has issued a Tornado Warning for Toronto beginning at 10:15 am \
             and ending at 11:15 am (NAADSCAP)",
            // No sender to strip, so it is not the ENDEC's sentence.
            "The National Weather Service has issued a Tornado Warning for Douglas, NE \
             beginning at 10:15 am and ending at 11:15 am",
            // 89-30's PEP preamble, which Rev96's table no longer has.
            "A Primary Entry Point System has issued a National Periodic Test for all of the \
             United States beginning at 10:15 am and ending at 11:15 am (WAGS)",
        ] {
            assert_eq!(rewrite(text), text);
        }
    }

    #[test]
    fn cap_text_is_paused_and_loses_its_marker() {
        assert_eq!(
            rewrite("Tornado Warning in effect until 5:00 PM (fromcap). Take shelter now."),
            "\\p1000 !Tornado Warning in effect until 5:00 PM. Take shelter now."
        );
    }

    /// What E2T writes in SAGE mode has to be what HelloTTS recognises, for every originator, or
    /// none of this runs.
    #[test]
    fn e2t_s_sage_sentence_is_the_one_it_rewrites() {
        for (originator, event, preamble) in [
            ("WXR", "TOR", "The National Weather Service has"),
            ("CIV", "CEM", "The Civil Authorities have"),
            ("EAS", "ADR", "An EAS Participant has"),
            ("PEP", "NPT", "The United States Government has"),
            (
                "EAN",
                "EAN",
                "The Emergency Action Notification Network has",
            ),
        ] {
            let header = format!("ZCZC-{originator}-{event}-031055-031153+0030-2621515-KWO35-");
            let sage = crate::e2t_ng::E2T(&header, "SAGE", false, Some("America/Chicago"));
            assert!(sage.starts_with(preamble), "{originator}: {sage}");

            let spoken = rewrite(&sage);
            assert!(
                spoken.starts_with(&format!("{PAUSE}{preamble}")),
                "{spoken}"
            );
            assert!(spoken.contains("beginning \\fAe-t "), "{spoken}");
            assert!(spoken.contains(" N.E,"), "{spoken}");
            assert!(!spoken.contains("KWO35"), "{spoken}");
            // The E.A.S. respelling is dead code on the device.
            assert!(!spoken.contains("E.A.S."), "{spoken}");
        }
    }

    #[test]
    fn a_sender_too_long_to_close_within_ten_characters_is_kept() {
        let text = "The National Weather Service has issued a Tornado Warning for Douglas, NE \
                    beginning at 10:15 am and ending at 11:15 am (A-VERY-LONG-SENDER)";
        assert!(rewrite(text).ends_with("11:15 am (A-VERY-LONG-SENDER)"));
    }
}
