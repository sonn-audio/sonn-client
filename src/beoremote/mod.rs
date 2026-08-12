//! Beoremote One support: menus on the remote, keys and picks back to the server.
//!
//! A Beoremote One paired to a stock Linux box is just a keyboard -- press MUSIC and the display
//! shows three dots forever, because the list has to come from the host and stock BlueZ has no idea
//! how to provide it. B&O's own BlueZ plugin does (they publish the patches under GPLv2 because they
//! must), and it exposes two unix sockets for whoever wants to fill in the menus and take the keys.
//! That "whoever" used to be a Python bridge next to the player; here it is part of the client, which
//! removes the awkward part of the old setup: volume no longer has to travel over D-Bus to another
//! process to reach the player, because the player is in this binary.
//!
//! ```text
//! /var/run/beoremote_one_socket   menus, volume, selections   (plugin listens, we connect)
//! /tmp/streamsdk_hog             raw 2-byte HID key reports   (we listen, hog connects)
//! ```
//!
//! Order matters for the second one. B&O's patch 1016 makes bluetoothd suppress uHID for exactly
//! these 2-byte reports *when the socket exists*; while it is absent, keys arrive as evdev events
//! instead and this bridge never sees them. So the listener is created before anything else.
//!
//! What this module deliberately does *not* do is decide what a key means. Only the server knows
//! what the zone is playing -- a source picked in the app never passes through here -- so keys go up
//! as raw codes and the server maps them. Volume is the single exception: it arrives in bursts of six
//! presses and has to keep working while the server is briefly away, so it is applied locally to the
//! player and reported upstream.

mod api;
mod protocol;

use crate::models::BeoremoteStatusReport;
use crate::status::Registry;
use crate::supervisor::{VolumeIntent, VolumeRequest};
use anyhow::{Context, Result};
use api::{BeoremoteApi, Menu, SelectOutcome};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub const DEFAULT_PLUGIN_SOCKET: &str = "/var/run/beoremote_one_socket";
pub const DEFAULT_HOG_SOCKET: &str = "/tmp/streamsdk_hog";
const DEFAULT_MENU_POLL_MS: u64 = 10_000;
const DEFAULT_VOLUME_STEP: u8 = 4;
/// The plugin needs a moment between attribute writes; B&O's own daemon paces them the same way.
const ATTRIBUTE_PACE: Duration = Duration::from_millis(150);
/// How long to wait before re-dialling the plugin socket. Absent usually means bluetoothd is not
/// running, which is a normal state on a device whose B&O component was never installed.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Consumer HID usages. Volume is the only key this bridge interprets.
const KEY_VOLUME_UP: u8 = 0xE9;
const KEY_VOLUME_DOWN: u8 = 0xEA;

#[derive(Debug, Clone)]
pub struct BeoremoteConfig {
    pub zone_id: u32,
    pub api_base_url: String,
    pub menu_poll: Duration,
    pub volume_player: Option<String>,
    pub volume_step: u8,
    pub plugin_socket: PathBuf,
    pub hog_socket: PathBuf,
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
            plugin_socket: desired
                .plugin_socket
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_PLUGIN_SOCKET)),
            hog_socket: desired
                .hog_socket
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_HOG_SOCKET)),
        })
    }

    /// A change to any of this needs the bridge restarted; a menu poll interval does not.
    pub fn restart_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.zone_id,
            self.api_base_url,
            self.plugin_socket.display(),
            self.hog_socket.display()
        )
    }
}

/// How long bluetoothd gets to attach to the HID socket by itself before the link is assumed to be
/// an older one that is still routed to uHID. It attaches within milliseconds when it is going to.
const HOG_ATTACH_GRACE: Duration = Duration::from_secs(5);

