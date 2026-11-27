//! The Plugin against servers that answer as they are told, and a Session that records what it is given.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

use calendar::accounts::Tokens;
use calendar::plugin::*;
use calendar::read::Cache;
use calendar::sources::{Credential, Http, Request, Response, Settings, Source};

const ROME: chrono_tz::Tz = chrono_tz::Europe::Rome;

type Answer = Box<dyn Fn(&Request) -> Result<Response, String> + Send + Sync>;

/// A server: what it was sent, and an answer for each request.
struct Server {
    sent: Mutex<Vec<Request>>,
    answer: Answer,
}

#[async_trait]
impl Http for Server {
    async fn send(&self, r: Request) -> Result<Response, String> {
        self.sent.lock().unwrap().push(r.clone());
        (self.answer)(&r)
    }
}

fn ok(body: &str) -> Result<Response, String> {
    Ok(Response {
        status: 200,
        body: body.into(),
    })
}

/// A directory of its own under the system's temporary one, empty.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("calendar-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cal(body: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
}

/// An event starting `minutes` from now, an hour long.
fn soon(uid: &str, title: &str, minutes: i64) -> String {
    let at = Utc::now() + chrono::Duration::minutes(minutes);
    format!(
        "BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{title}\r\nDTSTART:{}\r\nDURATION:PT1H\r\nEND:VEVENT\r\n",
        at.format("%Y%m%dT%H%M%SZ")
    )
}

struct Bench {
    calendar: Arc<Calendar>,
    server: Arc<Server>,
    happened: UnboundedReceiver<Happened>,
}

fn bench(
    sources: &[&str],
    credentials: BTreeMap<String, Credential>,
    cache: PathBuf,
    answer: Answer,
) -> Bench {
    bench_with(sources, credentials, None, cache, answer)
}

fn bench_with(
    sources: &[&str],
    credentials: BTreeMap<String, Credential>,
    tokens: Option<Arc<dyn Tokens>>,
    cache: PathBuf,
    answer: Answer,
) -> Bench {
    let server = Arc::new(Server {
        sent: Mutex::new(vec![]),
        answer,
    });
    let (tx, happened) = tokio::sync::mpsc::unbounded_channel();
    let calendar = Calendar::new(
        Parts {
            http: server.clone(),
            sources: sources.iter().map(|s| Source::parse(s).unwrap()).collect(),
            credentials,
            tokens,
            cache: Cache(cache),
            zone: ROME,
            remind_before: Duration::from_secs(600),
            upcoming: 2,
            horizon: chrono::Duration::days(7),
        },
        tx,
    );
    Bench {
        calendar,
        server,
        happened,
    }
}

fn ask(c: &Calendar, query: &str, payload: Value) -> Result<Value, Error> {
    c.answer(query, payload.to_string().as_bytes())
        .map(|a| serde_json::from_slice(&a).unwrap())
}

fn occurrences(h: &mut UnboundedReceiver<Happened>, want: &str) -> Vec<Value> {
    let mut out = vec![];
    while let Ok(x) = h.try_recv() {
        if let Happened::Occurred {
            occurrence, detail, ..
        } = x
        {
            if occurrence == want {
                out.push(detail);
            }
        }
    }
    out
}

#[tokio::test]
async fn an_ics_source_is_read_and_its_events_answered_and_its_url_is_shown_without_its_query() {
    let text = cal(&soon("a", "Standup", 30));
    let b = bench(
        &["work=ics:https://cal.example.org/team.ics?key=private"],
        BTreeMap::new(),
        scratch("ics"),
        Box::new(move |_| ok(&text)),
    );
    assert_eq!(b.calendar.refresh(None).await.unwrap(), "1 sources read");
    let sources = ask(&b.calendar, SOURCES, json!({})).unwrap();
    assert_eq!(sources["sources"][0]["state"], "ok");
    assert_eq!(
        sources["sources"][0]["location"],
        "https://cal.example.org/team.ics"
    );
    assert!(!sources.to_string().contains("private"));
    let events = ask(&b.calendar, EVENTS, json!({})).unwrap();
    assert_eq!(events["events"][0]["title"], "Standup");
    assert_eq!(events["events"][0]["source"], "work");
    assert_eq!(
        b.server.sent.lock().unwrap()[0].url,
        "https://cal.example.org/team.ics?key=private"
    );
}

