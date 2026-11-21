//! Access tokens asked of `online-accounts`, on the channel the Core binds for the need `accounts`.
//!
//! The unit is handed the channel's path and nothing else: the name of the unit behind it is read from the
//! channel's opening, which shows that unit alone. A channel that ends is attached again at the next
//! question, so a restart of `online-accounts` costs one reading.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use yoke_proto::interface::v1::record::Subject;
use yoke_sdk::interface::{Attachment, Options};

/// Tokens, behind a trait so that a test answers in `online-accounts`' place.
#[async_trait]
pub trait Tokens: Send + Sync {
    /// An access token for `account` (`microsoft:<username>`) carrying `scopes`. `Err` says why in words
    /// that carry no token.
    async fn token(&self, account: &str, scopes: &[&str]) -> Result<String, String>;
}

/// The need's channel.
pub struct Channel {
    path: String,
    attached: Mutex<Option<(Attachment, String)>>,
}

impl Channel {
    pub fn new(path: &str) -> Channel {
        Channel {
            path: path.to_string(),
            attached: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Tokens for Channel {
    async fn token(&self, account: &str, scopes: &[&str]) -> Result<String, String> {
        let mut attached = self.attached.lock().await;
        if attached.is_none() {
            let a = Attachment::attach(&self.path, Options::new())
                .await
                .map_err(|e| format!("online-accounts cannot be reached: {e}"))?;
            let unit = a
                .picture
                .records
                .iter()
                .find_map(|r| match &r.subject {
                    Some(Subject::Unit(u)) => u.declared.as_ref().map(|d| d.identity.clone()),
                    _ => None,
                })
                .ok_or("the channel of the need accounts shows no unit")?;
            *attached = Some((a, unit));
        }
        let question = json!({ "account": account, "scopes": scopes }).to_string();
        let asked = {
            let (a, unit) = attached.as_ref().unwrap();
            a.ask(unit, "oauth.token", question.as_bytes()).await
        };
        match asked {
            Ok(answer) => {
                let v: Value = serde_json::from_slice(&answer).map_err(|_| {
                    "online-accounts answered something that is not JSON".to_string()
                })?;
                v["access_token"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "online-accounts answered no access token".into())
            }
            Err(yoke_sdk::base::Error::Refusal(r)) => {
                // The unit's own code is the item of unit.failed: what the person acts on.
                let code = r.item.clone().unwrap_or(r.code.clone());
                Err(format!(
                    "online-accounts refused the token, {code}: {}",
                    r.message
                ))
            }
            Err(e) => {
                // A transport that ended is attached again next time.
                *attached = None;
                Err(format!("online-accounts cannot be reached: {e}"))
            }
        }
    }
}
