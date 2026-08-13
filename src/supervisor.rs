//! Keeps what is running equal to what the server asked for.
//!
//! The status loop drops the server's desired state into a watch channel; this reconciles against it.
//! A change a live player can absorb -- volume, mute, static delay -- is handed to it. A change it
//! cannot -- another sound card, another rate, another server -- restarts just that one. Adding a
//! second room to a device with two DACs starts one task and leaves the other playing.
//!
//! Players and sources each get their **own OS thread** with a current-thread runtime, rather than a
//! task on the shared pool. cpal's stream handle is not guaranteed to be `Send`, so an audio path
//! cannot be moved between worker threads; giving each one a thread makes that a non-question and
//! keeps a device's audio off the same scheduler as its HTTP polling. Everything that crosses between
//! them is a channel.

use crate::beoremote::{self, BeoremoteConfig};
use crate::bluetooth::{self, BluetoothConfig};
use crate::components;
use crate::models::{DesiredConfig, DesiredPlayer, DesiredSource};
use crate::player::{self, LiveSettings};
use crate::source::{self, SignalSettings};
use crate::status::Registry;
use crate::update;
use std::collections::HashMap;
use std::future::Future;
use std::thread::JoinHandle;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Depth of the volume queue. A burst of remote presses is six or so; anything past this is a device
/// that is not keeping up, and dropping the surplus beats queueing a slider that has moved on.
const VOLUME_QUEUE: usize = 32;

/// What someone on this device wants done to a player's volume.
///
/// The remote is the only such someone today. It goes through a channel rather than reaching into the
/// player because the player lives on its own thread, and because the *current* level is not the
/// remote's to know: the server can have changed it a moment ago.
#[derive(Debug, Clone, Copy)]
pub enum VolumeIntent {
    /// Relative, in points on the 0-100 scale.
    Step(i16),
    /// Absolute.
    Set(u8),
}

#[derive(Debug, Clone)]
pub struct VolumeRequest {
    /// Which player. `None` means the device's first, which is the only one on a single-output box.
    pub client_id: Option<String>,
    pub intent: VolumeIntent,
}

struct RunningPlayer {
    /// Everything that would need a reconnect to change; a mismatch restarts the thread.
    key: String,
    /// The last state the *server* asked for, so a poll that repeats it does not overwrite a level
    /// the user has since changed from the app or the remote.
    last_desired: LiveSettings,
    /// The last state we actually pushed, which includes local volume changes.
    last_pushed: LiveSettings,
    settings_tx: watch::Sender<LiveSettings>,
    stop_tx: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

struct RunningSource {
    key: String,
    last_desired: SignalSettings,
    signal_tx: watch::Sender<SignalSettings>,
    stop_tx: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

struct RunningBeoremote {
    key: String,
    handle: tokio::task::JoinHandle<()>,
}

/// Everything the supervisor needs that is not the desired state itself.
pub struct SupervisorContext {
    pub statuses: Registry,
    /// Hardware volume hook from config.toml, for players the server gives none.
    pub fallback_volume_hook: Option<String>,
    /// Base URL of the server we registered with, for the parts of the beoremote bridge that talk
    /// HTTP rather than Sendspin.
    pub server_base_url: String,
    /// This device's own id, which the Bluetooth input's client id is derived from when the server
    /// names none.
    pub device_id: String,
    /// Where an operator's Bluetooth commands are handed in. Filled when the radio starts for a
    /// zone and cleared when it stops, so a command sent while it is off says so instead of
    /// disappearing.
    pub bluetooth_commands: bluetooth::CommandBus,
}

pub async fn run(
    mut desired_rx: watch::Receiver<DesiredConfig>,
    ctx: SupervisorContext,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut players: HashMap<String, RunningPlayer> = HashMap::new();
    let mut sources: HashMap<String, RunningSource> = HashMap::new();
    let mut beoremote: Option<RunningBeoremote> = None;
    let mut bluetooth: Option<RunningBluetooth> = None;
    let mut adapter_name: Option<String> = None;
    let (volume_tx, mut volume_rx) = mpsc::channel::<VolumeRequest>(VOLUME_QUEUE);
    // Components are reconciled on change only: it writes files and restarts services, which is not
    // something to redo every five seconds.
    let mut component_key = String::new();

    loop {
        if *stop_rx.borrow() {
            break;
        }
        let desired = desired_rx.borrow_and_update().clone();
        reconcile_components(&desired, &ctx, &mut component_key).await;
        reconcile_players(&desired, &mut players, &ctx).await;
        reconcile_sources(&desired, &mut sources, &ctx).await;
        reconcile_adapter_name(&desired, &mut adapter_name);
        reconcile_beoremote(&desired, &mut beoremote, &ctx, &volume_tx);
        reconcile_bluetooth(&desired, &mut bluetooth, &ctx, &volume_tx);

        tokio::select! {
            changed = desired_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    break;
                }
            }
            request = volume_rx.recv() => {
                match request {
                    Some(request) => apply_volume(request, &mut players, &ctx),
                    None => break,
                }
            }
        }
    }

