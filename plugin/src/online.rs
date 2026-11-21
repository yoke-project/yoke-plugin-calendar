//! Microsoft Graph and Google Calendar, read with a token `online-accounts` answers. Both expand a series
//! themselves, so what they answer is occurrences, asked for in UTC.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::Value;

use crate::accounts::Tokens;
use crate::ics::Occurrence;
use crate::sources::{Http, Request};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
pub const GOOGLE: &str = "https://www.googleapis.com/calendar/v3";
pub const GRAPH_SCOPE: &str = "Calendars.Read";
pub const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// The span a provider is asked for, around now.
pub const BEFORE: chrono::Duration = chrono::Duration::days(31);
pub const AFTER: chrono::Duration = chrono::Duration::days(180);
/// The most pages followed for one reading.
const PAGES: usize = 20;

fn pct(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn get(http: &dyn Http, url: &str, token: &str, who: &str) -> Result<Value, String> {
    let r = http
        .send(
            Request::get(url)
                .header("Authorization", &format!("Bearer {token}"))
                .header("Accept", "application/json")
                .header("Prefer", "outlook.timezone=\"UTC\""),
        )
        .await?;
    match r.status {
        200..=299 => serde_json::from_str(&r.body)
            .map_err(|_| format!("{who} answered something that is not JSON")),
        401 => Err(format!("{who} refused the token")),
        403 => Err(format!(
            "{who} answered 403: the account was not granted the calendar"
        )),
        404 => Err(format!("{who} answered 404: there is no such calendar")),
        s => Err(format!("{who} answered {s}")),
    }
}

/// A Microsoft account's calendar: its default one, or the one named by its name or its id.
pub async fn microsoft(
    http: &dyn Http,
    tokens: &dyn Tokens,
    source: &str,
    account: &str,
    calendar: Option<&str>,
    zone: Tz,
) -> Result<Vec<Occurrence>, String> {
    let token = tokens
        .token(&format!("microsoft:{account}"), &[GRAPH_SCOPE])
        .await?;
    let base = match calendar {
        None => format!("{GRAPH}/me"),
        Some(c) => {
            let listed = get(
                http,
                &format!("{GRAPH}/me/calendars?$select=id,name&$top=100"),
                &token,
                "Microsoft",
            )
            .await?;
            let id = listed["value"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|x| x["name"] == c || x["id"] == c)
                .and_then(|x| x["id"].as_str())
                .ok_or_else(|| format!("the account has no calendar {c}"))?
                .to_string();
            format!("{GRAPH}/me/calendars/{}", pct(&id))
        }
    };
    let now = Utc::now();
    let f = "%Y-%m-%dT%H:%M:%SZ";
    let mut url = format!(
        "{base}/calendarView?startDateTime={}&endDateTime={}&$top=200&$select=id,iCalUId,subject,location,start,end,isAllDay,isCancelled,type",
        pct(&(now - BEFORE).format(f).to_string()),
        pct(&(now + AFTER).format(f).to_string())
    );
    let mut out = Vec::new();
    for _ in 0..PAGES {
        let page = get(http, &url, &token, "Microsoft").await?;
        for e in page["value"].as_array().into_iter().flatten() {
            if e["isCancelled"] == true {
                continue;
            }
            let all_day = e["isAllDay"] == true;
            let (Some(start), Some(end)) = (
                graph_time(&e["start"], all_day, zone),
                graph_time(&e["end"], all_day, zone),
            ) else {
                continue;
            };
            let uid = e["iCalUId"]
                .as_str()
                .or(e["id"].as_str())
                .unwrap_or_default();
            out.push(Occurrence {
                id: format!("{source}/{uid}/{}", start.timestamp_millis()),
                source: source.into(),
                uid: uid.into(),
                title: e["subject"].as_str().unwrap_or_default().into(),
                location: e["location"]["displayName"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string),
                start,
                end,
                all_day,
                recurring: e["type"] != "singleInstance",
            });
        }
        match page["@odata.nextLink"].as_str() {
            Some(next) => url = next.to_string(),
            None => return Ok(out),
        }
    }
    Ok(out)
}

/// `{dateTime, timeZone}`, the zone UTC as asked; a whole day is its date, placed in the unit's zone.
fn graph_time(v: &Value, all_day: bool, zone: Tz) -> Option<DateTime<Utc>> {
    let s = v["dateTime"].as_str()?;
    let n = NaiveDateTime::parse_from_str(s.split('.').next()?, "%Y-%m-%dT%H:%M:%S").ok()?;
    if all_day {
        return Some(midnight(zone, n.date()));
    }
    Some(Utc.from_utc_datetime(&n))
}

fn midnight(zone: Tz, d: NaiveDate) -> DateTime<Utc> {
    let n = d.and_hms_opt(0, 0, 0).unwrap();
    zone.from_local_datetime(&n)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&n))
}

/// A Google account's calendar: `primary`, or the one named by its id.
pub async fn google(
    http: &dyn Http,
    tokens: &dyn Tokens,
    source: &str,
    account: &str,
    calendar: Option<&str>,
    zone: Tz,
) -> Result<Vec<Occurrence>, String> {
    let token = tokens
        .token(&format!("google:{account}"), &[GOOGLE_SCOPE])
        .await?;
    let now = Utc::now();
    let base = format!(
        "{GOOGLE}/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults=2500&timeMin={}&timeMax={}",
        pct(calendar.unwrap_or("primary")),
        pct(&(now - BEFORE).to_rfc3339()),
        pct(&(now + AFTER).to_rfc3339())
    );
    let (mut out, mut url) = (Vec::new(), base.clone());
    for _ in 0..PAGES {
        let page = get(http, &url, &token, "Google").await?;
        for e in page["items"].as_array().into_iter().flatten() {
            if e["status"] == "cancelled" {
                continue;
            }
            let all_day = e["start"]["date"].is_string();
            let (Some(start), Some(end)) =
                (google_time(&e["start"], zone), google_time(&e["end"], zone))
            else {
                continue;
            };
            let uid = e["iCalUID"]
                .as_str()
                .or(e["id"].as_str())
                .unwrap_or_default();
            out.push(Occurrence {
                id: format!("{source}/{uid}/{}", start.timestamp_millis()),
                source: source.into(),
                uid: uid.into(),
                title: e["summary"].as_str().unwrap_or_default().into(),
                location: e["location"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string),
                start,
                end,
                all_day,
                recurring: e["recurringEventId"].is_string(),
            });
        }
        match page["nextPageToken"].as_str() {
            Some(next) => url = format!("{base}&pageToken={}", pct(next)),
            None => return Ok(out),
        }
    }
    Ok(out)
}

/// `{dateTime}` in RFC 3339, or `{date}` for a whole day, placed in the unit's zone.
fn google_time(v: &Value, zone: Tz) -> Option<DateTime<Utc>> {
    if let Some(d) = v["date"].as_str() {
        return NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .ok()
            .map(|d| midnight(zone, d));
    }
    DateTime::parse_from_rfc3339(v["dateTime"].as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}
