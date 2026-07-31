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
/// again later: the cpal device id, which on Linux reads as `alsa:hw:CARD=DAC,DEV=0` -- cpal's host
/// prefix in front of the ALSA name. `name` is only ever shown to a human.
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
    /// Named extras this build ships: `source`, `beoremote`, `components`.
    pub features: Vec<String>,
}

/// A component this device manages on the server's behalf — software that is not part of the client
/// binary but has to be present for a feature to work.
///
/// Only one so far: `beoremote-bluetoothd`, B&O's patched BlueZ 5.45, which is what makes a Beoremote
/// One serve menus instead of acting like a keyboard. It is GPLv2 and it is a whole daemon, so it is
/// fetched as its own artifact rather than linked into this binary.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentStatus {
    pub name: String,
    /// Version string recorded at install time, or null when nothing is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `absent` | `installed` | `running` | `failed`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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
    /// CPU architecture, as the build reports it: `aarch64`, `arm`, `x86_64`.
    ///
    /// Its own field rather than something the server parses out of `os`: it decides which build of
    /// a managed component this device is handed, and guessing that wrong installs a daemon that
    /// cannot run.
    pub arch: String,
    pub outputs: Vec<OutputDeviceInfo>,
    /// Capture devices, for the source role. Same shape as outputs: a sound card is a sound card.
    pub inputs: Vec<OutputDeviceInfo>,
    pub capabilities: ClientCapabilities,
    /// What is installed of the managed components, so the server knows whether to offer the
    /// features that depend on them.
    pub components: Vec<ComponentStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientStatusRequest {
    /// Device-level roll-up: `playing` when any player is streaming, else `connected`/`idle`/`error`.
    pub state: String,
    pub version: String,
    pub uptime_s: u64,
    pub players: Vec<PlayerStatusReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceStatusReport>,
    /// Re-sent only when the set of sound cards changed -- a USB DAC plugged in after boot has to
    /// show up in the picker without waiting for a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<OutputDeviceInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<OutputDeviceInfo>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentStatus>,
    /// Present only while a remote-pairing window is open or has just finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairingStatusReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beoremote: Option<BeoremoteStatusReport>,
}

/// How the Beoremote One bridge is doing, so the server can say "remote connected" instead of the
/// user guessing from a silent menu.
#[derive(Debug, Clone, Serialize)]
pub struct BeoremoteStatusReport {
    /// `disabled` | `waiting` | `connected` | `error` -- `waiting` means the patched bluetoothd is
    /// not there (or not running), which is the usual reason a remote shows three dots.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<u32>,
    /// Menu revision currently published to the remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu_revision: Option<String>,
    /// Whether B&O's key socket has a peer -- when false, keys go to the kernel as evdev instead.
    pub hid_connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatusReport {
    pub client_id: String,
    /// `idle` | `connecting` | `connected` | `streaming` | `error`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    /// Normalized input level, so the server (and the user) can see the turntable is playing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    /// `unknown` | `present` | `absent`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairingStatusReport {
    /// `scanning` | `pairing` | `paired` | `failed` | `timeout`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    /// Inputs this device should offer as Sendspin sources.
    #[serde(default)]
    pub sources: Vec<DesiredSource>,
    /// Beoremote One support, when this device has the patched BlueZ and a remote paired to it.
    #[serde(default)]
    pub beoremote: Option<DesiredBeoremote>,
    /// Managed software the server wants installed here.
    #[serde(default)]
    pub components: Vec<DesiredComponent>,
    /// One-shot commands queued for this device since the last poll, oldest first. Vocabulary is
    /// the server's; we hand each one to the command hook untouched.
    #[serde(default)]
    pub commands: Vec<DeviceCommand>,
}

/// One Sendspin source: a capture device offered to the server as a selectable input.
///
/// The zone side of this is the server's business. All this device does is capture, measure and
/// send -- and say when it hears something, because an analogue input is started by a human at the
/// turntable, not by the server.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesiredSource {
    /// Sendspin `client_id`. This is what the server's line-in input configuration points at.
    pub client_id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `OutputDeviceInfo::id` of the capture device. Absent means the host default.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Capture format. PCM only for now -- the server transcodes, so there is nothing to gain from
    /// encoding on a Pi.
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u8>,
    #[serde(default)]
    pub channels: Option<u16>,
    /// How much audio goes in one chunk. Smaller is more responsive and more overhead.
    #[serde(default)]
    pub frame_ms: Option<u64>,
    /// Level below which the input counts as silent, in dBFS. The server may also push this live
    /// over Sendspin as VAD settings; whichever arrived last wins.
    #[serde(default)]
    pub threshold_db: Option<f32>,
    /// How long a level change must persist before it is reported.
    #[serde(default)]
    pub hold_ms: Option<u64>,
    /// Transport controls to advertise for the device wired to this input. Each one that arrives is
    /// handed to `control_hook`.
    #[serde(default)]
    pub controls: Option<Vec<String>>,
    /// Script run for transport controls and activation, as `<script> <control>` --
    /// `activate`, `deactivate`, `play`, `pause`, `next`, `previous`. This is how a BeoSound 9000 on
    /// MasterLink gets switched on when the server selects its line-in.
    #[serde(default)]
    pub control_hook: Option<String>,
    /// Stream without waiting to be asked. For an input that is always live and a server that
    /// selects a source only once it reports audio.
    #[serde(default)]
    pub always_on: Option<bool>,
}