    stop_beoremote(&mut beoremote);
    stop_bluetooth(&mut bluetooth);
    stop_all_players(&mut players).await;
    stop_all_sources(&mut sources).await;
    ctx.statuses.retain(&[]);
    ctx.statuses.retain_sources(&[]);
}

// ---------------------------------------------------------------------------- players

async fn reconcile_players(
    desired: &DesiredConfig,
    running: &mut HashMap<String, RunningPlayer>,
    ctx: &SupervisorContext,
) {
    let Some(url) = sendspin_url(desired) else {
        if !running.is_empty() {
            info!("no sendspin endpoint configured; stopping all players");
            stop_all_players(running).await;
            ctx.statuses.retain(&[]);
        }
        return;
    };

    let wanted: Vec<&DesiredPlayer> = desired
        .players
        .iter()
        .filter(|player| player.is_enabled() && !player.client_id.trim().is_empty())
        .collect();
    let wanted_ids: Vec<String> = wanted
        .iter()
        .map(|player| player.client_id.clone())
        .collect();

    // Stop what is gone or has to be rebuilt. Waiting for the thread matters: the replacement opens
    // the same sound card, and ALSA hands out "device or resource busy" to whoever asks first.
    let mut stale: Vec<String> = Vec::new();
    for (client_id, entry) in running.iter() {
        let keep = wanted
            .iter()
            .find(|player| player.client_id == *client_id)
            .is_some_and(|player| player.restart_key(url) == entry.key);
        if !keep {
            stale.push(client_id.clone());
        }
    }
    for client_id in stale {
        if let Some(entry) = running.remove(&client_id) {
            info!(client_id = %client_id, "stopping player");
            stop_player(entry).await;
        }
    }

    for player in wanted {
        let settings = LiveSettings::from_desired(player);

        if let Some(entry) = running.get_mut(&player.client_id) {
            if entry.last_desired != settings {
                entry.last_desired = settings.clone();
                entry.last_pushed = settings.clone();
                let _ = entry.settings_tx.send(settings);
            }
            continue;
        }

        let name = player
            .name
            .clone()
            .or_else(|| desired.device_name.clone())
            .unwrap_or_else(|| player.client_id.clone());
        let status = ctx.statuses.handle(
            &player.client_id,
            player.output.clone(),
            settings.volume,
            settings.muted,
            settings.static_delay_ms,
        );
        // Built here rather than on the audio thread: opening the card and reading its mixer is
        // where this fails, and a failure has to reach the server as a fault on this speaker
        // instead of disappearing into a thread that quietly retries.
        let built = player::build(
            player,
            name.clone(),
            ctx.fallback_volume_hook.as_deref(),
            &settings,
        );
        let session = match built {
            Ok(session) => session,
            Err(err) => {
                warn!(client_id = %player.client_id, "cannot start player: {:#}", err);
                status.set_error(format!("{:#}", err));
                continue;
            }
        };

        let (settings_tx, settings_rx) = watch::channel(settings.clone());
        let (stop_tx, stop_rx) = watch::channel(false);

        info!(
            client_id = %player.client_id,
            name = %name,
            output = player.output.as_deref().unwrap_or("(default)"),
            "starting player"
        );
        let key = player.restart_key(url);
        let thread_name = format!("player-{}", short_name(&player.client_id));
        let url = url.to_string();
        // Its own OS thread with a current-thread runtime: cpal's stream is not guaranteed to be
        // Send, so the session that owns it cannot be moved between worker threads.
        let thread = spawn_audio_thread(thread_name, move || async move {
            player::run(session, url, status, settings_rx, stop_rx).await;
        });

        running.insert(
            player.client_id.clone(),
            RunningPlayer {
                key,
                last_desired: settings.clone(),
                last_pushed: settings,
                settings_tx,
                stop_tx,
                thread,
            },
        );
    }

    ctx.statuses.retain(&wanted_ids);
}

