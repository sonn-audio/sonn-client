//! Keeps the running players equal to what the server asked for.
//!
//! The status loop drops the server's desired state into a watch channel; this reconciles against it.
//! A change that a live player can absorb -- volume, mute, static delay -- is handed to it. A change
//! it cannot -- another sound card, another rate, another server -- restarts just that player. Adding
//! a second room to a device with two DACs starts one task and leaves the other playing.

use crate::hooks::VolumeHook;
use crate::models::{DesiredConfig, DesiredPlayer};
use crate::player::{self, LiveSettings, PlayerParams};
use crate::status::Registry;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

const DEFAULT_VOLUME: u8 = 100;

struct RunningPlayer {
    /// Everything that would need a reconnect to change; a mismatch restarts the task.
    key: String,
    /// The last state the *server* asked for, so a poll that repeats it does not overwrite a level
    /// the user has since changed from the app. Only an actual change in the desired config is
    /// pushed down.
    last_desired: LiveSettings,
    settings_tx: watch::Sender<LiveSettings>,
    handle: JoinHandle<()>,
}

pub async fn run(
    mut desired_rx: watch::Receiver<DesiredConfig>,
    statuses: Registry,
    fallback_volume_hook: Option<String>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut running: HashMap<String, RunningPlayer> = HashMap::new();

    loop {
        if *stop_rx.borrow() {
            break;
        }
        let desired = desired_rx.borrow_and_update().clone();
        reconcile(
            &desired,
            &mut running,
            &statuses,
            fallback_volume_hook.as_deref(),
        )
        .await;

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
        }
    }

    stop_all(&mut running).await;
    statuses.retain(&[]);
}

async fn reconcile(
    desired: &DesiredConfig,
    running: &mut HashMap<String, RunningPlayer>,
    statuses: &Registry,
    fallback_volume_hook: Option<&str>,
) {
    let Some(url) = desired
        .sendspin_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        // Registered but not yet pointed at a Sendspin endpoint. Stay up and keep polling: the
        // device is waiting to be given a room, which is not an error.
        if !running.is_empty() {
            info!("no sendspin endpoint configured; stopping all players");
            stop_all(running).await;
            statuses.retain(&[]);
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

    // Stop what is gone or has to be rebuilt. Awaiting the abort matters: the replacement opens the
    // same sound card, and ALSA hands out "device or resource busy" to whoever asks first.
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
            stop(entry).await;
        }
    }

    for player in wanted {
        let settings = LiveSettings {
            volume: player.volume.unwrap_or(DEFAULT_VOLUME).min(100),
            muted: player.muted.unwrap_or(false),
            static_delay_ms: player.static_delay_ms.unwrap_or(0),
        };

        if let Some(entry) = running.get_mut(&player.client_id) {
            if entry.last_desired != settings {
                entry.last_desired = settings.clone();
                let _ = entry.settings_tx.send(settings);
            }
            continue;
        }

        let params = PlayerParams {
            url: url.to_string(),
            client_id: player.client_id.clone(),
            name: player
                .name
                .clone()
                .or_else(|| desired.device_name.clone())
                .unwrap_or_else(|| player.client_id.clone()),
            output: player.output.clone(),
            codecs: player.codecs.clone().unwrap_or_default(),
            sample_rate: player.sample_rate,
            bit_depth: player.bit_depth,
            channels: player.channels,
            buffer_ms: player.buffer_ms,
            required_lead_time_ms: player.required_lead_time_ms,
        };
        let volume_hook = player
            .volume_hook
            .clone()
            .or_else(|| fallback_volume_hook.map(str::to_string))
            .map(VolumeHook::new);
        let status = statuses.handle(
            &player.client_id,
            player.output.clone(),
            settings.volume,
            settings.muted,
            settings.static_delay_ms,
        );
        let (settings_tx, settings_rx) = watch::channel(settings.clone());

        info!(
            client_id = %params.client_id,
            name = %params.name,
            output = params.output.as_deref().unwrap_or("(default)"),
            "starting player"
        );
        let key = player.restart_key(url);
        let handle = tokio::spawn(async move {
            let mut settings_rx = settings_rx;
            let mut backoff = Backoff::new();
            loop {
                match player::run_session(&params, &mut settings_rx, &status, volume_hook.as_ref())
                    .await
                {
                    Ok(()) => {
                        backoff.reset();
                    }
                    Err(err) => {
                        warn!(client_id = %params.client_id, "player session failed: {:#}", err);
                        status.set_error(format!("{:#}", err));
                    }
                }
                tokio::time::sleep(backoff.next_delay()).await;
            }
        });

        running.insert(
            player.client_id.clone(),
            RunningPlayer {
                key,
                last_desired: settings,
                settings_tx,
                handle,
            },
        );
    }

    statuses.retain(&wanted_ids);
}

async fn stop(entry: RunningPlayer) {
    entry.handle.abort();
    // Awaited so the task's SyncedPlayer is really dropped -- and the card really released -- before
    // anything tries to open it again.
    let _ = entry.handle.await;
}

async fn stop_all(running: &mut HashMap<String, RunningPlayer>) {
    for (_, entry) in running.drain().collect::<Vec<_>>() {
        stop(entry).await;
    }
}

/// 1s doubling to 30s. A server restart should be picked up in a second; a card that will never open
/// should not be retried in a hot loop.
struct Backoff {
    current: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            current: Duration::from_secs(1),
        }
    }

    fn reset(&mut self) {
        self.current = Duration::from_secs(1);
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(Duration::from_secs(30));
        delay
    }
}
