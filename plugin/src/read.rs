//! A source read, and what was last read of it kept in the unit's storage.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::accounts::Tokens;
use crate::ics::{self, Event, Occurrence};
use crate::online;
use crate::sources::{Credential, Http, Kind, Request, Source};

/// What a source holds: events to expand, read from iCalendar, or occurrences a provider expanded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Read {
    pub events: Vec<Event>,
    pub expanded: Vec<Occurrence>,
    /// Components that could not be placed.
    pub unread: usize,
    /// The text the events were read from, to be kept; none for a source on the host.
    pub kept: Option<String>,
}

impl Read {
    fn ics(events: Vec<Event>, unread: usize, kept: String) -> Read {
        Read {
            events,
            unread,
            kept: Some(kept),
            ..Default::default()
        }
    }
}

/// Reads a source. `Err` says why, in words that carry no credential.
pub async fn read(
    http: &dyn Http,
    source: &Source,
    credential: Option<&Credential>,
    tokens: Option<&dyn Tokens>,
    zone: Tz,
) -> Result<Read, String> {
    match &source.kind {
        Kind::Ics(location) => {
            let url = credential
                .and_then(|c| c.url.clone())
                .unwrap_or_else(|| location.clone());
            if url.is_empty() {
                return Err("it names no URL, and the credentials give none".into());
            }
            let r = http
                .send(
                    Request::get(&url)
                        .header("Accept", "text/calendar")
                        .basic(credential),
                )
                .await?;
            if !(200..300).contains(&r.status) {
                return Err(format!("the server answered {}", r.status));
            }
            let (events, unread) = ics::parse(&r.body, zone)?;
            Ok(Read::ics(events, unread, r.body))
        }
        Kind::CalDav(collection) => {
            let now = chrono::Utc::now();
            let body = caldav_query(now - CALDAV_BEFORE, now + CALDAV_AFTER);
            let r = http
                .send(
                    Request {
                        method: "REPORT".into(),
                        url: collection.clone(),
                        headers: vec![
                            ("Depth".into(), "1".into()),
                            (
                                "Content-Type".into(),
                                "application/xml; charset=utf-8".into(),
                            ),
                        ],
                        body: Some(body),
                    }
                    .basic(credential),
                )
                .await?;
            match r.status {
                207 | 200 => {}
                401 | 403 => {
                    return Err(format!(
                        "the server answered {}: the credentials are refused or missing",
                        r.status
                    ))
                }
                s => return Err(format!("the server answered {s}")),
            }
            let text = calendar_data(&r.body)?;
            let (events, unread) = if text.is_empty() {
                (vec![], 0)
            } else {
                ics::parse(&text, zone)?
            };
            let kept = if text.is_empty() {
                "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into()
            } else {
                text
            };
            Ok(Read::ics(events, unread, kept))
        }
        Kind::Dir(dir) => read_dir(dir, zone),
        Kind::Microsoft { account, calendar } | Kind::Google { account, calendar } => {
            let tokens = tokens.ok_or("online-accounts is not bound to the need accounts")?;
            let expanded = if matches!(source.kind, Kind::Microsoft { .. }) {
                online::microsoft(http, tokens, &source.id, account, calendar.as_deref(), zone)
                    .await?
            } else {
                online::google(http, tokens, &source.id, account, calendar.as_deref(), zone).await?
            };
            Ok(Read {
                expanded,
                ..Default::default()
            })
        }
    }
}

/// The span a CalDAV collection is asked for, around now: an event whose series has an instance in it is
/// answered whole, and expanded here.
pub const CALDAV_BEFORE: chrono::Duration = chrono::Duration::days(31);
pub const CALDAV_AFTER: chrono::Duration = chrono::Duration::days(400);

/// A `calendar-query` for every event with an instance in `[from, to)` (RFC 4791 §7.8).
pub fn caldav_query(
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> String {
    let f = "%Y%m%dT%H%M%SZ";
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">"#,
            r#"<D:prop><C:calendar-data/></D:prop>"#,
            r#"<C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">"#,
            r#"<C:time-range start="{}" end="{}"/>"#,
            r#"</C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
        ),
        from.format(f),
        to.format(f)
    )
}