/// Apply a local volume change (the remote) to a player.
///
/// The level it moves from is the one the *player* reports, not the one the server last configured:
/// the server can have changed it since, and stepping from a stale value makes the first press jump.
fn apply_volume(
    request: VolumeRequest,
    running: &mut HashMap<String, RunningPlayer>,
    ctx: &SupervisorContext,
) {
    let client_id = match request.client_id {
        Some(client_id) if running.contains_key(&client_id) => client_id,
        Some(client_id) => {
            debug!("volume request for unknown player {}", client_id);
            return;
        }
        None => match ctx.statuses.player_ids().into_iter().next() {
            Some(first) => first,
            None => return,
        },
    };
    let Some(entry) = running.get_mut(&client_id) else {
        return;
    };

    let (current_volume, current_muted) = ctx
        .statuses
        .player_volume(&client_id)
        .unwrap_or((entry.last_pushed.volume, entry.last_pushed.muted));
    let mut next = LiveSettings {
        volume: current_volume,
        muted: current_muted,
        static_delay_ms: entry.last_pushed.static_delay_ms,
    };

    match request.intent {
        VolumeIntent::Step(delta) => {
            let target = i32::from(current_volume) + i32::from(delta);
            next.volume = target.clamp(0, 100) as u8;
            // A press on a muted speaker means "let me hear it", not "adjust silence".
            next.muted = false;
        }
        VolumeIntent::Set(level) => {
            next.volume = level.min(100);
            next.muted = false;
        }
    }

    if next == entry.last_pushed {
        return;
    }
    debug!(
        client_id = %client_id,
        volume = next.volume,
        muted = next.muted,
        "applying local volume change"
    );
    entry.last_pushed = next.clone();
    let _ = entry.settings_tx.send(next);
}

async fn stop_player(entry: RunningPlayer) {
    let _ = entry.stop_tx.send(true);
    join_thread(entry.thread).await;
}

async fn stop_all_players(running: &mut HashMap<String, RunningPlayer>) {
    for (_, entry) in running.drain().collect::<Vec<_>>() {
        stop_player(entry).await;
    }
}

// ---------------------------------------------------------------------------- sources

async fn reconcile_sources(
    desired: &DesiredConfig,
    running: &mut HashMap<String, RunningSource>,
    ctx: &SupervisorContext,
) {
    let Some(url) = sendspin_url(desired) else {
        if !running.is_empty() {
            stop_all_sources(running).await;
            ctx.statuses.retain_sources(&[]);
        }
        return;
    };

    let wanted: Vec<&DesiredSource> = desired
        .sources
        .iter()
        .filter(|entry| entry.is_enabled() && !entry.client_id.trim().is_empty())
        .collect();
    let wanted_ids: Vec<String> = wanted.iter().map(|entry| entry.client_id.clone()).collect();

    let mut stale: Vec<String> = Vec::new();
    for (client_id, entry) in running.iter() {
        let keep = wanted
            .iter()
            .find(|source| source.client_id == *client_id)
            .is_some_and(|source| source.restart_key(url) == entry.key);
        if !keep {
            stale.push(client_id.clone());
        }
    }
    for client_id in stale {
        if let Some(entry) = running.remove(&client_id) {
            info!(client_id = %client_id, "stopping source");
            stop_source(entry).await;
        }
    }

    for desired_source in wanted {
        let signal = SignalSettings::from_desired(desired_source);

        if let Some(entry) = running.get_mut(&desired_source.client_id) {
            if entry.last_desired != signal {
                entry.last_desired = signal;
                let _ = entry.signal_tx.send(signal);
            }
            continue;
        }

        let name = desired_source
            .name
            .clone()
            .unwrap_or_else(|| desired_source.client_id.clone());
        let status = ctx
            .statuses
            .source_handle(&desired_source.client_id, desired_source.input.clone());
        // Transport controls used to ride on the source role. The protocol has no such message --
        // that surface was this client's invention -- so a configured hook would now be a setting
        // that silently does nothing, which is worse than one that is missing.

        let built = source::build(desired_source, name);
        let session = match built {
            Ok(session) => session,
            Err(err) => {
                warn!(client_id = %desired_source.client_id, "cannot start source: {:#}", err);
                status.set_error(format!("{:#}", err));
                continue;
            }
        };

        let (signal_tx, signal_rx) = watch::channel(signal);
        let (stop_tx, stop_rx) = watch::channel(false);

        info!(
            client_id = %desired_source.client_id,
            input = desired_source.input.as_deref().unwrap_or("(default)"),
            "starting source"
        );
        let key = desired_source.restart_key(url);
        let thread_name = format!("source-{}", short_name(&desired_source.client_id));
        let url = url.to_string();
        let thread = spawn_audio_thread(thread_name, move || async move {
            source::run(session, url, status, signal_rx, stop_rx).await;
        });

        running.insert(
            desired_source.client_id.clone(),
            RunningSource {
                key,
                last_desired: signal,
                signal_tx,
                stop_tx,
                thread,
            },
        );
    }

    ctx.statuses.retain_sources(&wanted_ids);
}

