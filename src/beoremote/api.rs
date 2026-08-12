//! The server side of the remote: menu in, picks and keys out.
//!
//! The server owns the menu. It knows the zone, the favourites, the stations and which of them can
//! be started; this end renders whatever it is given and reports positions back. A new playlist
//! appears on the remote with nothing deployed here.
//!
//! Picks carry the revision the menu was rendered from. The remote reports *a position*, so without
//! that check a list which changed since publishing would start the wrong thing -- and it changes
//! whenever a favourite is added.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::debug;

#[derive(Debug, Clone, Deserialize)]
pub struct MenuEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub submenu: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Menu {
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub sources: Vec<MenuEntry>,
    #[serde(default)]
    pub submenu: Vec<MenuEntry>,
}

impl Menu {
    /// `(name, has_submenu)` pairs for `MUSIC_SOURCES`, skipping entries with no name.
    pub fn source_entries(&self) -> Vec<(String, bool)> {
        self.sources
            .iter()
            .filter_map(|entry| {
                entry
                    .name
                    .as_ref()
                    .map(|name| (name.clone(), entry.submenu.unwrap_or(false)))
            })
            .collect()
    }

    pub fn submenu_entries(&self) -> Vec<String> {
        self.submenu
            .iter()
            .filter_map(|entry| entry.name.clone())
            .collect()
    }
}

/// What the server did with a pick.
#[derive(Debug, Clone)]
pub enum SelectOutcome {
    /// Started; the name is for the log.
    Started { name: Option<String> },
    /// The menu moved under us. Re-read and republish before doing anything else.
    Refresh,
    /// A header row or an entry that cannot be played. Not an error.
    NotSelectable,
    /// The call failed.
    Failed { message: String },
}

#[derive(Debug, Clone, Deserialize)]
struct SelectResponse {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct KeyResponse {
    #[serde(default)]
    name: Option<String>,
    /// What the server did with it, for the keys that start nothing named.
    #[serde(default)]
    action: Option<String>,
}

#[derive(Clone)]
pub struct BeoremoteApi {
    base_url: String,
    zone_id: u32,
    client: Client,
}

impl BeoremoteApi {
    pub fn new(base_url: &str, zone_id: u32) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            zone_id,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build http client")?,
        })
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "{}/api/beoremote/zones/{}/{}",
            self.base_url, self.zone_id, suffix
        )
    }

    pub async fn menu(&self) -> Result<Menu> {
        let response = self
            .client
            .get(self.url("menu"))
            .send()
            .await
            .context("get menu")?
            .error_for_status()
            .context("menu response status")?;
        response.json::<Menu>().await.context("parse menu")
    }

    /// Report a pick. `active_source` is the raw value the remote wrote (20 + position) for the
    /// source list, or the plain index for a submenu -- the server resolves both.
    pub async fn select(
        &self,
        list: &str,
        active_source: u8,
        revision: Option<&str>,
    ) -> SelectOutcome {
        let body = serde_json::json!({
            "list": list,
            "active_source": active_source,
            "revision": revision,
        });
        let response = match self
            .client
            .post(self.url("select"))
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                return SelectOutcome::Failed {
                    message: err.to_string(),
                }
            }
        };
        let status = response.status();
        let parsed = response.json::<SelectResponse>().await.ok();

        if status.is_success() {
            return SelectOutcome::Started {
                name: parsed.and_then(|body| body.name),
            };
        }
        if status.as_u16() == 409 {
            // Stale revision: the list moved since we rendered it. The body carries the current
            // revision, but re-reading the menu brings a fresh one anyway -- and we need the entries
            // regardless, so there is nothing to save by reading it here.
            return SelectOutcome::Refresh;
        }
        let error = parsed.and_then(|body| body.error).unwrap_or_default();
        if status.as_u16() == 400 {
            if error.contains("not-selectable") {
                return SelectOutcome::NotSelectable;
            }
            return SelectOutcome::Refresh;
        }
        SelectOutcome::Failed {
            message: format!("{} {}", status.as_u16(), error),
        }
    }

    /// Report a key press by its raw code.
    ///
    /// No revision: a key is not a list position, so nothing can have shifted underneath it. What a
    /// key does is entirely the server's business -- it is the only party that knows whether this
    /// zone is on a line-in (where `next` is a Beo4 command) or a network source (where it advances
    /// the queue).
    pub async fn key(&self, code: u16) -> Result<Option<String>> {
        // The kernel's key code, as a number. No hex string and no translation: the server decides
        // what a button means, and it can only do that if it is told what the kernel actually saw.
        let body = serde_json::json!({ "code": code });
        let response = self
            .client
            .post(self.url("key"))
            .json(&body)
            .send()
            .await
            .context("post key")?;
        if response.status().as_u16() == 404 {
            debug!("key {code} is not assigned on the server");
            return Ok(None);
        }
        // 409 is the server saying the button is bound but there is nothing behind it -- an empty
        // favorite slot, say. That is an answer, not a failure, and logging it as one sends someone
        // looking for a fault in the remote.
        if response.status().as_u16() == 409 {
            debug!("key {code} is bound to something empty");
            return Ok(Some("nothing to play".to_string()));
        }
        let response = response.error_for_status().context("key response status")?;
        // `name` is only there for a key that starts something named -- a favorite, a station. A
        // transport key answers with its action and no name, and reading that as "nothing happened"
        // is what made working keys look dead in the log.
        let parsed = response.json::<KeyResponse>().await.ok();
        Ok(parsed.and_then(|body| body.name.or(body.action)))
    }
}
