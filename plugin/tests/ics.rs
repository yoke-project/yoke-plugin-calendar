//! iCalendar read and expanded.

use calendar::ics::{expand, parse};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

const ROME: Tz = chrono_tz::Europe::Rome;

fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

fn cal(body: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
}

#[test]
fn a_weekly_meeting_keeps_its_wall_clock_across_a_change_of_offset() {
    let text = cal("BEGIN:VEVENT\r\nUID:w\r\nSUMMARY:Weekly\r\nDTSTART;TZID=Europe/Rome:20261020T100000\r\nDTEND;TZID=Europe/Rome:20261020T110000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n");
    let (events, unread) = parse(&text, ROME).unwrap();
    assert_eq!(unread, 0);
    let (o, _) = expand(
        &events,
        "s",
        utc(2026, 10, 1, 0, 0),
        utc(2026, 12, 1, 0, 0),
        ROME,
    );
    let starts: Vec<_> = o.iter().map(|o| o.start).collect();
    // 10:00 in Rome: UTC+2 before the last Sunday of October, UTC+1 after it.
    assert_eq!(
        starts,
        vec![
            utc(2026, 10, 20, 8, 0),
            utc(2026, 10, 27, 9, 0),
            utc(2026, 11, 3, 9, 0)
        ]
    );
    assert!(o
        .iter()
        .all(|o| o.end - o.start == chrono::Duration::hours(1) && o.recurring));
}

#[test]
fn an_instance_excluded_moved_or_cancelled_is_applied() {
    let text = cal(concat!(
        "BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Daily\r\nDTSTART:20261102T080000Z\r\nDURATION:PT30M\r\n",
        "RRULE:FREQ=DAILY;COUNT=5\r\nEXDATE:20261103T080000Z\r\nEND:VEVENT\r\n",
        "BEGIN:VEVENT\r\nUID:d\r\nRECURRENCE-ID:20261104T080000Z\r\nDTSTART:20261104T150000Z\r\nDTEND:20261104T153000Z\r\nEND:VEVENT\r\n",
        "BEGIN:VEVENT\r\nUID:d\r\nRECURRENCE-ID:20261105T080000Z\r\nDTSTART:20261105T080000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n",
    ));
    let (events, _) = parse(&text, ROME).unwrap();
    let (o, _) = expand(
        &events,
        "s",
        utc(2026, 11, 1, 0, 0),
        utc(2026, 11, 10, 0, 0),
        ROME,
    );
    let starts: Vec<_> = o.iter().map(|o| o.start).collect();
    assert_eq!(
        starts,
        vec![
            utc(2026, 11, 2, 8, 0),
            utc(2026, 11, 4, 15, 0),
            utc(2026, 11, 6, 8, 0)
        ]
    );
    // The moved instance keeps the series' title and the identity of the instance it replaces.
    assert_eq!(o[1].title, "Daily");
    assert_eq!(
        o[1].id,
        format!("s/d/{}", utc(2026, 11, 4, 8, 0).timestamp_millis())
    );
}

#[test]
fn a_whole_day_is_a_day_in_the_units_zone_and_a_floating_time_is_read_in_it() {
    let text = cal(concat!(
        "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261208\r\nEND:VEVENT\r\n",
        "BEGIN:VEVENT\r\nUID:f\r\nSUMMARY:Floating\r\nDTSTART:20261208T120000\r\nEND:VEVENT\r\n",
    ));
    let (events, _) = parse(&text, ROME).unwrap();
    let (o, _) = expand(
        &events,
        "s",
        utc(2026, 12, 1, 0, 0),
        utc(2026, 12, 31, 0, 0),
        ROME,
    );
    assert_eq!(o[0].title, "Holiday");
    assert!(o[0].all_day);
    assert_eq!(
        (o[0].start, o[0].end),
        (utc(2026, 12, 7, 23, 0), utc(2026, 12, 8, 23, 0))
    );
    assert_eq!(o[1].start, utc(2026, 12, 8, 11, 0));
}

#[test]
fn a_windows_zone_and_a_zone_named_by_its_location_are_resolved() {
    let text = cal(concat!(
        "BEGIN:VTIMEZONE\r\nTZID:/custom/Rome\r\nX-LIC-LOCATION:Europe/Rome\r\nEND:VTIMEZONE\r\n",
        "BEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Outlook\r\nDTSTART;TZID=\"Pacific Standard Time\":20261201T090000\r\nEND:VEVENT\r\n",
        "BEGIN:VEVENT\r\nUID:y\r\nSUMMARY:Custom\r\nDTSTART;TZID=/custom/Rome:20261201T090000\r\nEND:VEVENT\r\n",
    ));
    let (events, _) = parse(&text, chrono_tz::UTC).unwrap();
    let (o, _) = expand(
        &events,
        "s",
        utc(2026, 12, 1, 0, 0),
        utc(2026, 12, 2, 0, 0),
        chrono_tz::UTC,
    );
    let by: Vec<_> = o.iter().map(|o| (o.title.as_str(), o.start)).collect();
    assert!(by.contains(&("Outlook", utc(2026, 12, 1, 17, 0))), "{by:?}");
    assert!(by.contains(&("Custom", utc(2026, 12, 1, 8, 0))), "{by:?}");
}

#[test]
fn folded_lines_and_escapes_are_read_and_an_alarm_inside_is_not_the_event() {
    let text = cal(concat!(
        "BEGIN:VEVENT\r\nUID:z\r\nSUMMARY:Review\\, budget\r\n  and plan\r\nLOCATION:Room 1\\nfloor 2\r\n",
        "DTSTART:20261201T090000Z\r\nBEGIN:VALARM\r\nSUMMARY:not this\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
    ));
    let (events, _) = parse(&text, ROME).unwrap();
    assert_eq!(events[0].summary, "Review, budget and plan");
    assert_eq!(events[0].location.as_deref(), Some("Room 1\nfloor 2"));
}

#[test]
fn an_event_that_cannot_be_placed_is_counted_and_a_text_that_is_not_icalendar_is_refused() {
    let text = cal("BEGIN:VEVENT\r\nUID:bad\r\nDTSTART:tomorrow\r\nEND:VEVENT\r\n");
    assert_eq!(parse(&text, ROME).unwrap(), (vec![], 1));
    assert!(parse("<html>", ROME).unwrap_err().contains("not iCalendar"));
}

#[test]
fn an_event_running_into_the_window_is_in_it() {
    let text = cal("BEGIN:VEVENT\r\nUID:long\r\nSUMMARY:Long\r\nDTSTART:20261130T220000Z\r\nDTEND:20261201T020000Z\r\nEND:VEVENT\r\n");
    let (events, _) = parse(&text, ROME).unwrap();
    let (o, _) = expand(
        &events,
        "s",
        utc(2026, 12, 1, 0, 0),
        utc(2026, 12, 2, 0, 0),
        ROME,
    );
    assert_eq!(o.len(), 1);
}