#[tokio::test]
async fn a_private_url_and_a_password_come_from_the_credentials() {
    let credentials = BTreeMap::from([(
        "home".to_string(),
        Credential {
            username: Some("ada".into()),
            password: Some("pw".into()),
            url: Some("https://dav.example.org/secret.ics".into()),
        },
    )]);
    let text = cal("");
    let b = bench(
        &["home=ics:"],
        credentials,
        scratch("cred"),
        Box::new(move |_| ok(&text)),
    );
    b.calendar.refresh(None).await.unwrap();
    let sent = b.server.sent.lock().unwrap()[0].clone();
    assert_eq!(sent.url, "https://dav.example.org/secret.ics");
    // ada:pw
    assert!(sent
        .headers
        .contains(&("Authorization".into(), "Basic YWRhOnB3".into())));
    let sources = ask(&b.calendar, SOURCES, json!({})).unwrap();
    assert!(!sources.to_string().contains("secret.ics"));
}

#[tokio::test]
async fn a_source_that_fails_serves_what_was_kept_and_is_said_once() {
    let cache = scratch("kept");
    let text = cal(&soon("k", "Kept", 120));
    let first = bench(
        &["work=ics:https://x/c.ics"],
        BTreeMap::new(),
        cache.clone(),
        Box::new(move |_| ok(&text)),
    );
    first.calendar.refresh(None).await.unwrap();
    // A unit started again, its source no longer answering.
    let mut b = bench(
        &["work=ics:https://x/c.ics"],
        BTreeMap::new(),
        cache,
        Box::new(|_| {
            Ok(Response {
                status: 503,
                body: String::new(),
            })
        }),
    );
    assert_eq!(
        ask(&b.calendar, EVENTS, json!({})).unwrap()["events"][0]["title"],
        "Kept"
    );
    let line = b.calendar.refresh(None).await.unwrap();
    assert!(line.contains("could not be: work"), "{line}");
    b.calendar.refresh(None).await.unwrap();
    let said = occurrences(&mut b.happened, FAILED);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0]["stale"], true);
    assert!(said[0]["line"].as_str().unwrap().contains("503"));
    let sources = ask(&b.calendar, SOURCES, json!({})).unwrap();
    assert_eq!(sources["sources"][0]["state"], "stale");
    assert_eq!(
        ask(&b.calendar, EVENTS, json!({})).unwrap()["events"][0]["title"],
        "Kept"
    );
    match b.calendar.health() {
        Happened::Health { grade, line } => assert_eq!((grade, line.contains("work")), (30, true)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_source_never_read_is_failed_and_the_unit_says_none_can_be_read() {
    let b = bench(
        &["work=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("never"),
        Box::new(|_| Err("connection refused".into())),
    );
    b.calendar.refresh(None).await.unwrap();
    let sources = ask(&b.calendar, SOURCES, json!({})).unwrap();
    assert_eq!(sources["sources"][0]["state"], "failed");
    assert_eq!(sources["sources"][0]["line"], "connection refused");
    match b.calendar.health() {
        Happened::Health { grade, .. } => assert_eq!(grade, 50),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_directory_is_read_file_by_file_and_a_file_that_is_not_icalendar_is_counted() {
    let dir = scratch("dir");
    std::fs::write(dir.join("a.ics"), cal(&soon("a", "From a", 60))).unwrap();
    std::fs::write(dir.join("b.ics"), cal(&soon("b", "From b", 90))).unwrap();
    std::fs::write(dir.join("c.ics"), "not a calendar").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
    let rule = format!("local=dir:{}", dir.display());
    let b = bench(
        &[rule.as_str()],
        BTreeMap::new(),
        scratch("dir-cache"),
        Box::new(|_| Err("no network".into())),
    );
    b.calendar.refresh(None).await.unwrap();
    let events = ask(&b.calendar, EVENTS, json!({})).unwrap();
    let titles: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["From a", "From b"]);
    assert_eq!(
        ask(&b.calendar, SOURCES, json!({})).unwrap()["sources"][0]["unread"],
        1
    );
}

#[tokio::test]
async fn days_are_the_units_days_and_a_whole_day_is_in_its_own() {
    let text = cal(concat!(
        "BEGIN:VEVENT\r\nUID:h\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261208\r\nEND:VEVENT\r\n",
        // 23:30 in Rome on the 8th is 22:30 UTC: the 8th, not the 9th.
        "BEGIN:VEVENT\r\nUID:l\r\nSUMMARY:Late\r\nDTSTART:20261208T223000Z\r\nDURATION:PT15M\r\nEND:VEVENT\r\n",
    ));
    let b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("days"),
        Box::new(move |_| ok(&text)),
    );
    b.calendar.refresh(None).await.unwrap();
    let days = ask(
        &b.calendar,
        DAYS,
        json!({ "from": "2026-12-07", "days": 3 }),
    )
    .unwrap();
    let titles = |i: usize| -> Vec<String> {
        days["days"][i]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["title"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(days["days"][1]["date"], "2026-12-08");
    assert!(titles(0).is_empty());
    assert_eq!(titles(1), vec!["Holiday", "Late"]);
    assert!(titles(2).is_empty());
    assert_eq!(days["days"][1]["events"][0]["date"], "2026-12-08");
}

#[tokio::test]
async fn what_is_refused_is_refused_by_name() {
    let b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("refused"),
        Box::new(|_| ok(&cal(""))),
    );
    assert!(matches!(
        b.calendar.act(REFRESH, br#"{"source":"other"}"#).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        ask(&b.calendar, EVENTS, json!({ "sources": ["other"] })),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        ask(&b.calendar, EVENTS, json!({ "from": 10, "to": 5 })),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        ask(
            &b.calendar,
            EVENTS,
            json!({ "from": 0, "to": 401_i64 * 86_400_000 })
        ),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        ask(&b.calendar, DAYS, json!({ "days": 0 })),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        ask(&b.calendar, DAYS, json!({ "from": "tomorrow" })),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        ask(&b.calendar, SOURCES, json!({ "x": 1 })),
        Err(Error::Invalid(_))
    ));
}

#[tokio::test]
async fn the_stream_carries_what_is_on_and_what_is_next_and_an_event_starting_is_said_once_without_its_title(
) {
    let text = cal(&[
        soon("on", "On now", -10),
        soon("n1", "Next one", 5),
        soon("n2", "Later", 120),
        soon("n3", "Last", 240),
    ]
    .concat());
    let mut b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("stream"),
        Box::new(move |_| ok(&text)),
    );
    b.calendar.refresh(None).await.unwrap();
    let record = b.calendar.record();
    assert_eq!(record["now"][0]["title"], "On now");
    let next: Vec<&str> = record["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(next, vec!["Next one", "Later"]);
    b.calendar.changed(Utc::now());
    let starting = occurrences(&mut b.happened, STARTING);
    assert_eq!(starting.len(), 1, "{starting:?}");
    assert!(starting[0]["event"].as_str().unwrap().starts_with("s/n1/"));
    assert!(
        !starting[0].to_string().contains("Next one"),
        "an occurrence carries no title"
    );
    b.calendar.changed(Utc::now());
    assert!(occurrences(&mut b.happened, STARTING).is_empty());
}

#[tokio::test]
async fn the_stream_changes_only_when_what_it_carries_does() {
    let text = cal(&soon("x", "X", 60));
    let mut b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("seq"),
        Box::new(move |_| ok(&text)),
    );
    b.calendar.refresh(None).await.unwrap();
    let first = b.calendar.record()["sequence"].clone();
    while b.happened.try_recv().is_ok() {}
    b.calendar.changed(Utc::now());
    b.calendar.refresh(None).await.unwrap();
    assert_eq!(b.calendar.record()["sequence"], first);
    let changed = std::iter::from_fn(|| b.happened.try_recv().ok())
        .filter(|h| *h == Happened::Changed)
        .count();
    assert_eq!(changed, 0);
}

#[tokio::test]
async fn one_reading_at_a_time() {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("one"),
        Box::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            ok(&cal(""))
        }),
    );
    let (x, y) = tokio::join!(b.calendar.refresh(None), b.calendar.refresh(None));
    assert!(x.is_ok() && y.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn the_arguments_name_the_sources_and_a_source_through_accounts_needs_the_channel() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let s = Settings::parse(
        args(&[
            "--source",
            "w=ics:https://x",
            "--source",
            "m=microsoft:ada@example.org/Work",
            "--credentials",
            "/c",
            "--cache",
            "/k",
            "--accounts",
            "/a.sock",
            "--zone",
            "Europe/Rome",
        ]),
        chrono_tz::UTC,
    )
    .unwrap();
    assert_eq!(s.sources.len(), 2);
    assert_eq!(s.zone, ROME);
    let missing = Settings::parse(
        args(&[
            "--source",
            "m=google:ada@example.org",
            "--credentials",
            "/c",
            "--cache",
            "/k",
        ]),
        chrono_tz::UTC,
    );
    assert!(missing.unwrap_err().contains("--accounts"));
    let twice = Settings::parse(
        args(&[
            "--source",
            "a=ics:x",
            "--source",
            "a=ics:y",
            "--credentials",
            "/c",
            "--cache",
            "/k",
        ]),
        chrono_tz::UTC,
    );
    assert!(twice.unwrap_err().contains("two sources"));
    assert!(Source::parse("x=webcal:foo")
        .unwrap_err()
        .contains("not a kind"));
    assert!(Settings::parse(args(&["--zone", "Mars/Olympus"]), chrono_tz::UTC).is_err());
}

#[tokio::test]
async fn a_caldav_collection_is_asked_for_a_time_range_and_every_calendar_data_is_read() {
    let one = cal(&soon("c1", "Escaped &amp; read", 30)).replace("\r\n", "&#13;\n");
    let two = cal(&soon("c2", "In CDATA", 60));
    let multistatus = format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">"#,
            r#"<d:response><d:href>/c/1.ics</d:href><d:propstat><d:prop><cal:calendar-data>{}</cal:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
            r#"<d:response><d:href>/c/2.ics</d:href><d:propstat><d:prop><cal:calendar-data><![CDATA[{}]]></cal:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#,
            r#"</d:multistatus>"#
        ),
        one, two
    );
    let b = bench(
        &["team=caldav:https://dav.example.org/cal/team/"],
        BTreeMap::new(),
        scratch("caldav"),
        Box::new(move |_| {
            Ok(Response {
                status: 207,
                body: multistatus.clone(),
            })
        }),
    );
    assert_eq!(b.calendar.refresh(None).await.unwrap(), "1 sources read");
    let sent = b.server.sent.lock().unwrap()[0].clone();
    assert_eq!(sent.method, "REPORT");
    assert!(sent.headers.contains(&("Depth".into(), "1".into())));
    assert!(sent.body.unwrap().contains("<C:time-range start="));
    let events = ask(&b.calendar, EVENTS, json!({})).unwrap();
    let titles: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["Escaped & read", "In CDATA"]);
}

