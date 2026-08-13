//! Beoremote One support: menus on the remote, keys and picks back to the server.
//!
//! A Beoremote One paired to a stock Linux box is only a keyboard -- press MUSIC and the display
//! shows three dots forever, because the list has to come from the host and nothing on the host
//! offers it. B&O solve that with a patched BlueZ carrying their own plugin; this client solves it
//! by serving the remote's service itself ([`gatt`]) and reading its keys from the kernel's input
//! devices ([`keys`]). No vendor daemon, no GPL artifact to install, and no unix sockets in the
//! path -- and volume no longer travels to another process to reach the player, because the player
//! is in this binary.
//!
//! What this module deliberately does *not* do is decide what a key means. Only the server knows
//! what the zone is playing -- a source picked in the app never passes through here -- so keys go up
//! as raw codes and the server maps them. Volume is the single exception: it arrives in bursts of six
//! presses and has to keep working while the server is briefly away, so it is applied locally to the
//! player and reported upstream.

mod api;
mod gatt;
pub use gatt::unregister_leftovers;
mod keys;
mod protocol;

use crate::models::BeoremoteStatusReport;
use crate::status::Registry;
use crate::supervisor::{VolumeIntent, VolumeRequest};
use anyhow::Result;
use api::{BeoremoteApi, Menu, SelectOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const DEFAULT_MENU_POLL_MS: u64 = 10_000;
const DEFAULT_VOLUME_STEP: u8 = 4;
/// How long to leave a remote alone between calls. See [`refresh_remotes`].
const CALL_COOLDOWN: Duration = Duration::from_secs(60);

/// How long to wait before offering the remote's service to bluez again.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct BeoremoteConfig {
    pub zone_id: u32,
    pub api_base_url: String,
    pub menu_poll: Duration,
    pub volume_player: Option<String>,
    pub volume_step: u8,
}

impl BeoremoteConfig {
    pub fn from_desired(
        desired: &crate::models::DesiredBeoremote,
        fallback_base_url: &str,
    ) -> Option<Self> {
        let zone_id = desired.zone_id?;
        Some(Self {
            zone_id,
            api_base_url: desired
                .api_base_url
                .clone()
                .unwrap_or_else(|| fallback_base_url.to_string()),
            menu_poll: Duration::from_millis(
                desired
                    .menu_poll_ms
                    .unwrap_or(DEFAULT_MENU_POLL_MS)
                    .max(2000),
            ),
            volume_player: desired.volume_player.clone(),
            volume_step: desired.volume_step.unwrap_or(DEFAULT_VOLUME_STEP).max(1),
        })
    }

    /// A change to any of this needs the remote's service restarted; a menu poll interval does not.
    pub fn restart_key(&self) -> String {
        format!("{}|{}", self.zone_id, self.api_base_url)
    }
}

/// The paired remotes, as last read from bluez.
type Remotes = Arc<std::sync::Mutex<Vec<crate::models::PairedRemote>>>;

/// Read the paired list again, and call back anything that has wandered off.
///
/// Quiet on failure: bluez being briefly unavailable is the same state as having no remotes, and
/// the next tick asks again.
///
/// The calling back is not optional politeness. A remote sleeps between key presses and wakes with
/// *undirected* advertisements, which the kernel ignores for a device it already knows -- it only
/// answers directed ones. B&O patch their own BlueZ to make this one device an exception; with a
/// stock daemon the room has to reach out instead, which is the same thing this client does for a
/// phone that has wandered off.
async fn refresh_remotes(
    remotes: &Remotes,
    called: &mut std::collections::HashMap<String, std::time::Instant>,
) {
    let Ok(connection) = zbus::Connection::system().await else {
        return;
    };
    let found = crate::pairing::paired_remotes(&connection).await;
    for remote in &found {
        if remote.connected {
            // It is here; the next disconnect deserves a fresh call rather than the tail of an old
            // cooldown.
            called.remove(&remote.address);
            continue;
        }
        // Once a minute at most.
        //
        // A remote runs on a coin cell and it decides when to sleep -- staying connected is the
        // most expensive thing it can do, and a room that calls back the instant it hangs up is
        // arguing with its power management. The call is cheap for the remote (it only lands while
        // it is already advertising) but the link it opens is not, so it is offered rarely.
        let due = called
            .get(&remote.address)
            .is_none_or(|last| last.elapsed() >= CALL_COOLDOWN);
        if due {
            called.insert(remote.address.clone(), std::time::Instant::now());
            crate::pairing::call_remote(&connection, &remote.address).await;
        }
    }
    if let Ok(mut slot) = remotes.lock() {
        *slot = found;
    }
}

