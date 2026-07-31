//! What this device is doing, shared between the player tasks that know it and the status loop that
//! reports it.
//!
//! Players write here from their own tasks; the poller reads a snapshot. Nothing blocks on anything:
//! a lock is only ever held for a field assignment, never across an await.

use crate::models::PlayerStatusReport;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Player lifecycle as the server sees it. `Streaming` is the only one that means audio is moving.
pub const STATE_IDLE: &str = "idle";
pub const STATE_CONNECTING: &str = "connecting";
pub const STATE_CONNECTED: &str = "connected";
pub const STATE_STREAMING: &str = "streaming";
pub const STATE_ERROR: &str = "error";

#[derive(Debug, Clone)]
struct PlayerSnapshot {
    state: String,
    output: Option<String>,
    codec: Option<String>,
    sample_rate: Option<u32>,
    bit_depth: Option<u8>,
    channels: Option<u16>,
    volume: u8,
    muted: bool,
    static_delay_ms: u16,
    clock_rtt_ms: Option<f64>,
    clock_quality: Option<String>,
    last_error: Option<String>,
}

impl PlayerSnapshot {
    fn new(output: Option<String>, volume: u8, muted: bool, static_delay_ms: u16) -> Self {
        Self {
            state: STATE_IDLE.to_string(),
            output,
            codec: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            volume,
            muted,
            static_delay_ms,
            clock_rtt_ms: None,
            clock_quality: None,
            last_error: None,
        }
    }
}

type Shared = Arc<Mutex<HashMap<String, PlayerSnapshot>>>;

#[derive(Clone)]
pub struct Registry {
    players: Shared,
    started: Instant,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            players: Arc::new(Mutex::new(HashMap::new())),
            started: Instant::now(),
        }
    }

    /// Hand a player its own writer, creating (or resetting) its entry.
    pub fn handle(
        &self,
        client_id: &str,
        output: Option<String>,
        volume: u8,
        muted: bool,
        static_delay_ms: u16,
    ) -> PlayerHandle {
        if let Ok(mut players) = self.players.lock() {
            players.insert(
                client_id.to_string(),
                PlayerSnapshot::new(output, volume, muted, static_delay_ms),
            );
        }
        PlayerHandle {
            client_id: client_id.to_string(),
            players: Arc::clone(&self.players),
        }
    }

    /// Drop players the server no longer wants, so a removed room stops being reported.
    pub fn retain(&self, wanted: &[String]) {
        if let Ok(mut players) = self.players.lock() {
            players.retain(|client_id, _| wanted.iter().any(|id| id == client_id));
        }
    }

    pub fn reports(&self) -> Vec<PlayerStatusReport> {
        let Ok(players) = self.players.lock() else {
            return Vec::new();
        };
        let mut reports: Vec<PlayerStatusReport> = players
            .iter()
            .map(|(client_id, snapshot)| PlayerStatusReport {
                client_id: client_id.clone(),
                state: snapshot.state.clone(),
                output: snapshot.output.clone(),
                codec: snapshot.codec.clone(),
                sample_rate: snapshot.sample_rate,
                bit_depth: snapshot.bit_depth,
                channels: snapshot.channels,
                volume: snapshot.volume,
                muted: snapshot.muted,
                static_delay_ms: snapshot.static_delay_ms,
                clock_rtt_ms: snapshot.clock_rtt_ms,
                clock_quality: snapshot.clock_quality.clone(),
                last_error: snapshot.last_error.clone(),
            })
            .collect();
        reports.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        reports
    }

    /// Device-level roll-up. Playing wins over connected, and an error only shows when nothing else
    /// is working -- a device with one dead card and one playing room is not "in error".
    pub fn device_state(&self) -> String {
        let reports = self.reports();
        if reports.is_empty() {
            return STATE_IDLE.to_string();
        }
        for state in [STATE_STREAMING, STATE_CONNECTED, STATE_CONNECTING] {
            if reports.iter().any(|report| report.state == state) {
                return state.to_string();
            }
        }
        if reports.iter().all(|report| report.state == STATE_ERROR) {
            return STATE_ERROR.to_string();
        }
        STATE_IDLE.to_string()
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct PlayerHandle {
    client_id: String,
    players: Shared,
}

impl PlayerHandle {
    fn update(&self, apply: impl FnOnce(&mut PlayerSnapshot)) {
        if let Ok(mut players) = self.players.lock() {
            if let Some(snapshot) = players.get_mut(&self.client_id) {
                apply(snapshot);
            }
        }
    }

    pub fn set_state(&self, state: &str) {
        self.update(|snapshot| snapshot.state = state.to_string());
    }

    /// Entering a state that means "working" also clears the last error: leaving it behind makes a
    /// recovered player look broken forever in the UI.
    pub fn set_state_ok(&self, state: &str) {
        self.update(|snapshot| {
            snapshot.state = state.to_string();
            snapshot.last_error = None;
        });
    }

    pub fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        self.update(|snapshot| {
            snapshot.state = STATE_ERROR.to_string();
            snapshot.last_error = Some(message);
        });
    }

    pub fn set_format(&self, codec: &str, sample_rate: u32, bit_depth: u8, channels: u16) {
        self.update(|snapshot| {
            snapshot.codec = Some(codec.to_string());
            snapshot.sample_rate = Some(sample_rate);
            snapshot.bit_depth = Some(bit_depth);
            snapshot.channels = Some(channels);
        });
    }

    pub fn clear_format(&self) {
        self.update(|snapshot| {
            snapshot.codec = None;
            snapshot.sample_rate = None;
            snapshot.bit_depth = None;
            snapshot.channels = None;
        });
    }

    pub fn set_volume(&self, volume: u8, muted: bool) {
        self.update(|snapshot| {
            snapshot.volume = volume;
            snapshot.muted = muted;
        });
    }

    pub fn set_static_delay(&self, delay_ms: u16) {
        self.update(|snapshot| snapshot.static_delay_ms = delay_ms);
    }

    pub fn set_clock(&self, rtt_ms: Option<f64>, quality: Option<String>) {
        self.update(|snapshot| {
            snapshot.clock_rtt_ms = rtt_ms;
            snapshot.clock_quality = quality;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_playing_room_outranks_a_broken_one() {
        let registry = Registry::new();
        let good = registry.handle("a", None, 100, false, 0);
        let bad = registry.handle("b", None, 100, false, 0);
        good.set_state(STATE_STREAMING);
        bad.set_error("card busy");
        assert_eq!(registry.device_state(), STATE_STREAMING);
    }

    #[test]
    fn error_is_reported_only_when_nothing_works() {
        let registry = Registry::new();
        let one = registry.handle("a", None, 100, false, 0);
        one.set_error("card busy");
        assert_eq!(registry.device_state(), STATE_ERROR);
    }

    #[test]
    fn recovering_clears_the_last_error() {
        let registry = Registry::new();
        let player = registry.handle("a", None, 100, false, 0);
        player.set_error("socket closed");
        player.set_state_ok(STATE_CONNECTED);
        let report = registry.reports().remove(0);
        assert_eq!(report.state, STATE_CONNECTED);
        assert!(report.last_error.is_none());
    }

    #[test]
    fn dropped_players_stop_being_reported() {
        let registry = Registry::new();
        registry.handle("a", None, 100, false, 0);
        registry.handle("b", None, 100, false, 0);
        registry.retain(&["a".to_string()]);
        let reports = registry.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].client_id, "a");
    }
}