/// Beoremote One support for this device.
///
/// The remote talks to a patched `bluetoothd` (the `beoremote-bluetoothd` component) over two unix
/// sockets; this client fills the menus from the server and forwards what the user picks. The menu
/// itself is entirely the server's: a new playlist appears on the remote with nothing deployed here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesiredBeoremote {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Zone whose menu this remote shows and whose keys it drives. Required to do anything.
    #[serde(default)]
    pub zone_id: Option<u32>,
    /// Base URL for the beoremote API. Absent means the server we are registered with.
    #[serde(default)]
    pub api_base_url: Option<String>,
    /// How often to re-read the menu. The server also has a revision, so a poll that finds nothing
    /// changed costs one request and disturbs the remote not at all.
    #[serde(default)]
    pub menu_poll_ms: Option<u64>,
    /// Sendspin `client_id` of the player whose volume the remote's volume keys move. Absent means
    /// the first player on this device.
    #[serde(default)]
    pub volume_player: Option<String>,
    /// Volume points per key press, on the 0-100 scale.
    #[serde(default)]
    pub volume_step: Option<u8>,
    /// Override the socket paths B&O's plugin uses. Defaults match their daemon.
    #[serde(default)]
    pub plugin_socket: Option<String>,
    #[serde(default)]
    pub hog_socket: Option<String>,
}

/// Software the server wants present on this device.
///
/// Kept out of the client binary on purpose. `beoremote-bluetoothd` is GPLv2 (B&O publish their
/// BlueZ patches because they must), and linking a GPL daemon into this binary would relicense the
/// lot. It is also dead weight on the devices that have no B&O remote.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesiredComponent {
    /// Known name: `beoremote-bluetoothd`. Anything else is refused rather than guessed at.
    pub name: String,
    /// Version the server wants installed. A mismatch with what is here triggers a fetch.
    #[serde(default)]
    pub version: Option<String>,
    /// Where to fetch the tarball.
    #[serde(default)]
    pub url: Option<String>,
    /// Hex sha256 of the tarball. Required: this installs a daemon that owns the Bluetooth adapter.
    #[serde(default)]
    pub sha256: Option<String>,
    /// False removes the component's service instead of installing it.
    #[serde(default)]
    pub enabled: Option<bool>,
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

impl DesiredSource {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Everything a source can only change by reconnecting. Signal thresholds are absent: those are
    /// applied to a running capture, exactly as the server's live VAD settings are.
    pub fn restart_key(&self, sendspin_url: &str) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            sendspin_url,
            self.client_id,
            self.name.as_deref().unwrap_or_default(),
            self.input.as_deref().unwrap_or_default(),
            self.codec,
            self.sample_rate,
            self.bit_depth,
            self.channels,
            self.frame_ms,
            self.controls,
            self.always_on,
        )
    }
}

impl DesiredBeoremote {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false) && self.zone_id.is_some()
    }
}

impl DesiredComponent {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
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