/// Every `calendar-data` of a multistatus answer, one after the other.
pub fn calendar_data(xml: &str) -> Result<String, String> {
    use quick_xml::events::Event as X;
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut out, mut inside) = (String::new(), false);
    loop {
        match reader.read_event() {
            Ok(X::Start(e)) if e.local_name().as_ref() == b"calendar-data" => inside = true,
            Ok(X::End(e)) if e.local_name().as_ref() == b"calendar-data" => {
                inside = false;
                if !out.ends_with('\n') {
                    out.push_str("\r\n");
                }
            }
            Ok(X::Text(t)) if inside => out.push_str(
                &t.unescape()
                    .map_err(|e| format!("the server's answer is not XML: {e}"))?,
            ),
            Ok(X::CData(c)) if inside => out.push_str(&String::from_utf8_lossy(&c.into_inner())),
            Ok(X::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("the server's answer is not XML: {e}")),
        }
    }
    Ok(out)
}

/// Every `.ics` file of a directory, in name order; a file that is not iCalendar is counted, not fatal.
fn read_dir(dir: &Path, zone: Tz) -> Result<Read, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("{} cannot be read: {e}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ics")))
        .collect();
    files.sort();
    let (mut events, mut unread) = (Vec::new(), 0);
    for f in files {
        match std::fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| ics::parse(&t, zone))
        {
            Ok((e, u)) => {
                events.extend(e);
                unread += u;
            }
            Err(_) => unread += 1,
        }
    }
    Ok(Read {
        events,
        unread,
        ..Default::default()
    })
}

/// What is kept of a source: the text last read, or the occurrences a provider answered, and when.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Kept {
    at: u64,
    #[serde(default)]
    ics: String,
    #[serde(default)]
    occurrences: Vec<KeptOccurrence>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeptOccurrence {
    id: String,
    uid: String,
    title: String,
    location: Option<String>,
    start: i64,
    end: i64,
    all_day: bool,
    recurring: bool,
}

/// What a source's last reading left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keeping {
    Ics(String),
    Expanded(Vec<Occurrence>),
}

/// The storage a source's last reading is kept in, one file per source.
#[derive(Debug, Clone)]
pub struct Cache(pub PathBuf);

impl Cache {
    fn path(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.json"))
    }

    /// Keeps what a source's reading left, replacing what was kept whole: a reader never sees half of one.
    pub fn keep(&self, id: &str, at: SystemTime, what: &Keeping) -> Result<(), String> {
        let mut kept = Kept {
            at: at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            ..Default::default()
        };
        match what {
            Keeping::Ics(text) => kept.ics = text.clone(),
            Keeping::Expanded(o) => {
                kept.occurrences = o
                    .iter()
                    .map(|o| KeptOccurrence {
                        id: o.id.clone(),
                        uid: o.uid.clone(),
                        title: o.title.clone(),
                        location: o.location.clone(),
                        start: o.start.timestamp_millis(),
                        end: o.end.timestamp_millis(),
                        all_day: o.all_day,
                        recurring: o.recurring,
                    })
                    .collect()
            }
        }
        let body = serde_json::to_vec(&kept).map_err(|e| e.to_string())?;
        let path = self.path(id);
        let fresh = path.with_extension("json.new");
        std::fs::write(&fresh, body)
            .and_then(|_| std::fs::rename(&fresh, &path))
            .map_err(|e| format!("the cache cannot be written: {e}"))
    }

    /// What was kept of a source, and when it was read.
    pub fn kept(&self, id: &str) -> Option<(SystemTime, Keeping)> {
        let text = std::fs::read(self.path(id)).ok()?;
        let k: Kept = serde_json::from_slice(&text).ok()?;
        let at = UNIX_EPOCH + Duration::from_millis(k.at);
        if !k.ics.is_empty() {
            return Some((at, Keeping::Ics(k.ics)));
        }
        let instant = |ms: i64| chrono::Utc.timestamp_millis_opt(ms).single();
        let occurrences = k
            .occurrences
            .into_iter()
            .filter_map(|o| {
                Some(Occurrence {
                    start: instant(o.start)?,
                    end: instant(o.end)?,
                    source: id.to_string(),
                    id: o.id,
                    uid: o.uid,
                    title: o.title,
                    location: o.location,
                    all_day: o.all_day,
                    recurring: o.recurring,
                })
            })
            .collect();
        Some((at, Keeping::Expanded(occurrences)))
    }
}