#[tokio::test]
async fn a_caldav_server_refusing_the_credentials_is_said_so() {
    let b = bench(
        &["team=caldav:https://dav.example.org/cal/team/"],
        BTreeMap::new(),
        scratch("caldav-401"),
        Box::new(|_| {
            Ok(Response {
                status: 401,
                body: String::new(),
            })
        }),
    );
    b.calendar.refresh(None).await.unwrap();
    let line = ask(&b.calendar, SOURCES, json!({})).unwrap()["sources"][0]["line"].clone();
    assert!(
        line.as_str().unwrap().contains("credentials are refused"),
        "{line}"
    );
}

/// `online-accounts`: what it was asked, and a token or a refusal.
struct Accounts {
    asked: Mutex<Vec<(String, Vec<String>)>>,
    refuse: Option<String>,
}

#[async_trait]
impl Tokens for Accounts {
    async fn token(&self, account: &str, scopes: &[&str]) -> Result<String, String> {
        self.asked.lock().unwrap().push((
            account.into(),
            scopes.iter().map(|s| s.to_string()).collect(),
        ));
        match &self.refuse {
            Some(why) => Err(why.clone()),
            None => Ok("the-token".into()),
        }
    }
}

fn accounts(refuse: Option<&str>) -> Arc<Accounts> {
    Arc::new(Accounts {
        asked: Mutex::new(vec![]),
        refuse: refuse.map(str::to_string),
    })
}

