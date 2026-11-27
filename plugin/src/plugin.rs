//! The calendar Plugin: the events of its sources, as questions, a stream, a command and occurrences.
//!
//! - **`calendar.sources`** answers every source and where it stands.
//! - **`calendar.events`** answers the occurrences in a window, every recurrence expanded, and
//!   **`calendar.days`** the same by day in the unit's zone.
//! - **`calendar.upcoming`** is what is on now and what is next, again at every change.
//! - **`calendar.refresh`** reads the sources again, or one of them.
//! - **`calendar.event.starting`** says an event is about to start — which, and when, never its title, since
//!   an occurrence is kept and a title is the calendar's own text. **`calendar.source.failed`** says a source
//!   could not be read, once until it is read again.
//! - **What was last read of a source is kept in the storage `cache`**, and served while the source cannot
//!   be read.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use yoke_sdk::plugin::{
    Capability, Command, Declaration, Event, Object, Outcome, Question, Severity, Stream, Unit, Use,
};

use crate::accounts::Tokens;
use crate::ics::{self, Occurrence};
use crate::read::{self, Cache, Keeping};
use crate::sources::{Credential, Http, Source};

pub const PLUGIN: &str = "com.yoke.calendar";
/// The format every answer, frame and detail states as `schema`.
pub const SCHEMA: u32 = 1;

pub const SOURCES: &str = "calendar.sources";
pub const EVENTS: &str = "calendar.events";
pub const DAYS: &str = "calendar.days";
pub const QUERIES: [&str; 3] = [SOURCES, EVENTS, DAYS];

pub const UPCOMING: &str = "calendar.upcoming";
pub const STREAMS: [&str; 1] = [UPCOMING];

pub const REFRESH: &str = "calendar.refresh";
pub const COMMANDS: [&str; 1] = [REFRESH];

pub const STARTING: &str = "calendar.event.starting";
pub const FAILED: &str = "calendar.source.failed";
pub const OCCURRENCES: [&str; 2] = [STARTING, FAILED];

/// The secret the deployment supplies, the storage the unit keeps, and the unit it asks for tokens.
pub const CREDENTIALS_SECRET: &str = "credentials";
pub const CACHE_STORAGE: &str = "cache";
pub const ACCOUNTS_NEED: &str = "accounts";
/// What the unit asks of `online-accounts`.
pub const TOKEN: &str = "oauth.token";

/// The longest window a question may ask, and the most days.
pub const LONGEST: chrono::Duration = chrono::Duration::days(400);
pub const MOST_DAYS: u32 = 62;
/// The window a question names none of: a week from now.
pub const WINDOW: chrono::Duration = chrono::Duration::days(7);
/// An event is still said to start this long after it started, so that a reminder is not lost between two
/// looks.
pub const STARTED_WITHIN: chrono::Duration = chrono::Duration::seconds(60);

