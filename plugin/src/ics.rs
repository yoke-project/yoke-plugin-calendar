//! iCalendar read and expanded: the events of a text (RFC 5545), and their occurrences in a window, every
//! recurrence expanded in the event's own time zone and every instance moved or cancelled applied.
//!
//! A time with a zone is read in it: an IANA name, or one a `VTIMEZONE` names by `X-LIC-LOCATION`, or a
//! Windows name the table below knows. A time with no zone, and a zone none of these resolves, is read in the
//! unit's zone.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rrule::{RRule, RRuleSet, Unvalidated};

/// When something happens, as the text says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// A whole day, in the unit's zone.
    Date(NaiveDate),
    /// A wall-clock time in a zone.
    Local(NaiveDateTime, Tz),
    Utc(DateTime<Utc>),
}

impl When {
    /// The instant, a whole day starting at midnight in `zone`.
    pub fn instant(&self, zone: Tz) -> DateTime<Utc> {
        match self {
            When::Date(d) => local(zone, d.and_hms_opt(0, 0, 0).unwrap()),
            When::Local(n, tz) => local(*tz, *n),
            When::Utc(t) => *t,
        }
    }

    fn rrule(&self, zone: Tz) -> DateTime<rrule::Tz> {
        match self {
            When::Date(d) => rrule_local(zone, d.and_hms_opt(0, 0, 0).unwrap()),
            When::Local(n, tz) => rrule_local(*tz, *n),
            When::Utc(t) => t.with_timezone(&rrule::Tz::UTC),
        }
    }
}

/// A wall-clock time in a zone; in a gap of a change of offset, the instant after it.
fn local(tz: Tz, n: NaiveDateTime) -> DateTime<Utc> {
    tz.from_local_datetime(&n)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(n + Duration::hours(1))).earliest())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&n))
}

fn rrule_local(tz: Tz, n: NaiveDateTime) -> DateTime<rrule::Tz> {
    local(tz, n).with_timezone(&rrule::Tz::Tz(tz))
}

/// One `VEVENT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub location: Option<String>,
    pub start: Option<When>,
    pub end: Option<When>,
    pub duration: Option<Duration>,
    pub rrules: Vec<String>,
    pub rdates: Vec<When>,
    pub exdates: Vec<When>,
    /// Set on an instance moved or cancelled: which instance of the series it replaces.
    pub recurrence_id: Option<When>,
    pub cancelled: bool,
}

/// An occurrence of an event: one instance, in a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// `<source>/<uid>/<the instance's original start, in ms>`: the same across reads.
    pub id: String,
    pub source: String,
    pub uid: String,
    pub title: String,
    pub location: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub recurring: bool,
}

