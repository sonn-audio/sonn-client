//! The management contract with Sonn Core.
//!
//! Audio never travels through these types -- that is the Sendspin protocol's job and it stays
//! untouched, so this client is an ordinary spec client to any Sendspin server. What lives here is
//! everything the *spec has no message for*: which sound card to open, which server to dial, what
//! the room is called. The device reports what it has, the server decides what to do with it, and
//! the reply to every request is the full desired state so a config change lands on the next poll.
//!
//! See `docs/PROTOCOL.md` for the endpoint shapes and the server-side view of these payloads.

use serde::{Deserialize, Serialize};

/// One audio output this device can play through, as offered to the server for selection.
///
/// `id` is what comes back in `DesiredPlayer::output`, so it has to be something we can resolve
/// again later: the cpal device id (on Linux the ALSA name, e.g. `hw:CARD=DAC,DEV=0`). `name` is
/// only ever shown to a human.
// `Hash` so the status loop can tell whether the card list changed without diffing it by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputDeviceInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_rates: Vec<u32>,
    /// True for the host's default output. The server picks this when the user has not chosen yet.
    #[serde(default)]
    pub is_default: bool,
}

/// What this build can do, so the server can offer the right things in its UI without probing.
#[derive(Debug, Clone, Serialize)]
pub struct ClientCapabilities {
    /// Codecs we will accept for the player role, best first.
    pub codecs: Vec<String>,
    /// How many players (one per sound card) this device can run at once.
    pub max_players: u8,
    /// Named extras this build ships. Empty for now; `beoremote`/`bluetooth` land here.
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientRegisterRequest {
    /// Stable identity of the *device*, generated once and kept in config.toml. Also the default
    /// Sendspin `client_id` when the server does not assign one.
    pub device_id: String,
    pub agent: String,
    pub version: String,
    pub hostname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Hardware model, e.g. `Raspberry Pi 4 Model B Rev 1.4`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// OS description, e.g. `Debian GNU/Linux 12 (bookworm) aarch64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    pub outputs: Vec<OutputDeviceInfo>,
    pub capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatusRequest {
    /// Device-level roll-up: `playing` when any player is streaming, else `connected`/`idle`/`error`.
    pub state: String,
    pub version: String,
    pub uptime_s: u64,
    pub players: Vec<PlayerStatusReport>,
    /// Re-sent only when the set of sound cards changed -- a USB DAC plugged in after boot has to
    /// show up in the picker without waiting for a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputDeviceInfo>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerStatusReport {
    pub client_id: String,
    /// `idle` | `connecting` | `connected` | `streaming` | `error`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    pub volume: u8,
    pub muted: bool,
    pub static_delay_ms: u16,
    /// Round-trip time of the last clock sync exchange, in ms. The one number that says whether
    /// this device can hold sync at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// The server's desired state for this device. Returned by both register and status, so every poll
/// is also a config fetch.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesiredConfig {
    /// Friendly device name. Only used for logs and as a fallback player name.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Sendspin endpoint to dial, e.g. `ws://192.168.1.209:7090/sendspin`. Absent means "not yet
    /// configured": we stay registered and keep polling, playing nothing.
    #[serde(default)]
    pub sendspin_url: Option<String>,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub players: Vec<DesiredPlayer>,
    /// One-shot commands queued for this device since the last poll, oldest first. Vocabulary is
    /// the server's; we hand each one to the command hook untouched.
    #[serde(default)]
    pub commands: Vec<DeviceCommand>,
}

/// One Sendspin player instance: a sound card, a name, and the timing/volume state to start with.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesiredPlayer {
    /// Sendspin `client_id`. This is what the zone's output configuration points at, so it must be
    /// stable for as long as the user expects that room to keep working.
    pub client_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `OutputDeviceInfo::id` to open. Absent/empty means the host default.
    #[serde(default)]
    pub output: Option<String>,
    /// False parks the player without unregistering it -- the room stays configured, just silent.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Codecs to advertise, best first. Absent means everything this build can decode.
    #[serde(default)]
    pub codecs: Option<Vec<String>>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u8>,
    #[serde(default)]
    pub channels: Option<u16>,
    /// Delay this device's chain adds *after* the audio port (an amp, an active speaker). We
    /// subtract it from server timestamps and so play that much earlier.
    #[serde(default)]
    pub static_delay_ms: Option<u16>,
    /// Starting volume/mute. Live changes arrive over Sendspin as player commands, not here.
    #[serde(default)]
    pub volume: Option<u8>,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub buffer_ms: Option<u32>,
    #[serde(default)]
    pub required_lead_time_ms: Option<u32>,
    /// Hardware volume: run this command with the effective level (0-100) instead of applying gain
    /// in software. Same contract as the reference client's `--hook-set-volume`, so an existing
    /// script keeps working. Server-pushed because which speaker needs it is a server-side fact.
    #[serde(default)]
    pub volume_hook: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl DesiredPlayer {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Everything that can only change by reconnecting. Volume, mute and static delay are absent on
    /// purpose: those apply to a live player, and a rate change is the only reason to drop audio.
    pub fn restart_key(&self, sendspin_url: &str) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            sendspin_url,
            self.client_id,
            self.name.as_deref().unwrap_or_default(),
            self.output.as_deref().unwrap_or_default(),
            self.codecs,
            self.sample_rate,
            self.bit_depth,
            self.channels,
            self.buffer_ms,
            self.required_lead_time_ms,
        )
    }
}
