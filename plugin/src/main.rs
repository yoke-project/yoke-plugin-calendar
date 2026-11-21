//! calendar: the events of the sources its arguments name, as a Yoke Plugin unit.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use calendar::accounts::{Channel, Tokens};
use calendar::plugin::{carry, declaration, handle, Calendar, Happened, Parts, PLUGIN};
use calendar::read::Cache;
use calendar::sources::{self, host_zone, Network, Settings};
use yoke_sdk::plugin::{start, Event};

/// How often the unit looks at what is on and what is about to start.
const LOOK_EVERY: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() {
    // `calendar --manifest` prints the Manifest the declaration generates, which the bundle carries.
    if std::env::args().nth(1).as_deref() == Some("--manifest") {
        print!("{}", declaration().manifest());
        return;
    }
    let zone = host_zone(std::env::var("TZ").ok(), Path::new("/etc/localtime"));
    let settings = match Settings::parse(std::env::args().skip(1), zone) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("calendar: {e}");
            std::process::exit(2);
        }
    };
    // The credentials are the deployment's: ones it cannot read leave every source read without any, and
    // the unit says so.
    let (credentials, credentials_said) = match std::fs::read_to_string(&settings.credentials)
        .map_err(|e| format!("the secret credentials cannot be read: {e}"))
        .and_then(|text| sources::credentials(&text))
    {
        Ok(c) => (c, None),
        Err(why) => (Default::default(), Some(why)),
    };
    let unit = match start(&declaration()).await {
        Ok(unit) => Arc::new(unit),
        Err(err) => {
            eprintln!("calendar: {err}");
            std::process::exit(1);
        }
    };
    let (happened_tx, mut happened) = tokio::sync::mpsc::unbounded_channel();
    let parts = Parts {
        http: Arc::new(Network::new()),
        sources: settings.sources.clone(),
        credentials,
        tokens: settings
            .accounts
            .as_deref()
            .map(|path| Arc::new(Channel::new(path)) as Arc<dyn Tokens>),
        cache: Cache(settings.cache.clone()),
        zone: settings.zone,
        remind_before: settings.remind_before,
        upcoming: settings.upcoming,
        horizon: settings.horizon,
    };
    let calendar = Calendar::new(parts, happened_tx);
    if let Some(why) = &credentials_said {
        if let Err(err) = unit.health(50, why).await {
            eprintln!("{PLUGIN}: {err}");
        }
    }
    // The sources read at once and again at every interval; what is on looked at in between.
    let reading = calendar.clone();
    let every = settings.refresh;
    tokio::spawn(async move {
        loop {
            let _ = reading.refresh(None).await;
            tokio::time::sleep(every).await;
        }
    });
    let looking = calendar.clone();
    tokio::spawn(async move {
        loop {
            looking.changed(chrono::Utc::now());
            tokio::time::sleep(LOOK_EVERY).await;
        }
    });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("calendar listens for SIGTERM");
    loop {
        tokio::select! {
            event = unit.next() => match event {
                None | Some(Event::Ended(_)) => return,
                Some(Event::Activated(a)) => {
                    if let Err(err) = calendar.activated(unit.as_ref(), &a.stream).await {
                        eprintln!("calendar: {err}");
                    }
                }
                Some(Event::Stopped { stream }) => calendar.stopped(&stream),
                Some(event) => {
                    let (unit, calendar) = (unit.clone(), calendar.clone());
                    tokio::spawn(async move {
                        if let Err(err) = handle(unit.as_ref(), &event, &calendar).await {
                            eprintln!("calendar: {err}");
                        }
                    });
                }
            },
            Some(h) = happened.recv() => {
                // A health the credentials' failure set stays: no reading makes it good.
                if credentials_said.is_some() && matches!(h, Happened::Health { .. }) {
                    continue;
                }
                if let Err(err) = carry(unit.as_ref(), &calendar, h).await {
                    eprintln!("calendar: {err}");
                }
            },
            _ = term.recv() => {
                let _ = unit.close().await;
                while let Some(event) = unit.next().await {
                    if matches!(event, Event::Ended(_)) {
                        break;
                    }
                }
                return;
            }
        }
    }
}