async fn stop_source(entry: RunningSource) {
    let _ = entry.stop_tx.send(true);
    join_thread(entry.thread).await;
}

async fn stop_all_sources(running: &mut HashMap<String, RunningSource>) {
    for (_, entry) in running.drain().collect::<Vec<_>>() {
        stop_source(entry).await;
    }
}

// ---------------------------------------------------------------------------- adapter name

/// Give the radio the room's name, once, wherever it is used.
///
/// The adapter carries a single name, and everyone who reads it -- a phone looking for a speaker, a
/// Beoremote One listing its products -- should see the room. Set when it changes and not before:
/// renaming an adapter a remote is paired to makes the remote show a different product, so it is
/// worth doing exactly as often as the room is actually renamed.
fn reconcile_adapter_name(desired: &DesiredConfig, current: &mut Option<String>) {
    let Some(name) = desired.adapter_name() else {
        return;
    };
    if current.as_deref() == Some(name.as_str()) {
        return;
    }
    *current = Some(name.clone());
    tokio::spawn(async move {
        match bluetooth::set_adapter_name(&name).await {
            Ok(()) => info!("the bluetooth adapter is now called {name}"),
            Err(err) => warn!("could not name the bluetooth adapter: {err:#}"),
        }
    });
}

// ---------------------------------------------------------------------------- bluetooth

/// The radio, set up for whichever zone claimed it.
struct RunningBluetooth {
    key: String,
    handle: tokio::task::JoinHandle<()>,
}

fn reconcile_bluetooth(
    desired: &DesiredConfig,
    running: &mut Option<RunningBluetooth>,
    ctx: &SupervisorContext,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) {
    let wanted = desired.bluetooth.as_ref().and_then(|entry| {
        BluetoothConfig::from_desired(entry, sendspin_url(desired), &ctx.device_id)
    });

    match (&wanted, running.as_ref()) {
        (Some(config), Some(entry)) if entry.key == config.restart_key() => return,
        (None, None) => return,
        _ => {}
    }

    stop_bluetooth(running);
    ctx.statuses.set_bluetooth(None);
    ctx.bluetooth_commands.set(None);

    let Some(config) = wanted else {
        return;
    };
    info!(
        zone_id = config.zone_id,
        name = %config.name,
        "starting bluetooth audio"
    );
    let key = config.restart_key();
    let statuses = ctx.statuses.clone();
    // Room for a burst: an operator can press pair twice, and a phone can be forgotten while the
    // window is open, without either being dropped.
    let (commands, receiver) = mpsc::channel(8);
    let volume_tx = volume_tx.clone();
    let handle = tokio::spawn(async move {
        bluetooth::run(config, statuses, receiver, volume_tx).await;
    });
    ctx.bluetooth_commands.set(Some(commands));
    *running = Some(RunningBluetooth { key, handle });
}