/// The events of a text, and how many components could not be read. `zone` is the unit's.
pub fn parse(text: &str, zone: Tz) -> Result<(Vec<Event>, usize), String> {
    let lines = unfold(text);
    if !lines
        .iter()
        .any(|l| l.eq_ignore_ascii_case("BEGIN:VCALENDAR"))
    {
        return Err("it is not iCalendar: no BEGIN:VCALENDAR".into());
    }
    // The zones a VTIMEZONE names by a location, read before any event that uses them.
    let mut zones: BTreeMap<String, Tz> = BTreeMap::new();
    let mut tzid: Option<String> = None;
    for line in &lines {
        let p = property(line);
        match (p.name.as_str(), p.value.as_str()) {
            ("BEGIN", v) if v.eq_ignore_ascii_case("VTIMEZONE") => tzid = None,
            ("TZID", v) => tzid = Some(v.to_string()),
            ("X-LIC-LOCATION", v) => {
                if let (Some(id), Ok(tz)) = (&tzid, v.parse::<Tz>()) {
                    zones.insert(id.clone(), tz);
                }
            }
            _ => {}
        }
    }
    let resolve = |id: Option<&String>| -> Tz {
        id.and_then(|id| {
            zones.get(id).copied().or_else(|| {
                let id = id.trim_start_matches('/');
                id.parse::<Tz>().ok().or_else(|| windows(id))
            })
        })
        .unwrap_or(zone)
    };

    let (mut events, mut unread) = (Vec::new(), 0);
    let mut current: Option<Event> = None;
    let mut depth = 0usize; // components nested inside the VEVENT, such as VALARM
    for line in &lines {
        let p = property(line);
        if let Some(e) = current.as_mut() {
            match p.name.as_str() {
                "BEGIN" => depth += 1,
                "END" if depth > 0 => depth -= 1,
                "END" if p.value.eq_ignore_ascii_case("VEVENT") => {
                    let e = current.take().unwrap();
                    if e.start.is_some() && !e.uid.is_empty() {
                        events.push(e);
                    } else {
                        unread += 1;
                    }
                }
                _ if depth > 0 => {}
                name => {
                    let tz = resolve(p.params.get("TZID"));
                    let date = p
                        .params
                        .get("VALUE")
                        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
                    let ok = match name {
                        "UID" => {
                            e.uid = p.value.clone();
                            true
                        }
                        "SUMMARY" => {
                            e.summary = text_value(&p.value);
                            true
                        }
                        "LOCATION" => {
                            e.location = Some(text_value(&p.value)).filter(|l| !l.is_empty());
                            true
                        }
                        "DTSTART" => when(&p.value, date, tz)
                            .map(|w| e.start = Some(w))
                            .is_some(),
                        "DTEND" => when(&p.value, date, tz).map(|w| e.end = Some(w)).is_some(),
                        "DURATION" => duration(&p.value).map(|d| e.duration = Some(d)).is_some(),
                        "RRULE" => {
                            e.rrules.push(p.value.clone());
                            true
                        }
                        "RDATE" | "EXDATE" => {
                            let list: Option<Vec<When>> = p
                                .value
                                .split(',')
                                .map(|v| when(v.split('/').next().unwrap_or(v), date, tz))
                                .collect();
                            match list {
                                Some(l) if name == "RDATE" => {
                                    e.rdates.extend(l);
                                    true
                                }
                                Some(l) => {
                                    e.exdates.extend(l);
                                    true
                                }
                                None => false,
                            }
                        }
                        "RECURRENCE-ID" => when(&p.value, date, tz)
                            .map(|w| e.recurrence_id = Some(w))
                            .is_some(),
                        "STATUS" => {
                            e.cancelled = p.value.eq_ignore_ascii_case("CANCELLED");
                            true
                        }
                        _ => true,
                    };
                    if !ok {
                        // A time the event cannot be placed by: the event is not read.
                        e.uid.clear();
                        e.start = None;
                    }
                }
            }
        } else if p.name == "BEGIN" && p.value.eq_ignore_ascii_case("VEVENT") {
            current = Some(Event::default());
            depth = 0;
        }
    }
    Ok((events, unread))
}

/// Every occurrence of `events` overlapping `[from, to)`, in start order. An event whose recurrence cannot
/// be read is counted, and its first instance kept.
pub fn expand(
    events: &[Event],
    source: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    zone: Tz,
) -> (Vec<Occurrence>, usize) {
    let (mut out, mut unread) = (Vec::new(), 0);
    let mut moved: BTreeMap<&str, Vec<DateTime<Utc>>> = BTreeMap::new();
    for e in events {
        if let Some(r) = &e.recurrence_id {
            moved
                .entry(e.uid.as_str())
                .or_default()
                .push(r.instant(zone));
        }
    }
    let masters: BTreeMap<&str, &Event> = events
        .iter()
        .filter(|e| e.recurrence_id.is_none())
        .map(|e| (e.uid.as_str(), e))
        .collect();
    for e in events {
        let Some(start) = &e.start else { continue };
        let all_day = matches!(start, When::Date(_));
        let length = span(e, zone);
        let recurring = e.recurrence_id.is_some() || !e.rrules.is_empty() || !e.rdates.is_empty();
        let place = |original: DateTime<Utc>, begins: DateTime<Utc>, out: &mut Vec<Occurrence>| {
            let ends = begins + length;
            if begins < to && (ends > from || (ends == begins && begins >= from)) {
                // An instance moved keeps the series' title where its own says none.
                let title = if e.summary.is_empty() && e.recurrence_id.is_some() {
                    masters
                        .get(e.uid.as_str())
                        .map(|m| m.summary.clone())
                        .unwrap_or_default()
                } else {
                    e.summary.clone()
                };
                out.push(Occurrence {
                    id: format!("{source}/{}/{}", e.uid, original.timestamp_millis()),
                    source: source.to_string(),
                    uid: e.uid.clone(),
                    title,
                    location: e.location.clone(),
                    start: begins,
                    end: ends,
                    all_day,
                    recurring,
                });
            }
        };
        if e.cancelled {
            continue;
        }
        if let Some(r) = &e.recurrence_id {
            if masters.get(e.uid.as_str()).is_some_and(|m| m.cancelled) {
                continue;
            }
            place(r.instant(zone), start.instant(zone), &mut out);
            continue;
        }
        if !recurring {
            let at = start.instant(zone);
            place(at, at, &mut out);
            continue;
        }
        let dt_start = start.rrule(zone);
        let mut set = RRuleSet::new(dt_start);
        let mut readable = true;
        for r in &e.rrules {
            match r
                .parse::<RRule<Unvalidated>>()
                .and_then(|r| r.validate(dt_start))
            {
                Ok(r) => set = set.rrule(r),
                Err(_) => readable = false,
            }
        }
        for d in &e.rdates {
            set = set.rdate(d.rrule(zone));
        }
        for d in &e.exdates {
            set = set.exdate(d.rrule(zone));
        }
        if !readable {
            unread += 1;
            let at = start.instant(zone);
            place(at, at, &mut out);
            continue;
        }
        let after = (from - length - Duration::seconds(1)).with_timezone(&rrule::Tz::UTC);
        let before = to.with_timezone(&rrule::Tz::UTC);
        let skip = moved.get(e.uid.as_str());
        for at in set.after(after).before(before).all(5000).dates {
            let at = at.with_timezone(&Utc);
            if skip.is_some_and(|s| s.contains(&at)) {
                continue;
            }
            place(at, at, &mut out);
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    (out, unread)
}

/// How long an instance lasts: to its end, for its duration, or a day for a whole day and nothing for a
/// moment.
fn span(e: &Event, zone: Tz) -> Duration {
    let start = e.start.as_ref().map(|s| s.instant(zone));
    match (start, &e.end, e.duration) {
        (Some(s), Some(end), _) => (end.instant(zone) - s).max(Duration::zero()),
        (_, _, Some(d)) => d,
        _ if matches!(e.start, Some(When::Date(_))) => Duration::days(1),
        _ => Duration::zero(),
    }
}

/// The lines of a text, each folded line joined to the one it continues.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

struct Property {
    name: String,
    params: BTreeMap<String, String>,
    value: String,
}

/// `NAME;PARAM=value;PARAM="quoted:value":VALUE`.
fn property(line: &str) -> Property {
    let mut quoted = false;
    let mut split = line.len();
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                split = i;
                break;
            }
            _ => {}
        }
    }
    let (head, value) = (&line[..split], line.get(split + 1..).unwrap_or(""));
    let mut parts = head.split(';');
    let name = parts.next().unwrap_or("").to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        .collect();
    Property {
        name,
        params,
        value: value.to_string(),
    }
}