pub fn declaration() -> Declaration {
    let answer = |q: &&str| Capability {
        name: format!("query.{q}.answer"),
        governs: Object::Query(q.to_string()),
    };
    let accept = |c: &&str| Capability {
        name: format!("command.{c}.accept"),
        governs: Object::Command(c.to_string()),
    };
    let publish = |s: &&str| Capability {
        name: format!("stream.{s}.publish"),
        governs: Object::Stream(s.to_string()),
    };
    let report = |o: &&str| Capability {
        name: format!("event.{o}.report"),
        governs: Object::Occurrence(o.to_string()),
    };
    Declaration {
        id: PLUGIN.into(),
        needs: vec![
            format!("secret:{CREDENTIALS_SECRET}"),
            format!("storage:{CACHE_STORAGE}"),
            format!("unit:{ACCOUNTS_NEED}"),
        ],
        queries: QUERIES.iter().map(|q| q.to_string()).collect(),
        commands: COMMANDS.iter().map(|c| c.to_string()).collect(),
        // What is on and what is next, whole at every change: a lost frame is made good by the next.
        streams: vec![Stream {
            id: UPCOMING.into(),
            tolerates_loss: true,
            tolerates_reorder: false,
            max_frame: 0,
        }],
        occurrences: OCCURRENCES.iter().map(|o| o.to_string()).collect(),
        uses: BTreeMap::from([(
            ACCOUNTS_NEED.to_string(),
            Use {
                questions: vec![TOKEN.into()],
                streams: vec![],
            },
        )]),
        capabilities: QUERIES
            .iter()
            .map(answer)
            .chain(COMMANDS.iter().map(accept))
            .chain(STREAMS.iter().map(publish))
            .chain(OCCURRENCES.iter().map(report))
            .chain([Capability {
                name: format!("query.{TOKEN}.ask"),
                governs: Object::UseQuery(ACCOUNTS_NEED.into(), TOKEN.into()),
            }])
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Invalid(String),
    NotFound(String),
    Unavailable(String),
}

pub fn code(e: &Error) -> &'static str {
    match e {
        Error::Invalid(_) => "invalid",
        Error::NotFound(_) => "not_found",
        Error::Unavailable(_) => "unavailable",
    }
}

pub fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) | Error::NotFound(m) | Error::Unavailable(m) => m.clone(),
    }
}