fn stop_bluetooth(running: &mut Option<RunningBluetooth>) {
    if let Some(entry) = running.take() {
        entry.handle.abort();
    }
}

// ---------------------------------------------------------------------------- beoremote

fn reconcile_beoremote(
    desired: &DesiredConfig,
    running: &mut Option<RunningBeoremote>,
    ctx: &SupervisorContext,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) {
    let wanted = desired
        .beoremote
        .as_ref()
        .filter(|entry| entry.is_enabled())
        .and_then(|entry| BeoremoteConfig::from_desired(entry, &ctx.server_base_url));

    match (&wanted, running.as_ref()) {
        (Some(config), Some(entry)) if entry.key == config.restart_key() => return,
        (None, None) => return,
        _ => {}
    }

    stop_beoremote(running);
    ctx.statuses.set_beoremote(None);

    let Some(config) = wanted else {
        return;
    };
    info!(zone_id = config.zone_id, "starting beoremote bridge");
    let key = config.restart_key();
    let statuses = ctx.statuses.clone();
    let volume_tx = volume_tx.clone();
    // A normal task: no cpal here, only two unix sockets and an HTTP client.
    let handle = tokio::spawn(async move {
        beoremote::run(config, statuses, volume_tx).await;
    });
    *running = Some(RunningBeoremote { key, handle });
}

fn stop_beoremote(running: &mut Option<RunningBeoremote>) {
    if let Some(entry) = running.take() {
        entry.handle.abort();
    }
}

// ---------------------------------------------------------------------------- components

async fn reconcile_components(
    desired: &DesiredConfig,
    ctx: &SupervisorContext,
    last_key: &mut String,
) {
    let key = desired
        .components
        .iter()
        .map(|component| {
            format!(
                "{}@{}:{}",
                component.name,
                component.version.as_deref().unwrap_or("-"),
                component.is_enabled()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    // An update that was put off because this speaker was playing has to be tried again, and the
    // desired state has not changed in the meantime -- so the usual "nothing new" shortcut would
    // mean waiting for the next config edit to install a version that was already asked for.
    let update_waiting = desired.components.iter().any(|component| {
        component.name == crate::update::SONN_CLIENT && update::is_wanted(component)
    });
    if key == *last_key && !update_waiting {
        return;
    }
    *last_key = key;

    // Only the client's own update waits for this; nothing else here interrupts audio.
    let busy = ctx.statuses.device_state() == crate::status::STATE_STREAMING;
    let reports = components::reconcile(&desired.components, busy).await;
    for report in &reports {
        if let Some(error) = report.last_error.as_deref() {
            warn!("component {}: {}", report.name, error);
        } else {
            info!(
                "component {} is {} ({})",
                report.name,
                report.state,
                report.version.as_deref().unwrap_or("no version")
            );
        }
    }
    ctx.statuses.set_components(reports);
}

// ---------------------------------------------------------------------------- plumbing

fn sendspin_url(desired: &DesiredConfig) -> Option<&str> {
    desired
        .sendspin_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
}

/// One OS thread with its own current-thread runtime, for anything that owns an audio device.
fn spawn_audio_thread<F, Fut>(name: String, body: F) -> JoinHandle<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    warn!("cannot start runtime for {}: {}", name, err);
                    return;
                }
            };
            runtime.block_on(body());
        })
        .expect("spawn audio thread")
}

/// Wait for an audio thread to finish without blocking the supervisor's runtime.
async fn join_thread(thread: JoinHandle<()>) {
    if tokio::task::spawn_blocking(move || thread.join())
        .await
        .is_err()
    {
        warn!("audio thread did not shut down cleanly");
    }
}

/// Threads have a 15-character name limit on Linux, and a client id is longer than that.
fn short_name(client_id: &str) -> String {
    client_id.chars().rev().take(6).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_endpoint_reads_as_no_endpoint() {
        let mut desired = DesiredConfig::default();
        assert_eq!(sendspin_url(&desired), None);
        desired.sendspin_url = Some("   ".to_string());
        assert_eq!(sendspin_url(&desired), None);
        desired.sendspin_url = Some(" ws://host:7090/sendspin ".to_string());
        assert_eq!(sendspin_url(&desired), Some("ws://host:7090/sendspin"));
    }
}