/// A copy for a status report.
fn paired(remotes: &Remotes) -> Vec<crate::models::PairedRemote> {
    remotes.lock().map(|slot| slot.clone()).unwrap_or_default()
}

/// Run the bridge until told to stop. Returns only on shutdown; connection loss is retried inside.
pub async fn run(
    config: BeoremoteConfig,
    statuses: Registry,
    volume_tx: mpsc::Sender<VolumeRequest>,
) {
    let hid_connected = Arc::new(AtomicBool::new(false));
    // Which remotes are paired changes when somebody pairs or forgets one, so it is read on the menu
    // poll rather than on every report -- the answer costs a walk over every bluez object.
    let remotes: Remotes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut called: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();

    // Keys come from the kernel's input devices: on a stock BlueZ the remote is an ordinary HID
    // peripheral, and there is no vendor socket in the path at all.
    tokio::spawn(keys::run(
        config.api_base_url.clone(),
        config.zone_id,
        config.volume_player.clone(),
        config.volume_step,
        volume_tx.clone(),
        statuses.clone(),
    ));

    loop {
        refresh_remotes(&remotes, &mut called).await;
        statuses.set_beoremote(Some(BeoremoteStatusReport {
            state: "waiting".to_string(),
            zone_id: Some(config.zone_id),
            menu_revision: None,
            hid_connected: hid_connected.load(Ordering::Relaxed),
            devices: paired(&remotes),
            last_error: None,
        }));

        match serve_remote(
            &config,
            &statuses,
            &volume_tx,
            &hid_connected,
            &remotes,
            &mut called,
        )
        .await
        {
            Ok(()) => info!("beoremote service unregistered"),
            Err(err) => {
                // Not being able to connect is the normal state on a device without the patched
                // bluetoothd, so it is logged at debug and reported as "waiting", not as broken.
                debug!("beoremote bridge: {:#}", err);
                statuses.set_beoremote(Some(BeoremoteStatusReport {
                    state: "waiting".to_string(),
                    zone_id: Some(config.zone_id),
                    menu_revision: None,
                    hid_connected: hid_connected.load(Ordering::Relaxed),
                    devices: paired(&remotes),
                    last_error: Some(format!("{:#}", err)),
                }));
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// One session: register the service, publish the menu, then serve what the remote writes.
async fn serve_remote(
    config: &BeoremoteConfig,
    statuses: &Registry,
    volume_tx: &mpsc::Sender<VolumeRequest>,
    hid_connected: &Arc<AtomicBool>,
    remotes: &Remotes,
    called: &mut std::collections::HashMap<String, std::time::Instant>,
) -> Result<()> {
    let api = BeoremoteApi::new(&config.api_base_url, config.zone_id)?;
    // Room for a burst of writes: the remote sends a selection and its follow-ups back to back, and
    // dropping one of those is a menu pick that silently does nothing.
    let (writes_tx, mut writes) = mpsc::channel::<gatt::Write>(32);
    let service = gatt::BeoremoteGatt::register(writes_tx).await?;
    info!("beoremote service registered with bluez");

    let mut published = publish(&service, &api).await?;
    statuses.set_beoremote(Some(BeoremoteStatusReport {
        state: "connected".to_string(),
        zone_id: Some(config.zone_id),
        menu_revision: published.revision.clone(),
        hid_connected: hid_connected.load(Ordering::Relaxed),
        devices: paired(remotes),
        last_error: None,
    }));

    let mut menu_poll = tokio::time::interval(config.menu_poll);
    menu_poll.tick().await; // the first tick is immediate; we just published

    loop {
        tokio::select! {
            write = writes.recv() => {
                let Some((name, value)) = write else {
                    service.unregister().await;
                    return Ok(());
                };
                if handle_write(&name, &value, &api, &published, config, volume_tx).await {
                    published =
                        republish(&service, &api, statuses, config, hid_connected, remotes, None)
                            .await?;
                }
            }
            // bluetoothd going away takes the registration with it; the outer loop puts it back.
            _ = service.wait_until_bluez_goes_away() => {
                warn!("bluez went away; re-registering the beoremote service");
                return Ok(());
            }
            _ = menu_poll.tick() => {
                refresh_remotes(remotes, called).await;
                // Re-read every tick and only republish on a real change: the remote is not
                // disturbed for nothing, and a new favourite still shows up within one interval.
                let menu = match api.menu().await {
                    Ok(menu) => menu,
                    Err(err) => {
                        debug!("beoremote menu poll failed: {:#}", err);
                        continue;
                    }
                };
                if menu_changed(&published, &menu) {
                    info!("beoremote menu changed; republishing");
                    published =
                        republish(&service, &api, statuses, config, hid_connected, remotes, Some(menu))
                            .await?;
                }
            }
        }
    }
}

/// What the remote is currently looking at. Kept because it reports positions and nothing else.
///
/// There is deliberately no "active source" here: the server owns that, and a pick made in the app
/// never reaches this process. Tracking it locally is what used to make transport keys keep going to
/// the MasterLink bus after the app had already switched the zone to something else.
#[derive(Debug, Clone, Default)]
struct Published {
    revision: Option<String>,
    sources: Vec<(String, bool)>,
    submenu: Vec<String>,
}

fn menu_changed(published: &Published, menu: &Menu) -> bool {
    published.revision != menu.revision
        || published.sources != menu.source_entries()
        || published.submenu != menu.submenu_entries()
}

async fn publish(service: &gatt::BeoremoteGatt, api: &BeoremoteApi) -> Result<Published> {
    let menu = match api.menu().await {
        Ok(menu) => menu,
        Err(err) => {
            // An empty menu is a better failure than none: the remote renders "no sources" instead
            // of hanging on three dots, and the next poll fills it in.
            warn!(
                "beoremote menu unavailable ({:#}); publishing an empty menu",
                err
            );
            Menu {
                revision: None,
                sources: Vec::new(),
                submenu: Vec::new(),
            }
        }
    };
    write_menu(service, &menu).await
}

async fn republish(
    service: &gatt::BeoremoteGatt,
    api: &BeoremoteApi,
    statuses: &Registry,
    config: &BeoremoteConfig,
    hid_connected: &Arc<AtomicBool>,
    remotes: &Remotes,
    menu: Option<Menu>,
) -> Result<Published> {
    let published = match menu {
        Some(menu) => write_menu(service, &menu).await?,
        None => publish(service, api).await?,
    };
    statuses.set_beoremote(Some(BeoremoteStatusReport {
        state: "connected".to_string(),
        zone_id: Some(config.zone_id),
        menu_revision: published.revision.clone(),
        hid_connected: hid_connected.load(Ordering::Relaxed),
        devices: paired(remotes),
        last_error: None,
    }));
    Ok(published)
}

/// Write the attributes the remote reads on connect, in the order it reads them.
async fn write_menu(service: &gatt::BeoremoteGatt, menu: &Menu) -> Result<Published> {
    let sources = menu.source_entries();
    let submenu = menu.submenu_entries();

    service.set("VERSION", b"1.0");
    service.set("FEATURES", &protocol::FEATURES);
    // An empty TV list suppresses the TV menu, which this is not.
    service.set("TV_SOURCES", b"");
    service.set("MUSIC_SOURCES", &protocol::encode_sources(&sources));
    service.set("SOURCE_CONTENT_1", &protocol::encode_content(&submenu));
    // Last, and announced: this is the one the remote subscribes to, and it is what tells a remote
    // that is already looking at the menu to read the lists again.
    service.set("FEATURES_CHANGED", &protocol::FEATURES_CHANGED);
    service.announce("FEATURES_CHANGED").await?;

    info!(
        "beoremote menu published: revision {:?}, {} sources, {} submenu items",
        menu.revision,
        sources.len(),
        submenu.len()
    );
    Ok(Published {
        revision: menu.revision.clone(),
        sources,
        submenu,
    })
}

/// Handle an attribute the remote wrote. Returns true when the menu has to be republished -- which
/// happens when the server says the list moved since we rendered it.
async fn handle_write(
    name: &str,
    value: &[u8],
    api: &BeoremoteApi,
    published: &Published,
    config: &BeoremoteConfig,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) -> bool {
    match (name, value.len()) {
        ("ACTIVE_SOURCE", 1) => {
            let raw = value[0];
            let index = usize::from(raw.saturating_sub(protocol::ACTIVE_SOURCE_BASE));
            let label = published
                .sources
                .get(index)
                .map(|(name, _)| name.as_str())
                .unwrap_or("?");
            info!("beoremote picked source {} ({})", index, label);
            match api
                .select("source", raw, published.revision.as_deref())
                .await
            {
                SelectOutcome::Started { name } => {
                    debug!("server started {:?}", name);
                    false
                }
                SelectOutcome::Refresh => true,
                SelectOutcome::NotSelectable => {
                    debug!("header row picked; nothing to play");
                    false
                }
                SelectOutcome::Failed { message } => {
                    warn!("beoremote selection failed: {}", message);
                    false
                }
            }
        }
        ("ACTIVE_SOURCE_CONTENT", 1) => {
            let index = value[0];
            let label = published
                .submenu
                .get(usize::from(index))
                .map(String::as_str)
                .unwrap_or("?");
            info!("beoremote picked submenu item {} ({})", index, label);
            match api
                .select("submenu", index, published.revision.as_deref())
                .await
            {
                SelectOutcome::Refresh => true,
                SelectOutcome::Failed { message } => {
                    warn!("beoremote submenu selection failed: {}", message);
                    false
                }
                _ => false,
            }
        }
        ("VOLUME", 1) => {
            // The remote's own absolute volume. Rare -- the keys are relative -- but when it comes
            // it is authoritative, so it is passed straight through.
            let level = value[0].min(100);
            debug!("beoremote wrote absolute volume {}", level);
            let _ = volume_tx
                .send(VolumeRequest {
                    client_id: config.volume_player.clone(),
                    intent: VolumeIntent::Set(level),
                })
                .await;
            false
        }
        ("INJECT_PRESS", _) | ("INJECT_RELEASE", _) => {
            debug!("beoremote {}: {:?}", name, String::from_utf8_lossy(value));
            false
        }
        _ => {
            debug!("beoremote wrote {} = {:?}", name, value);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_menu_is_republished_only_when_it_really_changed() {
        let published = Published {
            revision: Some("abc".to_string()),
            sources: vec![("Radio".to_string(), true)],
            submenu: vec!["NPO 2".to_string()],
        };
        let same = Menu {
            revision: Some("abc".to_string()),
            sources: vec![api::MenuEntry {
                name: Some("Radio".to_string()),
                submenu: Some(true),
            }],
            submenu: vec![api::MenuEntry {
                name: Some("NPO 2".to_string()),
                submenu: None,
            }],
        };
        assert!(!menu_changed(&published, &same));
        let renamed = Menu {
            revision: Some("def".to_string()),
            ..same
        };
        assert!(menu_changed(&published, &renamed));
    }
}