fn input<T: DeserializeOwned>(what: &str, payload: &[u8]) -> Result<T, Error> {
    let payload = if payload.iter().all(u8::is_ascii_whitespace) {
        b"{}".as_slice()
    } else {
        payload
    };
    serde_json::from_slice(payload).map_err(|e| {
        let kind = match e.classify() {
            serde_json::error::Category::Data => "a field is missing, unknown or of the wrong type",
            _ => "it is not JSON",
        };
        Error::Invalid(format!(
            "the payload is not what {what} takes: {kind}, at line {} column {}",
            e.line(),
            e.column()
        ))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NoInput {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsInput {
    /// Milliseconds since the epoch; now where absent.
    pub from: Option<i64>,
    /// A week after `from` where absent.
    pub to: Option<i64>,
    /// Every source where absent.
    pub sources: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaysInput {
    /// `YYYY-MM-DD` in the unit's zone; today where absent.
    pub from: Option<String>,
    /// Seven where absent.
    pub days: Option<u32>,
    pub sources: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshInput {
    /// Every source where absent.
    pub source: Option<String>,
}

fn ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Where a source stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Not read yet since the unit started; what was kept, if anything, is served.
    Pending,
    Ok,
    /// Its last reading failed, and what was read before is served.
    Stale,
    /// Its last reading failed, and nothing was ever read of it.
    Failed,
}

impl Standing {
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Pending => "pending",
            Standing::Ok => "ok",
            Standing::Stale => "stale",
            Standing::Failed => "failed",
        }
    }
}

/// A source and what is held of it.
#[derive(Debug, Clone)]
struct Held {
    source: Source,
    events: Vec<ics::Event>,
    /// Occurrences a provider expanded itself, for a source read that way.
    expanded: Vec<Occurrence>,
    unread: usize,
    standing: Standing,
    read_at: Option<SystemTime>,
    line: Option<String>,
}

/// What the Plugin does on the Session, behind a trait so that a test can stand in for the Core.
pub trait Acts {
    fn answer(
        &self,
        q: &Question,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn fail(
        &self,
        q: &Question,
        code: &str,
        message: &str,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn ack(
        &self,
        c: &Command,
        outcome: Outcome,
        line: &str,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn report(
        &self,
        occurrence: &str,
        severity: u8,
        line: &str,
        detail: Value,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn emit(
        &self,
        stream: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn health(
        &self,
        grade: u8,
        line: &str,
    ) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
}

impl Acts for Unit {
    async fn answer(&self, q: &Question, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::answer(self, q, payload).await
    }
    async fn fail(
        &self,
        q: &Question,
        code: &str,
        message: &str,
    ) -> Result<(), yoke_sdk::base::Error> {
        Unit::fail(self, &q.id, code, message).await
    }
    async fn ack(
        &self,
        c: &Command,
        outcome: Outcome,
        line: &str,
    ) -> Result<(), yoke_sdk::base::Error> {
        Unit::ack(self, c, outcome, line).await
    }
    async fn report(
        &self,
        occurrence: &str,
        severity: u8,
        line: &str,
        detail: Value,
    ) -> Result<(), yoke_sdk::base::Error> {
        Unit::report(
            self,
            occurrence,
            Some(Severity::of(severity)),
            line,
            detail.to_string().into_bytes(),
        )
        .await
    }
    async fn emit(&self, stream: &str, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::emit(self, stream, payload).await
    }
    async fn health(&self, grade: u8, line: &str) -> Result<(), yoke_sdk::base::Error> {
        Unit::health(self, grade, line).await
    }
}

/// What happened apart from a question or a command, for the Session to carry.
#[derive(Debug, Clone, PartialEq)]
pub enum Happened {
    Changed,
    Occurred {
        occurrence: &'static str,
        severity: u8,
        line: String,
        detail: Value,
    },
    Health {
        grade: u8,
        line: String,
    },
}

/// What the Plugin is built from, apart from the Session.
pub struct Parts {
    pub http: Arc<dyn Http>,
    pub sources: Vec<Source>,
    pub credentials: BTreeMap<String, Credential>,
    /// `online-accounts`, on the need's channel.
    pub tokens: Option<Arc<dyn Tokens>>,
    pub cache: Cache,
    pub zone: Tz,
    pub remind_before: Duration,
    pub upcoming: usize,
    pub horizon: chrono::Duration,
}

pub struct Calendar {
    http: Arc<dyn Http>,
    credentials: BTreeMap<String, Credential>,
    tokens: Option<Arc<dyn Tokens>>,
    cache: Cache,
    zone: Tz,
    remind_before: chrono::Duration,
    upcoming: usize,
    horizon: chrono::Duration,
    held: Mutex<Vec<Held>>,
    /// One reading at a time, so that two refreshes do not race to the cache.
    reading: tokio::sync::Mutex<()>,
    /// The events said to start, by occurrence and its start, until they are past.
    reported: Mutex<BTreeMap<String, DateTime<Utc>>>,
    /// What the stream last carried, to tell a change.
    shown: Mutex<Value>,
    sequence: AtomicU64,
    /// Raised whenever a read changes the occurrences a source holds, so that a client showing more than
    /// the stream carries — a month of `calendar.days` — knows when to ask again.
    revision: AtomicU64,
    emitted: AtomicU64,
    flows: AtomicBool,
    publishing: tokio::sync::Mutex<()>,
    happened: UnboundedSender<Happened>,
}

fn event_json(o: &Occurrence, zone: Tz) -> Value {
    let mut v = json!({
        "id": o.id,
        "source": o.source,
        "uid": o.uid,
        "title": o.title,
        "location": o.location,
        "start": o.start.timestamp_millis(),
        "end": o.end.timestamp_millis(),
        "all_day": o.all_day,
        "recurring": o.recurring,
    });
    if o.all_day {
        v["date"] = json!(o.start.with_timezone(&zone).date_naive().to_string());
    }
    v
}

impl Calendar {
    /// The Plugin from its parts, every source served from what the storage kept of it until it is read.
    pub fn new(parts: Parts, happened: UnboundedSender<Happened>) -> Arc<Calendar> {
        let held = parts
            .sources
            .into_iter()
            .map(|source| {
                let (mut events, mut expanded, mut unread, mut read_at) = (vec![], vec![], 0, None);
                match parts.cache.kept(&source.id) {
                    Some((at, Keeping::Ics(text))) => {
                        if let Ok((e, u)) = ics::parse(&text, parts.zone) {
                            (events, unread, read_at) = (e, u, Some(at));
                        }
                    }
                    Some((at, Keeping::Expanded(o))) => (expanded, read_at) = (o, Some(at)),
                    None => {}
                }
                Held {
                    source,
                    events,
                    expanded,
                    unread,
                    standing: Standing::Pending,
                    read_at,
                    line: None,
                }
            })
            .collect();
        Arc::new(Calendar {
            http: parts.http,
            credentials: parts.credentials,
            tokens: parts.tokens,
            cache: parts.cache,
            zone: parts.zone,
            remind_before: chrono::Duration::from_std(parts.remind_before)
                .unwrap_or(chrono::Duration::zero()),
            upcoming: parts.upcoming,
            horizon: parts.horizon,
            held: Mutex::new(held),
            reading: tokio::sync::Mutex::new(()),
            reported: Mutex::new(BTreeMap::new()),
            shown: Mutex::new(Value::Null),
            sequence: AtomicU64::new(1),
            revision: AtomicU64::new(0),
            emitted: AtomicU64::new(0),
            flows: AtomicBool::new(false),
            publishing: tokio::sync::Mutex::new(()),
            happened,
        })
    }

    fn occurred(&self, occurrence: &'static str, severity: u8, line: String, detail: Value) {
        let _ = self.happened.send(Happened::Occurred {
            occurrence,
            severity,
            line,
            detail,
        });
    }

    /// Reads every source, or the one named, and says how it went. A source that cannot be read is said
    /// once, until it is read again.
    pub async fn refresh(&self, only: Option<&str>) -> Result<String, Error> {
        let wanted: Vec<Source> = {
            let held = self.held.lock().unwrap();
            if let Some(id) = only {
                if !held.iter().any(|h| h.source.id == id) {
                    return Err(Error::NotFound(format!("there is no source {id}")));
                }
            }
            held.iter()
                .filter(|h| only.is_none_or(|id| h.source.id == id))
                .map(|h| h.source.clone())
                .collect()
        };
        let _one = self.reading.lock().await;
        let (mut read, mut failed) = (0, Vec::new());
        for source in wanted {
            let got = read::read(
                self.http.as_ref(),
                &source,
                self.credentials.get(&source.id),
                self.tokens.as_deref(),
                self.zone,
            )
            .await;
            let now = SystemTime::now();
            let mut said = None;
            {
                let mut held = self.held.lock().unwrap();
                let Some(h) = held.iter_mut().find(|h| h.source.id == source.id) else {
                    continue;
                };
                match got {
                    Ok(r) => {
                        read += 1;
                        let keeping = match r.kept {
                            Some(text) => Some(Keeping::Ics(text)),
                            None if source.kind.uses_accounts() => {
                                Some(Keeping::Expanded(r.expanded.clone()))
                            }
                            None => None,
                        };
                        h.line = keeping.and_then(|k| self.cache.keep(&source.id, now, &k).err());
                        if h.events != r.events || h.expanded != r.expanded {
                            self.revision.fetch_add(1, Ordering::Relaxed);
                        }
                        h.events = r.events;
                        h.expanded = r.expanded;
                        h.unread = r.unread;
                        h.standing = Standing::Ok;
                        h.read_at = Some(now);
                    }
                    Err(why) => {
                        failed.push(source.id.clone());
                        let was = h.standing;
                        h.standing = if h.read_at.is_some() {
                            Standing::Stale
                        } else {
                            Standing::Failed
                        };
                        h.line = Some(why.clone());
                        if matches!(was, Standing::Ok | Standing::Pending) {
                            said = Some((why, h.standing == Standing::Stale));
                        }
                    }
                }
            }
            if let Some((why, stale)) = said {
                let line = format!("the source {} could not be read: {why}", source.id);
                self.occurred(
                    FAILED,
                    30,
                    line.clone(),
                    json!({ "schema": SCHEMA, "line": line, "source": source.id, "stale": stale }),
                );
            }
        }
        self.changed(Utc::now());
        let _ = self.happened.send(self.health());
        Ok(match failed.len() {
            0 => format!("{read} sources read"),
            _ => format!(
                "{read} sources read; {} could not be: {}",
                failed.len(),
                failed.join(", ")
            ),
        })
    }

    /// Where the unit stands: every source read, some not, or none.
    pub fn health(&self) -> Happened {
        let held = self.held.lock().unwrap();
        let failing: Vec<&str> = held
            .iter()
            .filter(|h| matches!(h.standing, Standing::Stale | Standing::Failed))
            .map(|h| h.source.id.as_str())
            .collect();
        let serving = held
            .iter()
            .filter(|h| h.standing != Standing::Failed)
            .count();
        let (grade, line) = if held.is_empty() {
            (
                50,
                "no source is named: --source <id>=<kind>:<location>".to_string(),
            )
        } else if serving == 0 {
            (50, format!("no source can be read: {}", failing.join(", ")))
        } else if !failing.is_empty() {
            (
                30,
                format!(
                    "{} sources; {} could not be read: {}",
                    held.len(),
                    failing.len(),
                    failing.join(", ")
                ),
            )
        } else {
            (0, format!("{} sources", held.len()))
        };
        Happened::Health { grade, line }
    }

    /// The occurrences of the sources named, or of every source, overlapping `[from, to)`.
    pub fn occurrences(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        sources: Option<&[String]>,
    ) -> Result<Vec<Occurrence>, Error> {
        let held = self.held.lock().unwrap();
        if let Some(unknown) = sources
            .unwrap_or_default()
            .iter()
            .find(|id| !held.iter().any(|h| &h.source.id == *id))
        {
            return Err(Error::NotFound(format!("there is no source {unknown}")));
        }
        let mut out = Vec::new();
        for h in held
            .iter()
            .filter(|h| sources.is_none_or(|s| s.contains(&h.source.id)))
        {
            out.extend(ics::expand(&h.events, &h.source.id, from, to, self.zone).0);
            out.extend(
                h.expanded
                    .iter()
                    .filter(|o| {
                        o.start < to && (o.end > from || (o.end == o.start && o.start >= from))
                    })
                    .cloned(),
            );
        }
        out.sort_by(|a, b| a.start.cmp(&b.start).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// Answers a question.
    pub fn answer(&self, query: &str, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let answer = match query {
            SOURCES => {
                let _: NoInput = input(query, payload)?;
                let held = self.held.lock().unwrap();
                let sources: Vec<Value> = held
                    .iter()
                    .map(|h| {
                        json!({
                            "id": h.source.id,
                            "kind": h.source.kind.as_str(),
                            "location": h.source.kind.shown(),
                            "state": h.standing.as_str(),
                            "read_at": h.read_at.map(ms),
                            "unread": h.unread,
                            "line": h.line,
                        })
                    })
                    .collect();
                json!({ "schema": SCHEMA, "zone": self.zone.name(), "sources": sources })
            }
            EVENTS => {
                let i: EventsInput = input(query, payload)?;
                let from = match i.from {
                    Some(v) => instant(v)?,
                    None => Utc::now(),
                };
                let to = match i.to {
                    Some(v) => instant(v)?,
                    None => from + WINDOW,
                };
                if to <= from {
                    return Err(Error::Invalid("to is not after from".into()));
                }
                if to - from > LONGEST {
                    return Err(Error::Invalid(format!(
                        "a window is at most {} days",
                        LONGEST.num_days()
                    )));
                }
                let events: Vec<Value> = self
                    .occurrences(from, to, i.sources.as_deref())?
                    .iter()
                    .map(|o| event_json(o, self.zone))
                    .collect();
                json!({ "schema": SCHEMA, "from": from.timestamp_millis(), "to": to.timestamp_millis(), "events": events })
            }
            DAYS => {
                let i: DaysInput = input(query, payload)?;
                let first = match &i.from {
                    Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
                        .map_err(|_| Error::Invalid(format!("{d} is not a day: YYYY-MM-DD")))?,
                    None => Utc::now().with_timezone(&self.zone).date_naive(),
                };
                let n = i.days.unwrap_or(7);
                if n == 0 || n > MOST_DAYS {
                    return Err(Error::Invalid(format!("days is from 1 to {MOST_DAYS}")));
                }
                let midnight = |d: NaiveDate| {
                    self.zone
                        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                        .earliest()
                        .map(|t| t.with_timezone(&Utc))
                        .unwrap_or_else(|| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
                };
                let last = first + chrono::Duration::days(n as i64);
                let all =
                    self.occurrences(midnight(first), midnight(last), i.sources.as_deref())?;
                let days: Vec<Value> = first
                    .iter_days()
                    .take(n as usize)
                    .map(|d| {
                        let (from, to) = (midnight(d), midnight(d + chrono::Duration::days(1)));
                        let events: Vec<Value> = all
                            .iter()
                            .filter(|o| {
                                o.start < to
                                    && (o.end > from || (o.end == o.start && o.start >= from))
                            })
                            .map(|o| event_json(o, self.zone))
                            .collect();
                        json!({ "date": d.to_string(), "events": events })
                    })
                    .collect();
                json!({ "schema": SCHEMA, "zone": self.zone.name(), "days": days })
            }
            other => {
                return Err(Error::Invalid(format!(
                    "{other} is not a question this Plugin answers"
                )))
            }
        };
        Ok(answer.to_string().into_bytes())
    }

    /// Carries out a command, and says how it went.
    pub async fn act(&self, command: &str, payload: &[u8]) -> Result<String, Error> {
        match command {
            REFRESH => {
                let i: RefreshInput = input(command, payload)?;
                self.refresh(i.source.as_deref()).await
            }
            other => Err(Error::Invalid(format!(
                "{other} is not a command this Plugin takes"
            ))),
        }
    }

    /// What is on at `now` and what is next, within the horizon.
    fn upcoming_at(&self, now: DateTime<Utc>) -> (Vec<Occurrence>, Vec<Occurrence>) {
        let all = self
            .occurrences(now, now + self.horizon, None)
            .unwrap_or_default();
        let (on, next): (Vec<_>, Vec<_>) = all.into_iter().partition(|o| o.start <= now);
        (on, next.into_iter().take(self.upcoming).collect())
    }

    /// Looks at `now`: the stream changes where what is on or next did, and an event about to start is said.
    pub fn changed(&self, now: DateTime<Utc>) {
        let (on, next) = self.upcoming_at(now);
        let content = json!({
            "now": on.iter().map(|o| event_json(o, self.zone)).collect::<Vec<_>>(),
            "next": next.iter().map(|o| event_json(o, self.zone)).collect::<Vec<_>>(),
            "revision": self.revision.load(Ordering::Relaxed),
        });
        {
            let mut shown = self.shown.lock().unwrap();
            if *shown != content {
                *shown = content;
                self.sequence.fetch_add(1, Ordering::Relaxed);
                let _ = self.happened.send(Happened::Changed);
            }
        }
        let starting: Vec<Occurrence> = self
            .occurrences(
                now - STARTED_WITHIN,
                now + self.remind_before + chrono::Duration::seconds(1),
                None,
            )
            .unwrap_or_default()
            .into_iter()
            .filter(|o| {
                !o.all_day && o.start - self.remind_before <= now && now < o.start + STARTED_WITHIN
            })
            .collect();
        let mut reported = self.reported.lock().unwrap();
        reported.retain(|_, start| *start + STARTED_WITHIN > now);
        for o in starting {
            if reported.insert(o.id.clone(), o.start).is_some() {
                continue;
            }
            let at = o.start.with_timezone(&self.zone).format("%H:%M");
            let line = format!("an event of {} starts at {at}", o.source);
            self.occurred(
                STARTING,
                10,
                line.clone(),
                json!({ "schema": SCHEMA, "line": line, "source": o.source, "event": o.id, "start": o.start.timestamp_millis() }),
            );
        }
    }

    /// The record the stream carries.
    pub fn record(&self) -> Value {
        let shown = self.shown.lock().unwrap().clone();
        json!({
            "schema": SCHEMA,
            "sequence": self.sequence.load(Ordering::Relaxed),
            "at": ms(SystemTime::now()),
            "zone": self.zone.name(),
            "now": shown.get("now").cloned().unwrap_or(json!([])),
            "next": shown.get("next").cloned().unwrap_or(json!([])),
            "revision": shown.get("revision").cloned().unwrap_or(json!(0)),
        })
    }

    /// Emits the record where the stream flows and it changed since it was last emitted.
    pub async fn publish(&self, u: &impl Acts) -> Result<(), yoke_sdk::base::Error> {
        if !self.flows.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _one = self.publishing.lock().await;
        let record = self.record();
        let sequence = record["sequence"].as_u64().unwrap_or(0);
        if self.emitted.fetch_max(sequence, Ordering::Relaxed) >= sequence {
            return Ok(());
        }
        u.emit(UPCOMING, record.to_string().into_bytes()).await
    }

    /// The stream was activated: it is given the present record at once.
    pub async fn activated(
        &self,
        u: &impl Acts,
        stream: &str,
    ) -> Result<(), yoke_sdk::base::Error> {
        if stream != UPCOMING {
            return Ok(());
        }
        self.flows.store(true, Ordering::Relaxed);
        let _one = self.publishing.lock().await;
        let record = self.record();
        self.emitted
            .fetch_max(record["sequence"].as_u64().unwrap_or(0), Ordering::Relaxed);
        u.emit(UPCOMING, record.to_string().into_bytes()).await
    }

    pub fn stopped(&self, stream: &str) {
        if stream == UPCOMING {
            self.flows.store(false, Ordering::Relaxed);
        }
    }

    /// The sources' identities, in the order the arguments named them.
    pub fn source_ids(&self) -> BTreeSet<String> {
        self.held
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.source.id.clone())
            .collect()
    }
}

fn instant(v: i64) -> Result<DateTime<Utc>, Error> {
    Utc.timestamp_millis_opt(v)
        .single()
        .ok_or_else(|| Error::Invalid(format!("{v} is not an instant in milliseconds")))
}

/// Answers a question or carries out a command the Session brought.
pub async fn handle(
    u: &impl Acts,
    event: &Event,
    c: &Arc<Calendar>,
) -> Result<(), yoke_sdk::base::Error> {
    match event {
        Event::Question(q) if QUERIES.contains(&q.r#type.as_str()) => {
            match c.answer(&q.r#type, &q.payload) {
                Ok(answer) => u.answer(q, answer).await,
                Err(e) => u.fail(q, code(&e), &message(&e)).await,
            }
        }
        Event::Command(cmd) if COMMANDS.contains(&cmd.r#type.as_str()) => {
            match c.act(&cmd.r#type, &cmd.payload).await {
                Ok(line) => u
                    .ack(cmd, Outcome::Done, &line)
                    .await
                    .and(c.publish(u).await),
                Err(e) => {
                    u.ack(
                        cmd,
                        Outcome::Failed,
                        &format!("{}: {}", code(&e), message(&e)),
                    )
                    .await
                }
            }
        }
        _ => Ok(()),
    }
}

/// What happened, carried on the Session.
pub async fn carry(u: &impl Acts, c: &Calendar, h: Happened) -> Result<(), yoke_sdk::base::Error> {
    match h {
        Happened::Changed => c.publish(u).await,
        Happened::Occurred {
            occurrence,
            severity,
            line,
            detail,
        } => u.report(occurrence, severity, &line, detail).await,
        Happened::Health { grade, line } => u.health(grade, &line).await,
    }
}