fn text_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `YYYYMMDD`, `YYYYMMDDTHHMMSS` or `YYYYMMDDTHHMMSSZ`.
fn when(v: &str, date: bool, tz: Tz) -> Option<When> {
    let v = v.trim();
    if date || v.len() == 8 {
        return NaiveDate::parse_from_str(v, "%Y%m%d").ok().map(When::Date);
    }
    if let Some(utc) = v.strip_suffix('Z') {
        return NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .ok()
            .map(|n| When::Utc(Utc.from_utc_datetime(&n)));
    }
    NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")
        .ok()
        .map(|n| When::Local(n, tz))
}

/// `[+-]P[nW]` or `[+-]P[nD][T[nH][nM][nS]]`.
fn duration(v: &str) -> Option<Duration> {
    let (negative, v) = match v.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let (mut total, mut number, mut in_time) = (Duration::zero(), String::new(), false);
    for c in v.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' => in_time = true,
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += match (unit, in_time) {
                    ('W', false) => Duration::weeks(n),
                    ('D', false) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    if !number.is_empty() {
        return None;
    }
    Some(if negative { -total } else { total })
}

/// The Windows zone names Outlook and Exchange write most, to their IANA zone.
fn windows(name: &str) -> Option<Tz> {
    let iana = match name {
        "UTC" | "Coordinated Universal Time" => "UTC",
        "GMT Standard Time" => "Europe/London",
        "W. Europe Standard Time" => "Europe/Berlin",
        "Romance Standard Time" => "Europe/Paris",
        "Central Europe Standard Time" => "Europe/Budapest",
        "Central European Standard Time" => "Europe/Warsaw",
        "E. Europe Standard Time" => "Europe/Chisinau",
        "FLE Standard Time" => "Europe/Kiev",
        "GTB Standard Time" => "Europe/Bucharest",
        "Russian Standard Time" => "Europe/Moscow",
        "Eastern Standard Time" => "America/New_York",
        "Central Standard Time" => "America/Chicago",
        "Mountain Standard Time" => "America/Denver",
        "Pacific Standard Time" => "America/Los_Angeles",
        "India Standard Time" => "Asia/Kolkata",
        "China Standard Time" => "Asia/Shanghai",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        _ => return None,
    };
    iana.parse().ok()
}