#[tokio::test]
async fn a_microsoft_calendar_is_read_with_a_token_page_by_page_and_kept() {
    let tokens = accounts(None);
    let cache = scratch("graph");
    let answer: Answer = Box::new(|r| {
        assert!(r
            .headers
            .contains(&("Authorization".into(), "Bearer the-token".into())));
        assert!(r
            .headers
            .contains(&("Prefer".into(), "outlook.timezone=\"UTC\"".into())));
        if r.url.contains("/me/calendars?") {
            return ok(r#"{"value":[{"id":"AAA=","name":"Work"}]}"#);
        }
        if r.url.contains("page=2") {
            return ok(
                r#"{"value":[{"id":"3","iCalUId":"u3","subject":"Holiday","isAllDay":true,"type":"singleInstance","start":{"dateTime":"2026-12-08T00:00:00.0000000","timeZone":"UTC"},"end":{"dateTime":"2026-12-09T00:00:00.0000000","timeZone":"UTC"}}]}"#,
            );
        }
        assert!(
            r.url
                .starts_with("https://graph.microsoft.com/v1.0/me/calendars/AAA%3D/calendarView?"),
            "{}",
            r.url
        );
        ok(r#"{"value":[
            {"id":"1","iCalUId":"u1","subject":"Review","type":"occurrence","location":{"displayName":"Room 4"},"start":{"dateTime":"2026-12-01T09:00:00.0000000","timeZone":"UTC"},"end":{"dateTime":"2026-12-01T10:00:00.0000000","timeZone":"UTC"}},
            {"id":"2","iCalUId":"u2","subject":"Gone","isCancelled":true,"start":{"dateTime":"2026-12-01T11:00:00.0000000"},"end":{"dateTime":"2026-12-01T12:00:00.0000000"}}
          ],"@odata.nextLink":"https://graph.microsoft.com/v1.0/next?page=2"}"#)
    });
    let b = bench_with(
        &["work=microsoft:ada@example.org/Work"],
        BTreeMap::new(),
        Some(tokens.clone()),
        cache.clone(),
        answer,
    );
    assert_eq!(b.calendar.refresh(None).await.unwrap(), "1 sources read");
    assert_eq!(
        tokens.asked.lock().unwrap()[0],
        (
            "microsoft:ada@example.org".to_string(),
            vec!["Calendars.Read".to_string()]
        )
    );
    let from = Utc
        .with_ymd_and_hms(2026, 12, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis();
    let window = json!({ "from": from, "to": from + 30 * 86_400_000_i64 });
    let events = ask(&b.calendar, EVENTS, window.clone()).unwrap();
    let got: Vec<(&str, bool)> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["title"].as_str().unwrap(),
                e["all_day"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, vec![("Review", false), ("Holiday", true)]);
    assert_eq!(events["events"][0]["location"], "Room 4");
    assert_eq!(events["events"][0]["recurring"], true);
    // The whole day is the 8th in Rome.
    assert_eq!(events["events"][1]["date"], "2026-12-08");
    // Started again with online-accounts refusing: what was kept is served, and why it is not read is said.
    let refused = accounts(Some("online-accounts refused the token, sign_in_again: microsoft:ada@example.org must sign in again"));
    let again = bench_with(
        &["work=microsoft:ada@example.org/Work"],
        BTreeMap::new(),
        Some(refused),
        cache,
        Box::new(|_| panic!("no request without a token")),
    );
    assert_eq!(
        ask(&again.calendar, EVENTS, window).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    again.calendar.refresh(None).await.unwrap();
    let source = &ask(&again.calendar, SOURCES, json!({})).unwrap()["sources"][0];
    assert_eq!(source["state"], "stale");
    assert!(source["line"].as_str().unwrap().contains("sign_in_again"));
}

#[tokio::test]
async fn a_google_calendar_is_read_with_a_token_page_by_page() {
    let tokens = accounts(None);
    let answer: Answer = Box::new(|r| {
        assert!(r.url.starts_with("https://www.googleapis.com/calendar/v3/calendars/team%40group.calendar.google.com/events?singleEvents=true"), "{}", r.url);
        if r.url.contains("pageToken=p2") {
            return ok(
                r#"{"items":[{"id":"c","iCalUID":"c@google.com","summary":"Off","start":{"date":"2026-12-08"},"end":{"date":"2026-12-09"}}]}"#,
            );
        }
        ok(r#"{"items":[
            {"id":"a","iCalUID":"a@google.com","summary":"Planning","recurringEventId":"s","start":{"dateTime":"2026-12-01T10:00:00+01:00"},"end":{"dateTime":"2026-12-01T11:00:00+01:00"}},
            {"id":"b","status":"cancelled","summary":"Gone","start":{"dateTime":"2026-12-01T12:00:00Z"},"end":{"dateTime":"2026-12-01T13:00:00Z"}}
          ],"nextPageToken":"p2"}"#)
    });
    let b = bench_with(
        &["team=google:ada@example.org/team@group.calendar.google.com"],
        BTreeMap::new(),
        Some(tokens.clone()),
        scratch("google"),
        answer,
    );
    b.calendar.refresh(None).await.unwrap();
    assert_eq!(
        tokens.asked.lock().unwrap()[0].1,
        vec!["https://www.googleapis.com/auth/calendar.readonly".to_string()]
    );
    let from = Utc
        .with_ymd_and_hms(2026, 12, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis();
    let events = ask(
        &b.calendar,
        EVENTS,
        json!({ "from": from, "to": from + 30 * 86_400_000_i64 }),
    )
    .unwrap();
    let titles: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["Planning", "Off"]);
    assert_eq!(
        events["events"][0]["start"],
        Utc.with_ymd_and_hms(2026, 12, 1, 9, 0, 0)
            .unwrap()
            .timestamp_millis()
    );
    assert_eq!(events["events"][0]["recurring"], true);
}

#[tokio::test]
async fn a_source_through_accounts_with_none_bound_is_said_so() {
    let b = bench(
        &["m=microsoft:ada@example.org"],
        BTreeMap::new(),
        scratch("unbound"),
        Box::new(|_| panic!("no request")),
    );
    b.calendar.refresh(None).await.unwrap();
    let line = ask(&b.calendar, SOURCES, json!({})).unwrap()["sources"][0]["line"].clone();
    assert!(line.as_str().unwrap().contains("not bound"), "{line}");
}

#[tokio::test]
async fn a_change_beyond_what_the_stream_carries_raises_its_revision() {
    let later = |title: &str| {
        let at = Utc::now() + chrono::Duration::days(30);
        cal(&format!(
            "BEGIN:VEVENT\r\nUID:far\r\nSUMMARY:{title}\r\nDTSTART:{}\r\nDURATION:PT1H\r\nEND:VEVENT\r\n",
            at.format("%Y%m%dT%H%M%SZ")
        ))
    };
    let text = Arc::new(Mutex::new(later("Far")));
    let serving = text.clone();
    let mut b = bench(
        &["s=ics:https://x/c.ics"],
        BTreeMap::new(),
        scratch("revision"),
        Box::new(move |_| ok(&serving.lock().unwrap())),
    );
    b.calendar.refresh(None).await.unwrap();
    let first = b.calendar.record();
    assert!(
        first["next"].as_array().unwrap().is_empty(),
        "the event is beyond the horizon"
    );
    while b.happened.try_recv().is_ok() {}

    *text.lock().unwrap() = later("Moved");
    b.calendar.refresh(None).await.unwrap();
    let second = b.calendar.record();
    assert!(
        second["revision"].as_u64() > first["revision"].as_u64(),
        "{second}"
    );
    assert_ne!(second["sequence"], first["sequence"]);
    assert!(std::iter::from_fn(|| b.happened.try_recv().ok()).any(|h| h == Happened::Changed));

    b.calendar.refresh(None).await.unwrap();
    assert_eq!(
        b.calendar.record()["revision"],
        second["revision"],
        "a read that changes nothing"
    );
}
