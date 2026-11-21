//! The sources a calendar is read from, as the unit's arguments name them, the credentials the deployment
//! supplies for them, and how each is read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono_tz::Tz;
use serde::Deserialize;

/// Where a source's events come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// An iCalendar document at a URL; an empty one is the credentials' `url`.
    Ics(String),
    /// A CalDAV calendar collection.
    CalDav(String),
    /// Every `.ics` file of a directory on the host.
    Dir(PathBuf),
    /// A Microsoft account's calendar, through `online-accounts`; its default calendar where none is named.
    Microsoft {
        account: String,
        calendar: Option<String>,
    },
    /// A Google account's calendar, through `online-accounts`; `primary` where none is named.
    Google {
        account: String,
        calendar: Option<String>,
    },
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Ics(_) => "ics",
            Kind::CalDav(_) => "caldav",
            Kind::Dir(_) => "dir",
            Kind::Microsoft { .. } => "microsoft",
            Kind::Google { .. } => "google",
        }
    }

    /// Whether the source is reached through `online-accounts`.
    pub fn uses_accounts(&self) -> bool {
        matches!(self, Kind::Microsoft { .. } | Kind::Google { .. })
    }

    /// Where the source is, as it may be shown: a URL's scheme, host and path, never its query, which may
    /// carry a private calendar's key.
    pub fn shown(&self) -> String {
        match self {
            Kind::Ics(u) | Kind::CalDav(u) => match url::Url::parse(u) {
                Ok(u) => format!(
                    "{}://{}{}",
                    u.scheme(),
                    u.host_str().unwrap_or(""),
                    u.path()
                ),
                Err(_) if u.is_empty() => "the credentials' url".into(),
                Err(_) => "a URL that cannot be read".into(),
            },
            Kind::Dir(p) => p.display().to_string(),
            Kind::Microsoft { account, calendar } | Kind::Google { account, calendar } => {
                match calendar {
                    Some(c) => format!("{account}/{c}"),
                    None => account.clone(),
                }
            }
        }
    }
}

/// A source: its identity, chosen by the deployment, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub kind: Kind,
}

impl Source {
    /// `<id>=<kind>:<location>`: `ics:<url>`, `caldav:<url>`, `dir:<path>`, `microsoft:<account>[/<calendar>]`,
    /// `google:<account>[/<calendar>]`. An account is the identity `online-accounts` holds it by, without
    /// its provider: `microsoft:ada@example.org` is the account `microsoft:ada@example.org`.
    pub fn parse(rule: &str) -> Result<Source, String> {
        let wrong = || format!("--source takes <id>=<kind>:<location>, and {rule} is not one");
        let (id, rest) = rule.split_once('=').ok_or_else(wrong)?;
        let (kind, location) = rest.split_once(':').ok_or_else(wrong)?;
        if id.is_empty() || id.contains(['/', ' ']) {
            return Err(format!(
                "a source's id is a word with no slash or space, and {id:?} is not one"
            ));
        }
        let account = |location: &str| -> Result<(String, Option<String>), String> {
            // An account may hold a slash nowhere, so the first one starts the calendar.
            let (account, calendar) = match location.split_once('/') {
                Some((a, c)) => (a, Some(c.to_string()).filter(|c| !c.is_empty())),
                None => (location, None),
            };
            if account.is_empty() {
                return Err(format!("{rule} names no account"));
            }
            Ok((account.to_string(), calendar))
        };
        let kind = match kind {
            "ics" => Kind::Ics(location.to_string()),
            "caldav" if !location.is_empty() => Kind::CalDav(location.to_string()),
            "dir" if !location.is_empty() => Kind::Dir(PathBuf::from(location)),
            "microsoft" => {
                let (account, calendar) = account(location)?;
                Kind::Microsoft { account, calendar }
            }
            "google" => {
                let (account, calendar) = account(location)?;
                Kind::Google { account, calendar }
            }
            "caldav" | "dir" => return Err(format!("{rule} names no location")),
            other => {
                return Err(format!(
                    "{other} is not a kind of source: ics, caldav, dir, microsoft or google"
                ))
            }
        };
        Ok(Source {
            id: id.to_string(),
            kind,
        })
    }
}

/// What the deployment supplies for a source, in the secret `credentials`, by the source's id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub username: Option<String>,
    pub password: Option<String>,
    /// The URL itself, where it is a private calendar's and the unit's arguments should not show it.
    pub url: Option<String>,
}

pub fn credentials(text: &str) -> Result<BTreeMap<String, Credential>, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("the credentials are not what this Plugin reads: {e}"))
}

/// What the unit's arguments choose.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub sources: Vec<Source>,
    pub credentials: PathBuf,
    pub cache: PathBuf,
    /// The channel of the need `accounts`, where a source is reached through `online-accounts`.
    pub accounts: Option<String>,
    pub zone: Tz,
    pub refresh: Duration,
    pub remind_before: Duration,
    pub upcoming: usize,
    pub horizon: chrono::Duration,
}

