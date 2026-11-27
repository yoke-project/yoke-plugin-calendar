# calendar

A Yoke Plugin that reads calendars: iCalendar documents at a URL, CalDAV collections, a directory of `.ics`
files, and Microsoft and Google calendars through `online-accounts`. It expands every recurrence in the
event's own time zone, keeps what it last read of each source, and answers what is on, what is next and
what a day holds — for a meeting-room panel, a reception display, a kiosk or a desktop shell.

| | |
| --- | --- |
| questions | `calendar.sources` `{}` (every source and where it stands), `calendar.events` `{from?, to?, sources?}` (the occurrences in a window), `calendar.days` `{from?, days?, sources?}` (the same by day) |
| commands | `calendar.refresh` `{source?}` |
| streams | `calendar.upcoming`: what is on now and what is next, again at every change, with a `revision` raised whenever a read changes what a source holds, so that a client showing more — a month of `calendar.days` — asks again then and not on a clock |
| occurrences | `calendar.event.starting` (10), `calendar.source.failed` (30) |
| needs | `secret:credentials`, `storage:cache`, `unit:accounts` — the last bound to an `online-accounts` unit |

Every payload is JSON and states `schema`. An instant is milliseconds since the epoch; a day is `YYYY-MM-DD`
in the unit's zone.

**The sources are the unit's arguments**, each `--source <id>=<kind>:<location>`:

| Kind | Location | Read |
| --- | --- | --- |
| `ics` | a URL, or nothing where the credentials give it | `GET`, with Basic authentication where the credentials give a username and a password |
| `caldav` | a calendar collection's URL | `REPORT calendar-query` for the events with an instance from a month ago to 400 days ahead |
| `dir` | a directory on the host | every `.ics` file in it |
| `microsoft` | `<account>[/<calendar>]` | Microsoft Graph's `calendarView`, the calendar by its name or its id, the default one where none is named |
| `google` | `<account>[/<calendar>]` | Google Calendar's events, the calendar by its id, `primary` where none is named |

    --source team=caldav:https://dav.example.org/cal/team/ --source holidays=ics:https://example.org/it.ics
    --source work=microsoft:ada@example.org/Calendar --source home=google:ada@example.org

An account is the one `online-accounts` holds, without its provider: `microsoft:ada@example.org` is read as
`--source work=microsoft:ada@example.org`, with a token carrying `Calendars.Read`, and a Google account with
one carrying `calendar.readonly`; the person signs in once, in `online-accounts`, asking for that scope.
Microsoft and Google are read from a month ago to 180 days ahead.

**The credentials are the deployment's**, in the secret `credentials`, by source:

```json
{
  "team": { "username": "ada", "password": "…" },
  "holidays": { "url": "https://example.org/private/…/basic.ics" }
}
```

A `url` keeps a private calendar's address out of the unit's arguments: the source is then `--source
holidays=ics:`. A source is shown by its URL's scheme, host and path and never its query. The secret is
supplied as `{}` where no source needs one.

**A recurrence is expanded in the event's own zone**: a weekly meeting at 10:00 in Rome stays at 10:00 across
a change of offset. A zone is an IANA name, one a `VTIMEZONE` names by `X-LIC-LOCATION`, or a Windows name
Outlook writes; a time with no zone, and one none of these resolves, is read in the unit's zone. An instance
excluded, moved or cancelled is applied. A whole day is a day in the unit's zone.

| Argument | |
| --- | --- |
| `--credentials ${bind.credentials}` | the sources' credentials |
| `--cache ${bind.cache}` | where what was last read of each source is kept |
| `--accounts ${bind.accounts}` | the channel to `online-accounts` |
| `--source <id>=<kind>:<location>` | repeated |
| `--zone <IANA name>` | the unit's zone; the host's where absent |
| `--refresh-s N` | how often every source is read again (900, at least 60) |
| `--remind-before-s N` | how long before an event starts `calendar.event.starting` is said (300) |
| `--upcoming N` | how many events the stream carries as next (5) |
| `--horizon-days N` | how far ahead the stream looks (7) |

**A source that cannot be read is served from what was kept**, and stands `stale`; one never read stands
`failed`. `calendar.source.failed` says so once, until the source is read again, and the unit's health names
every source that cannot be read. **`calendar.event.starting` says which event starts and when, never its
title**: an occurrence is kept by the Core, and a title is the calendar's own text, which travels on the
stream and in the answers alone. A whole-day event is not said to start.

**Every deployment binds the need `accounts`**, since a need is never optional: one with no Microsoft or
Google source binds an `online-accounts` unit given no client, which holds no account and is never asked.
The operator grants the use of `oauth.token`, `query.oauth.token.ask`, besides what each Plugin answers.

Not yet: writing to a calendar, attendees, and free and busy times.

**It needs Yoke 0.17.0**, the release in which a unit uses another; until that release is published the Plugin
builds on the Rust library at the commit that carries it.

Take it into an application with `yoke-compose add plugin https://github.com/yoke-project/yoke-plugin-calendar@<tag> --bind accounts=<the online-accounts unit>`,
give its unit the arguments above, set the secret on the instance, and grant both Plugins. Build it with
`yoke-compose build`, and try it from `try/`.

Licensed under the Apache License 2.0.