/// Run the bridge until told to stop. Returns only on shutdown; connection loss is retried inside.
pub async fn run(
    config: BeoremoteConfig,
    statuses: Registry,
    volume_tx: mpsc::Sender<VolumeRequest>,
) {
    let hid_connected = Arc::new(AtomicBool::new(false));

    // First, before anything can send a key: with no listener on this socket bluetoothd falls back
    // to uHID for the whole connection, and the fallback is sticky.
    match spawn_hid_listener(
        config.hog_socket.clone(),
        config.clone(),
        statuses.clone(),
        volume_tx.clone(),
        Arc::clone(&hid_connected),
    ) {
        Ok(()) => {}
        Err(err) => {
            warn!("beoremote HID socket unavailable: {:#}", err);
            statuses.set_beoremote(Some(BeoremoteStatusReport {
                state: "error".to_string(),
                zone_id: Some(config.zone_id),
                menu_revision: None,
                hid_connected: false,
                last_error: Some(format!("{:#}", err)),
            }));
        }
    }

    // A remote that was already connected when this started is stuck on uHID (see
    // `drop_remote_link`), so its keys go nowhere. Give bluetoothd a moment to attach to the socket
    // on its own -- which is what happens when the remote connects *after* us -- and only if it has
    // not, drop the link so the next key press rebuilds it the right way round.
    tokio::spawn({
        let hid_connected = Arc::clone(&hid_connected);
        async move {
            tokio::time::sleep(HOG_ATTACH_GRACE).await;
            if hid_connected.load(Ordering::Relaxed) {
                return;
            }
            match crate::pairing::drop_remote_link().await {
                Ok(Some(address)) => info!(
                    "{address} was connected before the bridge was; dropped the link so its keys \
                     come here -- press any key on the remote to bring it back"
                ),
                Ok(None) => debug!("no remote connected yet; its keys will arrive when it connects"),
                Err(err) => warn!("could not drop the stale remote link: {:#}", err),
            }
        }
    });

    loop {
        statuses.set_beoremote(Some(BeoremoteStatusReport {
            state: "waiting".to_string(),
            zone_id: Some(config.zone_id),
            menu_revision: None,
            hid_connected: hid_connected.load(Ordering::Relaxed),
            last_error: None,
        }));

        match serve_plugin(&config, &statuses, &volume_tx, &hid_connected).await {
            Ok(()) => info!("beoremote plugin socket closed"),
            Err(err) => {
                // Not being able to connect is the normal state on a device without the patched
                // bluetoothd, so it is logged at debug and reported as "waiting", not as broken.
                debug!("beoremote bridge: {:#}", err);
                statuses.set_beoremote(Some(BeoremoteStatusReport {
                    state: "waiting".to_string(),
                    zone_id: Some(config.zone_id),
                    menu_revision: None,
                    hid_connected: hid_connected.load(Ordering::Relaxed),
                    last_error: Some(format!("{:#}", err)),
                }));
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// One session on the plugin socket: publish the menu, then serve writes until it closes.
async fn serve_plugin(
    config: &BeoremoteConfig,
    statuses: &Registry,
    volume_tx: &mpsc::Sender<VolumeRequest>,
    hid_connected: &Arc<AtomicBool>,
) -> Result<()> {
    let api = BeoremoteApi::new(&config.api_base_url, config.zone_id)?;
    let mut socket = UnixStream::connect(&config.plugin_socket)
        .await
        .with_context(|| format!("connect {}", config.plugin_socket.display()))?;
    info!(
        "beoremote bridge connected to {}",
        config.plugin_socket.display()
    );

    let mut published = publish(&mut socket, &api).await?;
    statuses.set_beoremote(Some(BeoremoteStatusReport {
        state: "connected".to_string(),
        zone_id: Some(config.zone_id),
        menu_revision: published.revision.clone(),
        hid_connected: hid_connected.load(Ordering::Relaxed),
        last_error: None,
    }));

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut menu_poll = tokio::time::interval(config.menu_poll);
    menu_poll.tick().await; // the first tick is immediate; we just published

    loop {
        tokio::select! {
            read = socket.read(&mut chunk) => {
                let read = read.context("read plugin socket")?;
                if read == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..read]);
                while let Some((attribute, value, consumed)) = take_frame(&buffer) {
                    buffer.drain(..consumed);
                    if handle_write(attribute, &value, &api, &published, config, volume_tx).await {
                        published =
                            republish(&mut socket, &api, statuses, config, hid_connected, None)
                                .await?;
                    }
                }
            }
            _ = menu_poll.tick() => {
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
                    published = republish(&mut socket, &api, statuses, config, hid_connected, Some(menu)).await?;
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

async fn publish(socket: &mut UnixStream, api: &BeoremoteApi) -> Result<Published> {
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
    write_menu(socket, &menu).await
}

async fn republish(
    socket: &mut UnixStream,
    api: &BeoremoteApi,
    statuses: &Registry,
    config: &BeoremoteConfig,
    hid_connected: &Arc<AtomicBool>,
    menu: Option<Menu>,
) -> Result<Published> {
    let published = match menu {
        Some(menu) => write_menu(socket, &menu).await?,
        None => publish(socket, api).await?,
    };
    statuses.set_beoremote(Some(BeoremoteStatusReport {
        state: "connected".to_string(),
        zone_id: Some(config.zone_id),
        menu_revision: published.revision.clone(),
        hid_connected: hid_connected.load(Ordering::Relaxed),
        last_error: None,
    }));
    Ok(published)
}

/// Write the attributes the remote reads on connect, in the order it reads them.
async fn write_menu(socket: &mut UnixStream, menu: &Menu) -> Result<Published> {
    let sources = menu.source_entries();
    let submenu = menu.submenu_entries();

    set(socket, "VERSION", b"1.0").await?;
    set(socket, "FEATURES", &protocol::FEATURES).await?;
    // An empty TV list suppresses the TV menu, which this is not.
    set(socket, "TV_SOURCES", b"").await?;
    set(socket, "MUSIC_SOURCES", &protocol::encode_sources(&sources)).await?;
    set(
        socket,
        "SOURCE_CONTENT_1",
        &protocol::encode_content(&submenu),
    )
    .await?;
    set(socket, "FEATURES_CHANGED", &protocol::FEATURES_CHANGED).await?;

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

async fn set(socket: &mut UnixStream, name: &str, value: &[u8]) -> Result<()> {
    let Some(attribute) = protocol::attribute(name) else {
        return Ok(());
    };
    socket
        .write_all(&protocol::frame(attribute, value))
        .await
        .with_context(|| format!("write {}", name))?;
    tokio::time::sleep(ATTRIBUTE_PACE).await;
    Ok(())
}

/// Split one frame off the front of the buffer: attribute, value, and how many bytes it used.
///
/// The value is copied out rather than borrowed so the caller can drain the buffer in the same loop
/// iteration -- a partial frame at the end has to survive to the next read.
fn take_frame(buffer: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buffer.len() < 3 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([buffer[1], buffer[2]]));
    if buffer.len() < 3 + length {
        return None;
    }
    Some((buffer[0], buffer[3..3 + length].to_vec(), 3 + length))
}

/// Handle an attribute the remote wrote. Returns true when the menu has to be republished -- which
/// happens when the server says the list moved since we rendered it.
async fn handle_write(
    attribute: u8,
    value: &[u8],
    api: &BeoremoteApi,
    published: &Published,
    config: &BeoremoteConfig,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) -> bool {
    let name = protocol::attribute_name(attribute).unwrap_or("UNKNOWN");
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

/// Listen for B&O's HID socket and forward key reports.
///
/// Its own task: the plugin socket may come and go (bluetoothd restarts) while this listener must
/// stay up, because losing it makes bluetoothd fall back to uHID for the next connection.
fn spawn_hid_listener(
    path: PathBuf,
    config: BeoremoteConfig,
    statuses: Registry,
    volume_tx: mpsc::Sender<VolumeRequest>,
    hid_connected: Arc<AtomicBool>,
) -> Result<()> {
    remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    // bluetoothd runs as root; the socket has to be writable by it regardless of who we are.
    set_socket_permissions(&path)?;
    info!("beoremote listening on {} for HID reports", path.display());

    tokio::spawn(async move {
        let api = BeoremoteApi::new(&config.api_base_url, config.zone_id).ok();
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(peer) => peer,
                Err(err) => {
                    warn!("beoremote HID accept failed: {}", err);
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
            info!("bluetoothd connected to the beoremote HID socket");
            hid_connected.store(true, Ordering::Relaxed);
            statuses.set_beoremote_hid(true);

            let mut buffer = [0u8; 64];
            loop {
                let read = match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                // Reports are 2-byte pairs: code then modifier. A release is code 0.
                for pair in buffer[..read].chunks_exact(2) {
                    let (code, _modifier) = (pair[0], pair[1]);
                    if code == 0 {
                        continue;
                    }
                    handle_key(code, &config, api.as_ref(), &volume_tx).await;
                }
            }
            info!("beoremote HID socket peer disconnected");
            hid_connected.store(false, Ordering::Relaxed);
            statuses.set_beoremote_hid(false);
        }
    });
    Ok(())
}

async fn handle_key(
    code: u8,
    config: &BeoremoteConfig,
    api: Option<&BeoremoteApi>,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) {
    if code == KEY_VOLUME_UP || code == KEY_VOLUME_DOWN {
        // Kept local: volume arrives in bursts of six presses, and it should keep working while the
        // server is briefly away. Everything else is the server's decision.
        let step = i16::from(config.volume_step);
        let delta = if code == KEY_VOLUME_UP { step } else { -step };
        let _ = volume_tx
            .send(VolumeRequest {
                client_id: config.volume_player.clone(),
                intent: VolumeIntent::Step(delta),
            })
            .await;
        return;
    }

    let Some(api) = api else { return };
    // Forwarded as a raw code. Which button that is is a property of this remote's hardware, and
    // which action it triggers is a property of the zone -- so the table lives on the server, in one
    // place, instead of in every bridge.
    match api.key(code).await {
        Ok(Some(name)) => info!("beoremote key 0x{:02x} -> {}", code, name),
        Ok(None) => debug!("beoremote key 0x{:02x} is unassigned", code),
        Err(err) => warn!("beoremote key 0x{:02x} failed: {:#}", code, err),
    }
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove stale socket {}", path.display())),
    }
}

fn set_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_taken_one_at_a_time_and_partials_wait() {
        let mut buffer = protocol::frame(22, &[21]);
        buffer.extend_from_slice(&protocol::frame(44, &[40]));
        let (attribute, value, consumed) = take_frame(&buffer).expect("first frame");
        assert_eq!(attribute, 22);
        assert_eq!(value, vec![21]);
        let rest = &buffer[consumed..];
        let (attribute, value, consumed) = take_frame(rest).expect("second frame");
        assert_eq!(attribute, 44);
        assert_eq!(value, vec![40]);
        assert!(take_frame(&rest[consumed..]).is_none());
        // A header without its value yet is not a frame.
        assert!(take_frame(&[22, 0x00, 0x04, 1, 2]).is_none());
    }

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