pub const REFRESH_S: u64 = 900;
pub const REMIND_BEFORE_S: u64 = 300;
pub const UPCOMING: usize = 5;
pub const HORIZON_DAYS: i64 = 7;

impl Settings {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        host_zone: Tz,
    ) -> Result<Settings, String> {
        let (mut credentials, mut cache) = (None, None);
        let mut s = Settings {
            sources: vec![],
            credentials: PathBuf::new(),
            cache: PathBuf::new(),
            accounts: None,
            zone: host_zone,
            refresh: Duration::from_secs(REFRESH_S),
            remind_before: Duration::from_secs(REMIND_BEFORE_S),
            upcoming: UPCOMING,
            horizon: chrono::Duration::days(HORIZON_DAYS),
        };
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or(format!("{flag} takes a value"));
            let number = |v: String| -> Result<u64, String> {
                v.parse().map_err(|_| format!("{flag} takes a number"))
            };
            match flag.as_str() {
                "--source" => {
                    let source = Source::parse(&value()?)?;
                    if s.sources.iter().any(|x| x.id == source.id) {
                        return Err(format!("two sources are named {}", source.id));
                    }
                    s.sources.push(source);
                }
                "--credentials" => credentials = Some(PathBuf::from(value()?)),
                "--cache" => cache = Some(PathBuf::from(value()?)),
                "--accounts" => s.accounts = Some(value()?),
                "--zone" => {
                    let v = value()?;
                    s.zone = v
                        .parse()
                        .map_err(|_| format!("{v} is not a time zone: an IANA name, Europe/Rome"))?
                }
                "--refresh-s" => s.refresh = Duration::from_secs(number(value()?)?.max(60)),
                "--remind-before-s" => s.remind_before = Duration::from_secs(number(value()?)?),
                "--upcoming" => s.upcoming = number(value()?)? as usize,
                "--horizon-days" => s.horizon = chrono::Duration::days(number(value()?)? as i64),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        s.credentials =
            credentials.ok_or("--credentials names the secret credentials: ${bind.credentials}")?;
        s.cache = cache.ok_or("--cache names the storage cache: ${bind.cache}")?;
        if s.accounts.is_none() {
            if let Some(src) = s.sources.iter().find(|s| s.kind.uses_accounts()) {
                return Err(format!(
                    "the source {} is read through online-accounts, and --accounts names its channel: ${{bind.accounts}}",
                    src.id
                ));
            }
        }
        Ok(s)
    }
}

/// The host's zone: `TZ`, or where `/etc/localtime` points, or UTC.
pub fn host_zone(tz: Option<String>, localtime: &Path) -> Tz {
    tz.and_then(|t| t.trim_start_matches(':').parse().ok())
        .or_else(|| {
            let target = std::fs::read_link(localtime).ok()?;
            let s = target.to_string_lossy();
            s.split_once("zoneinfo/")?.1.parse().ok()
        })
        .unwrap_or(chrono_tz::UTC)
}

/// A request a source makes, and its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: &str) -> Request {
        Request {
            method: "GET".into(),
            url: url.into(),
            headers: vec![],
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The credential's username and password, as Basic authentication, where it has both.
    pub fn basic(self, c: Option<&Credential>) -> Request {
        match c.and_then(|c| c.username.as_ref().zip(c.password.as_ref())) {
            Some((u, p)) => {
                let v = format!("Basic {}", STANDARD.encode(format!("{u}:{p}")));
                self.header("Authorization", &v)
            }
            None => self,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// HTTP, behind a trait so that a test answers in a server's place.
#[async_trait::async_trait]
pub trait Http: Send + Sync {
    async fn send(&self, r: Request) -> Result<Response, String>;
}

/// HTTP over the network, every request bounded.
pub struct Network(reqwest::Client);

impl Network {
    pub fn new() -> Network {
        Network(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("yoke-plugin-calendar/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("an HTTP client is built"),
        )
    }
}

impl Default for Network {
    fn default() -> Self {
        Network::new()
    }
}

#[async_trait::async_trait]
impl Http for Network {
    async fn send(&self, r: Request) -> Result<Response, String> {
        let method = reqwest::Method::from_bytes(r.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut b = self.0.request(method, &r.url);
        for (k, v) in &r.headers {
            b = b.header(k, v);
        }
        if let Some(body) = r.body {
            b = b.body(body);
        }
        let resp = b.send().await.map_err(|e| {
            // reqwest names the URL, query included: the cause alone is said.
            let e = e.without_url();
            e.to_string()
        })?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| e.without_url().to_string())?;
        Ok(Response { status, body })
    }
}
