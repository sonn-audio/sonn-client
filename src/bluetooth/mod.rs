//! Bluetooth audio into a zone: a phone pairs with this device and plays to the room.
//!
//! What a BeoSound Core does, and what this has to match: be findable under the room's name, pair
//! without a dance, accept audio at the best quality both ends can manage, show what is playing, and
//! let the phone's transport keys work. None of that reaches the server as Bluetooth -- A2DP is
//! terminated here and the audio goes on as an ordinary source, so the server needs no receiver and
//! no codec, and everything it already does with a line input applies unchanged.
//!
//! The parts, and which end of BlueZ each one talks to:
//!
//! ```text
//! pairing        org.bluez.Agent1              a phone asks, we answer
//! visibility     org.bluez.Adapter1            Discoverable/Pairable, on a timer
//! audio          org.bluez.MediaEndpoint1      we advertise a sink; bluez hands us a transport
//! the stream     org.bluez.MediaTransport1     Acquire() gives a socket carrying RTP/SBC
//! metadata       org.bluez.MediaPlayer1        AVRCP: what is playing, and the keys back
//! ```
//!
//! Deliberately *not* here: restarting bluetoothd. A remote whose link is cut that way stops
//! advertising until it is factory reset, which cost an evening once.

mod decode;
mod endpoint;
mod metadata;
mod transport;

use crate::models::DesiredBluetooth;
use crate::status::Registry;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{interface, proxy, Connection};

pub use endpoint::A2dpEndpoint;
pub use metadata::{NowPlaying, PlayerControl};

const BLUEZ: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const AGENT_PATH: &str = "/sonn/bluetooth/agent";
/// How long the device stays findable when nobody says otherwise.
///
/// Visible forever is an invitation to the street; visible never is unusable. Two minutes is what
/// every consumer device settles on, and it is long enough to walk to a phone and back.
const DEFAULT_DISCOVERABLE: Duration = Duration::from_secs(120);
/// How often to re-read what the adapter and its devices are doing.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
trait AgentManager {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device {
    /// Ask for the profiles this device and we have in common -- for a phone, A2DP.
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    /// Ask for one profile by name, which is what to do when the link is up but the audio is not.
    fn connect_profile(&self, uuid: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_trusted(&self, trusted: bool) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter {
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_alias(&self, alias: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_powered(&self, powered: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_discoverable(&self, discoverable: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_discoverable_timeout(&self, seconds: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_pairable(&self, pairable: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable(&self) -> zbus::Result<bool>;
}

/// How much audio has arrived, so "streaming" is something measured rather than assumed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StreamReport {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
}

/// What this device is doing with Bluetooth, for the screen that configures it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BluetoothStatus {
    /// Whether the radio is set up for this zone at all.
    pub enabled: bool,
    pub zone_id: Option<u32>,
    /// The name a phone sees.
    pub name: Option<String>,
    /// Whether the device can be found right now, and for how much longer.
    pub discoverable: bool,
    /// Phones that have been paired, most recently connected first.
    pub devices: Vec<PairedPhone>,
    /// What the connected phone is playing, when it says.
    pub now_playing: Option<NowPlaying>,
    /// What has arrived over the air since the stream started.
    pub stream: Option<StreamReport>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PairedPhone {
    pub address: String,
    pub name: String,
    pub connected: bool,
    /// Whether this is the one currently sending audio.
    pub streaming: bool,
}

/// What the module needs to know to run.
#[derive(Debug, Clone)]
pub struct BluetoothConfig {
    pub zone_id: u32,
    pub name: String,
    pub discoverable: Duration,
    pub pin: Option<String>,
    pub control: bool,
    /// Where the decoded audio goes, and under which name the server knows it.
    ///
    /// The audio leaves here as an ordinary sendspin source, so a phone in a room is a line input
    /// as far as everything downstream is concerned.
    pub server_url: Option<String>,
    pub client_id: String,
    /// The rate to decode to: the room's output rate, so nothing downstream has to convert.
    pub sample_rate: u32,
}

impl BluetoothConfig {
    pub fn from_desired(
        desired: &DesiredBluetooth,
        server_url: Option<&str>,
        device_id: &str,
    ) -> Option<Self> {
        if desired.enabled != Some(true) {
            return None;
        }
        let zone_id = desired.zone_id?;
        Some(Self {
            server_url: server_url.map(str::to_string),
            // The server names it; falling back keeps a hand-run client working, and the two
            // spellings agree because both are this device's id with the same suffix.
            // 48 kHz unless the server says otherwise: it is what every output here runs at, and a
            // phone that sends it already is then carried untouched.
            sample_rate: desired
                .sample_rate
                .filter(|rate| *rate > 0)
                .unwrap_or(48_000),
            client_id: desired
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map_or_else(|| format!("{device_id}-bt"), str::to_string),
            zone_id,
            name: desired
                .name
                .clone()
                .unwrap_or_else(|| format!("Sonn zone {zone_id}")),
            discoverable: desired
                .discoverable_seconds
                .map(|seconds| Duration::from_secs(u64::from(seconds)))
                .unwrap_or(DEFAULT_DISCOVERABLE),
            pin: desired.pin.clone().filter(|pin| !pin.trim().is_empty()),
            control: desired.control.unwrap_or(true),
        })
    }

    /// A change to any of this needs the radio set up again; a different zone name does not.
    pub fn restart_key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.zone_id,
            self.pin.as_deref().unwrap_or(""),
            self.control
        )
    }
}

/// Where an operator's commands are handed in.
///
/// The supervisor owns the running radio and the status poller receives the commands, so the two
/// meet here rather than through either of them knowing about the other. Empty means Bluetooth is
/// not set up for any zone on this device, and a command for it is a command with nowhere to go.
#[derive(Clone, Default)]
pub struct CommandBus(std::sync::Arc<std::sync::Mutex<Option<mpsc::Sender<Command>>>>);

impl CommandBus {
    pub fn set(&self, sender: Option<mpsc::Sender<Command>>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = sender;
        }
    }

    /// Hand a command over. Returns false when there is nothing listening, which is worth saying out
    /// loud: a button that does nothing should not look like a button that worked.
    pub fn send(&self, command: Command) -> bool {
        let sender = self.0.lock().ok().and_then(|slot| slot.clone());
        match sender {
            Some(sender) => sender.try_send(command).is_ok(),
            None => false,
        }
    }
}

/// What the server can ask this module to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// Be findable and pairable for the configured window.
    Discoverable,
    /// Forget one phone, by address.
    Forget(String),
    /// Press a transport key on the phone: what the room does, the phone does.
    ///
    /// The zone is the remote here. Someone who starts the music from a wall panel or a Beoremote
    /// expects the phone in their pocket to start playing, and AVRCP is how that is said.
    Control(metadata::PlayerControl),
}

/// The agent that answers a phone's pairing questions.
///
/// A speaker has no keypad, so it cannot type a passkey a phone shows -- but it can *display* one,
/// which is what a fixed PIN means here. Without one this is a `NoInputNoOutput` device and pairing
/// is confirmed on the phone alone, which is how every modern speaker behaves.
struct Agent {
    pin: Option<String>,
}

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, device: OwnedObjectPath) -> String {
        let pin = self.pin.clone().unwrap_or_else(|| "0000".to_string());
        info!("bluetooth: {} asked for a pin code", device.as_str());
        pin
    }

    fn display_pin_code(&self, device: OwnedObjectPath, pin_code: String) {
        info!("bluetooth: show {pin_code} to pair {}", device.as_str());
    }

    fn request_passkey(&self, device: OwnedObjectPath) -> u32 {
        let passkey = self
            .pin
            .as_deref()
            .and_then(|pin| pin.parse::<u32>().ok())
            .unwrap_or(0);
        info!("bluetooth: {} asked for a passkey", device.as_str());
        passkey
    }

    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        info!("bluetooth: show {passkey:06} to pair {}", device.as_str());
    }

    fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) {
        // Nothing to compare it against on this end; the phone shows the same number and its user
        // is the one who decides.
        info!("bluetooth: confirming {passkey:06} for {}", device.as_str());
    }

    fn request_authorization(&self, device: OwnedObjectPath) {
        info!("bluetooth: authorising {}", device.as_str());
    }

    /// Which profile a paired phone may use.
    ///
    /// Audio and its remote control, and nothing else: a phone that is paired for music has no
    /// business reaching this device's other services.
    fn authorize_service(&self, device: OwnedObjectPath, uuid: String) -> zbus::fdo::Result<()> {
        if is_audio_service(&uuid) {
            info!("bluetooth: {} may use {uuid}", device.as_str());
            return Ok(());
        }
        warn!("bluetooth: refused {uuid} for {}", device.as_str());
        Err(zbus::fdo::Error::AccessDenied(format!(
            "{uuid} is not audio"
        )))
    }

    fn cancel(&self) {
        debug!("bluetooth: the pairing was cancelled");
    }
}

/// Whether a paired device is something that plays audio to us.
///
/// A phone offers Audio *Source*; a remote offers a keyboard and a battery. The service list is the
/// honest way to tell them apart -- better than a name, which is a phone's to choose.
fn offers_audio(properties: &Properties) -> bool {
    let Some(uuids) = properties.get("UUIDs") else {
        return false;
    };
    let Ok(uuids) = Vec::<String>::try_from(uuids.clone()) else {
        return false;
    };
    uuids
        .iter()
        .any(|uuid| uuid.to_ascii_lowercase().starts_with(A2DP_SOURCE))
}

/// A2DP Source: "I have audio to send you", which is what a phone says and a remote does not.
const A2DP_SOURCE: &str = "0000110a";
const A2DP_SOURCE_UUID: &str = "0000110a-0000-1000-8000-00805f9b34fb";

/// A2DP source and sink, AVRCP, and the two audio/video umbrella UUIDs a phone offers alongside.
fn is_audio_service(uuid: &str) -> bool {
    const AUDIO: [&str; 6] = [
        "0000110a", // Audio Source (the phone)
        "0000110b", // Audio Sink (us)
        "0000110c", // A/V Remote Control Target
        "0000110d", // Advanced Audio Distribution
        "0000110e", // A/V Remote Control
        "0000111e", // Handsfree -- offered by phones, refused politely below
    ];
    let uuid = uuid.to_ascii_lowercase();
    // Handsfree is listed so it is recognised, but it is not audio we want: it takes the phone into
    // call mode at 8 or 16 kHz and would replace the music with a telephone.
    if uuid.starts_with("0000111e") || uuid.starts_with("0000111f") {
        return false;
    }
    AUDIO.iter().any(|known| uuid.starts_with(known))
}

/// Name the adapter, which is what a phone and a Beoremote One both read.
pub async fn set_adapter_name(name: &str) -> Result<()> {
    let connection = Connection::system()
        .await
        .context("connect to the system bus")?;
    let adapter = AdapterProxy::builder(&connection)
        .path(ADAPTER_PATH)?
        .build()
        .await
        .context("talk to the adapter")?;
    adapter.set_alias(name).await.context("name the adapter")?;
    Ok(())
}

/// Set the radio up for this zone and keep it that way until told otherwise.
pub async fn run(
    config: BluetoothConfig,
    statuses: Registry,
    mut commands: mpsc::Receiver<Command>,
    volume_tx: mpsc::Sender<crate::supervisor::VolumeRequest>,
) {
    loop {
        match serve(&config, &statuses, &mut commands, &volume_tx).await {
            Ok(()) => {
                info!("bluetooth: stopped");
                return;
            }
            Err(err) => {
                warn!("bluetooth: {err:#}");
                statuses.set_bluetooth(Some(BluetoothStatus {
                    enabled: true,
                    zone_id: Some(config.zone_id),
                    name: Some(config.name.clone()),
                    last_error: Some(format!("{err:#}")),
                    ..Default::default()
                }));
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

async fn serve(
    config: &BluetoothConfig,
    statuses: &Registry,
    commands: &mut mpsc::Receiver<Command>,
    volume_tx: &mpsc::Sender<crate::supervisor::VolumeRequest>,
) -> Result<()> {
    let connection = Connection::system()
        .await
        .context("connect to the system bus")?;
    let adapter = AdapterProxy::builder(&connection)
        .path(ADAPTER_PATH)?
        .build()
        .await
        .context("talk to the adapter")?;

    // The name is not set here: one radio carries one name for everything that reads it, so the
    // supervisor sets it from whichever zone claims this device -- see `set_adapter_name`.
    adapter.set_powered(true).await.ok();
    // Pairable, but not findable until someone asks. A speaker that is permanently discoverable is
    // one every passer-by can see.
    adapter.set_pairable(true).await.ok();
    adapter
        .set_discoverable_timeout(u32::try_from(config.discoverable.as_secs()).unwrap_or(120))
        .await
        .ok();

    let _agent = AgentGuard::register(&connection, config.pin.clone()).await?;
    let endpoint = A2dpEndpoint::register(&connection).await?;
    let counters = std::sync::Arc::new(transport::StreamCounters::default());
    // Which transport is being read, so a phone that reconnects is picked up and one that is already
    // being read is not read twice.
    let mut reading: Option<OwnedObjectPath> = None;
    let mut streaming: Option<Streaming> = None;
    // When each phone was last asked for its audio, so it is asked at most once in a while.
    let mut asked: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    let mut last_bytes = 0u64;
    // The phone's slider, as last seen. Absolute volume arrives as a property on the transport, and
    // it is the one thing B&O put real work into on this path: a phone whose volume does nothing is
    // a phone whose owner turns the speaker up by hand and then blames the speaker.
    let mut last_volume: Option<u16> = None;
    info!(
        zone_id = config.zone_id,
        name = %config.name,
        "bluetooth ready; phones can pair when the window is open"
    );

    let mut poll = tokio::time::interval(POLL_INTERVAL);
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None => return Ok(()),
                    Some(Command::Discoverable) => {
                        match adapter.set_discoverable(true).await {
                            Ok(()) => info!(
                                "bluetooth: findable as {} for {}s",
                                config.name,
                                config.discoverable.as_secs()
                            ),
                            Err(err) => warn!("bluetooth: could not become findable: {err}"),
                        }
                    }
                    Some(Command::Forget(address)) => forget(&connection, &adapter, &address).await,
                    Some(Command::Control(control)) => {
                        if !config.control {
                            debug!("bluetooth: {control:?} ignored; this zone does not drive the phone");
                        } else if let Err(err) = metadata::control(&connection, control).await {
                            warn!("bluetooth: {control:?} did not reach the phone: {err:#}");
                        }
                    }
                }
            }
            _ = poll.tick() => {
                follow_stream(
                    &connection,
                    &endpoint,
                    config,
                    &counters,
                    &mut reading,
                    &mut streaming,
                )
                .await;
                follow_volume(&connection, &endpoint, config, volume_tx, &mut last_volume).await;
                let mut report =
                    inspect(&connection, &adapter, config, &endpoint, &mut asked).await;
                let stream = StreamReport {
                    packets: counters.packets.load(std::sync::atomic::Ordering::Relaxed),
                    frames: counters.frames(),
                    bytes: counters.bytes.load(std::sync::atomic::Ordering::Relaxed),
                };
                // Said out loud while it moves: this is how anyone can tell audio is arriving,
                // before there is any sound to hear it by.
                if stream.bytes != last_bytes {
                    info!(
                        "bluetooth: {} packets, {} sbc frames, {} kB",
                        stream.packets,
                        stream.frames,
                        stream.bytes / 1024
                    );
                    last_bytes = stream.bytes;
                }
                report.stream = Some(stream);
                statuses.set_bluetooth(Some(report));
            }
        }
    }
}

/// Follow the phone's own volume slider.
///
/// AVRCP absolute volume runs 0-127 and the rest of this system runs 0-100, so the one is scaled to
/// the other -- rounding up, so that a nudge off zero is audible rather than silently still zero.
/// Only changes are passed on: the property is read every few seconds and re-applying the same
/// number would fight whoever is turning the knob in the room.
async fn follow_volume(
    connection: &Connection,
    endpoint: &A2dpEndpoint,
    config: &BluetoothConfig,
    volume_tx: &mpsc::Sender<crate::supervisor::VolumeRequest>,
    last: &mut Option<u16>,
) {
    if !config.control {
        return;
    }
    let Some(path) = endpoint.transport().path else {
        *last = None;
        return;
    };
    let Ok(builder) = endpoint::MediaTransportProxy::builder(connection).path(path) else {
        return;
    };
    let Ok(transport) = builder.build().await else {
        return;
    };
    let Ok(volume) = transport.volume().await else {
        return;
    };
    if *last == Some(volume) {
        return;
    }
    *last = Some(volume);
    let scaled = (u32::from(volume) * 100).div_ceil(127).min(100) as u8;
    info!("bluetooth: the phone set the volume to {scaled}");
    let _ = volume_tx
        .send(crate::supervisor::VolumeRequest {
            client_id: None,
            intent: crate::supervisor::VolumeIntent::Set(scaled),
        })
        .await;
}

/// One phone's audio, from the moment it starts until it stops.
///
/// The decoder and the source live and die together with the stream: a phone that stops playing
/// releases the transport, ffmpeg is closed, and the source ends -- which is what tells the server
/// the room's Bluetooth input has gone quiet.
struct Streaming {
    decoder: decode::Decoder,
    source: tokio::task::JoinHandle<()>,
    /// Devices put aside for the duration, to be called back when the music stops.
    quieted: Vec<OwnedObjectPath>,
}

impl Drop for Streaming {
    fn drop(&mut self) {
        self.source.abort();
    }
}

/// Take the socket as soon as a phone starts playing, and let it go when it stops.
///
/// bluez only hands the socket over while the transport is `active`, and acquiring it twice is an
/// error -- so which one is being read is remembered rather than asked for again.
async fn follow_stream(
    connection: &Connection,
    endpoint: &A2dpEndpoint,
    config: &BluetoothConfig,
    counters: &std::sync::Arc<transport::StreamCounters>,
    reading: &mut Option<OwnedObjectPath>,
    streaming: &mut Option<Streaming>,
) {
    let transport_state = endpoint.transport();
    let Some(path) = transport_state.path.clone() else {
        // The phone disconnected or stopped: close the decoder and end the source, which is what
        // tells the server this input has gone quiet.
        if let Some(stopped) = streaming.take() {
            info!("bluetooth: the stream ended");
            wake_the_radio(connection, &stopped.quieted).await;
        }
        *reading = None;
        return;
    };
    // Playing, about to play, or stopped: bluez keeps the transport in place across a pause, so its
    // state is the only thing that says whether there is still music.
    let state = transport::state(connection, &path).await;
    if state != "active" && state != "pending" {
        if let Some(stopped) = streaming.take() {
            info!("bluetooth: the phone stopped");
            wake_the_radio(connection, &stopped.quieted).await;
        }
        *reading = None;
        return;
    }
    if reading.as_ref() == Some(&path) {
        return;
    }

    match transport::acquire(connection, &path).await {
        Ok((fd, read_mtu)) => {
            info!(
                "bluetooth: taking the stream on {} ({} Hz, {}ch, bitpool {})",
                path.as_str(),
                transport_state.sample_rate,
                transport_state.channels,
                transport_state.bitpool
            );
            *reading = Some(path);

            // Decoded to the rate the server wants, not the rate the phone sends.
            //
            // A phone sends 44.1 kHz and outputs run at 48. Someone has to convert, and it is much
            // better done here: ffmpeg is already decoding this stream sample by sample as it
            // arrives, so the conversion rides along with it. Handing 44.1 kHz to the server instead
            // puts a resampler in the middle of a live stream that has no clock of its own, and that
            // is audible -- a tone that is clean at 48 kHz stutters at 44.1.
            let sample_rate = config.sample_rate;
            let channels = if transport_state.channels > 0 {
                transport_state.channels
            } else {
                2
            };
            let started = match start_source(config, sample_rate, channels) {
                Ok(started) => started,
                Err(err) => {
                    warn!("bluetooth: no audio path for this stream: {err:#}");
                    return;
                }
            };
            let frames = started.decoder.frames.clone();
            let mut started = started;
            started.quieted = quiet_the_radio(connection).await;
            *streaming = Some(started);

            let counters = std::sync::Arc::clone(counters);
            // Its own thread: this is a blocking socket read that runs for as long as the music
            // does, and it has no business on the runtime that answers D-Bus.
            std::thread::spawn(move || {
                if let Err(err) = transport::read_stream(fd, read_mtu, counters, Some(frames)) {
                    warn!("bluetooth: the stream ended: {err:#}");
                }
            });
        }
        // Not yet playing is the normal case between "connected" and "pressed play".
        Err(err) => debug!("bluetooth: the stream is not ready: {err:#}"),
    }
}

/// Start the decoder and the source that carries its audio to the server.
fn start_source(config: &BluetoothConfig, sample_rate: u32, channels: u8) -> Result<Streaming> {
    let (decoder, pcm) = decode::spawn("sbc", sample_rate, channels)?;
    let server_url = config
        .server_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no sendspin server to send this to"))?;

    let mut source_config = sendspin::source::SourceConfig::new(
        config.client_id.clone(),
        format!("{} Bluetooth", config.name),
    );
    // PCM, because that is what the protocol names -- see the note in `decode`.
    source_config.codec = "pcm".to_string();
    source_config.sample_rate = sample_rate;
    source_config.channels = channels;
    source_config.bit_depth = 16;
    // A phone's audio is present exactly while it is streaming, which the transport already says;
    // there is no silence to sense.
    source_config.line_sense = false;

    let source = sendspin::source::Source::with_frames(source_config, pcm);
    let handle = tokio::spawn(async move {
        if let Err(err) = source
            .run_outbound(&server_url, Some(Duration::from_secs(2)))
            .await
        {
            warn!("bluetooth: the audio source stopped: {err}");
        }
    });

    Ok(Streaming {
        decoder,
        source: handle,
        quieted: Vec::new(),
    })
}

/// What the adapter and the phones around it are doing.
async fn inspect(
    connection: &Connection,
    adapter: &AdapterProxy<'_>,
    config: &BluetoothConfig,
    endpoint: &A2dpEndpoint,
    asked: &mut std::collections::HashMap<String, std::time::Instant>,
) -> BluetoothStatus {
    let mut report = BluetoothStatus {
        enabled: true,
        zone_id: Some(config.zone_id),
        name: Some(config.name.clone()),
        discoverable: adapter.discoverable().await.unwrap_or(false),
        ..Default::default()
    };

    let streaming = endpoint.streaming_address().await;
    match managed_objects(connection).await {
        Ok(objects) => {
            for (path, interfaces) in objects {
                let Some(properties) = interface(&interfaces, "org.bluez.Device1") else {
                    continue;
                };
                if !bool_property(properties, "Paired").unwrap_or(false) {
                    continue;
                }
                // Only things that can play *to* us. Everything paired to this adapter shows up
                // here otherwise -- including the Beoremote One, listed as a phone, with a button
                // beside it that would unpair the room's remote.
                if !offers_audio(properties) {
                    continue;
                }
                let connected = bool_property(properties, "Connected").unwrap_or(false);
                // Nothing is connected, so nothing can play: call the phone.
                //
                // This is the half that was missing, and the half that made it feel unreliable. A
                // phone's own list only offers a device it is already connected to, so a room whose
                // speaker never calls can only be reached by pairing again -- which works, and which
                // is exactly what "it is not stable" means. A B&O box calls its last phone; so does
                // this. It is a paired, trusted device and the call costs nothing when it is out of
                // range or busy.
                if !connected && due(asked, &path) {
                    asked.insert(path.as_str().to_string(), std::time::Instant::now());
                    call(connection, &path).await;
                }
                // A phone that is here but not sending is one whose audio session went with a
                // restart of this process: the link survives, the A2DP session does not, and iOS
                // does not rebuild it by itself. So it is asked -- but rarely.
                //
                // Rarely, because a phone that simply is not playing anything answers this happily
                // and goes on not playing anything, and because iOS drops the profile again straight
                // afterwards, which looks exactly like a phone that needs asking. Asking every poll
                // turned into a speaker tugging at someone's sleeve three times a second.
                if connected && streaming.is_none() && due(asked, &path) {
                    asked.insert(path.as_str().to_string(), std::time::Instant::now());
                    reconnect_audio(connection, &path).await;
                }
                // A paired phone is trusted, which is what makes it reconnect on its own. Without
                // it every reconnection needs authorising again, and a phone that asks and does not
                // get an answer in time simply drops back to its own speaker -- with nothing on
                // either screen to say why.
                if !bool_property(properties, "Trusted").unwrap_or(false) {
                    trust(connection, &path).await;
                }
                let address = string_property(properties, "Address").unwrap_or_default();
                report.devices.push(PairedPhone {
                    name: string_property(properties, "Alias")
                        .or_else(|| string_property(properties, "Name"))
                        .unwrap_or_else(|| address.clone()),
                    connected: bool_property(properties, "Connected").unwrap_or(false),
                    streaming: streaming.as_deref() == Some(address.as_str()),
                    address,
                });
            }
        }
        Err(err) => report.last_error = Some(format!("{err:#}")),
    }
    // Connected first, then by name, so the phone someone is holding is at the top.
    report.devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| a.name.cmp(&b.name))
    });
    report.now_playing = metadata::now_playing(connection).await;
    report
}

/// How long to leave a phone alone between calls.
const RECONNECT_COOLDOWN: Duration = Duration::from_secs(60);

/// Whether this phone may be reached for again.
fn due(
    asked: &std::collections::HashMap<String, std::time::Instant>,
    path: &OwnedObjectPath,
) -> bool {
    asked
        .get(path.as_str())
        .is_none_or(|last| last.elapsed() >= RECONNECT_COOLDOWN)
}

/// Put everything that is not the music off the air for as long as the music lasts.
///
/// One radio, two jobs. A Beoremote One holds a connection open the whole time it is paired, and
/// every one of its connection events is a slot the phone's audio does not get: measured here, 83
/// to 94 percent of the audio arriving while it was connected against 99 to 100 with it gone, and
/// the room hearing every bit of that difference. Relaxing the connection's timing was tried and
/// changed nothing -- the cost is in the link itself, and the remote negotiates its own terms.
///
/// So the remote steps aside while a phone plays. It is a real loss: the remote is deaf until it
/// comes back, and the key press that wakes it is the one that gets lost. It is offered here anyway
/// because the alternative is music that stutters, and because the remote picks itself up -- and is
/// called back the moment the music stops.
async fn quiet_the_radio(connection: &Connection) -> Vec<OwnedObjectPath> {
    let mut quieted = Vec::new();
    let Ok(objects) = managed_objects(connection).await else {
        return quieted;
    };
    for (path, interfaces) in objects {
        let Some(properties) = interface(&interfaces, "org.bluez.Device1") else {
            continue;
        };
        if !bool_property(properties, "Connected").unwrap_or(false) {
            continue;
        }
        // Everything that is not something to listen to. A second phone stays: it is not costing
        // airtime unless it plays, and taking someone else's music away is not this module's call.
        if offers_audio(properties) {
            continue;
        }
        let name = string_property(properties, "Alias")
            .or_else(|| string_property(properties, "Name"))
            .unwrap_or_else(|| path.as_str().to_string());
        match DeviceProxy::builder(connection).path(path.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(device) => match device.disconnect().await {
                    Ok(()) => {
                        info!("bluetooth: {name} stands aside while the phone plays");
                        quieted.push(path);
                    }
                    Err(err) => warn!("bluetooth: {name} would not stand aside: {err}"),
                },
                Err(err) => warn!("bluetooth: cannot reach {name}: {err}"),
            },
            Err(err) => warn!("bluetooth: cannot reach {name}: {err}"),
        }
    }
    quieted
}

/// Call back whatever stood aside.
async fn wake_the_radio(connection: &Connection, quieted: &[OwnedObjectPath]) {
    for path in quieted {
        let Ok(builder) = DeviceProxy::builder(connection).path(path.clone()) else {
            continue;
        };
        let Ok(device) = builder.build().await else {
            continue;
        };
        match device.connect().await {
            Ok(()) => info!("bluetooth: {} is back", path.as_str()),
            // A remote that is asleep answers no call; it comes back on its own when a key is
            // pressed, which is the whole reason this is acceptable.
            Err(err) => debug!("bluetooth: {} did not answer: {err}", path.as_str()),
        }
    }
}

/// Call a paired phone that is not connected.
async fn call(connection: &Connection, path: &OwnedObjectPath) {
    let Ok(builder) = DeviceProxy::builder(connection).path(path.clone()) else {
        return;
    };
    let Ok(device) = builder.build().await else {
        return;
    };
    match device.connect().await {
        Ok(()) => info!("bluetooth: connected to {}", path.as_str()),
        // Out of range, switched off, or busy with someone else: all normal, none worth shouting
        // about every minute.
        Err(err) => debug!("bluetooth: {} did not answer: {err}", path.as_str()),
    }
}

/// Ask a connected phone for its audio again.
async fn reconnect_audio(connection: &Connection, path: &OwnedObjectPath) {
    let Ok(builder) = DeviceProxy::builder(connection).path(path.clone()) else {
        return;
    };
    let Ok(device) = builder.build().await else {
        return;
    };
    // The profile, not the device: the link is already up -- it is the audio session that went with
    // the restart, and `Connect` on a connected device only answers "already connected".
    match device.connect_profile(A2DP_SOURCE_UUID).await {
        Ok(()) => info!("bluetooth: asked {} for its audio again", path.as_str()),
        Err(err) => {
            debug!(
                "bluetooth: {} would not reconnect audio: {err}",
                path.as_str()
            );
            // Some phones will not answer a profile request but will take the whole device.
            if let Err(err) = device.connect().await {
                debug!("bluetooth: {} would not connect: {err}", path.as_str());
            }
        }
    }
}

/// Trust a phone, so it can come back without asking again.
async fn trust(connection: &Connection, path: &OwnedObjectPath) {
    let device = match DeviceProxy::builder(connection).path(path.clone()) {
        Ok(builder) => builder.build().await,
        Err(err) => {
            debug!("bluetooth: {err}");
            return;
        }
    };
    match device {
        Ok(device) => match device.set_trusted(true).await {
            Ok(()) => info!("bluetooth: {} may reconnect on its own", path.as_str()),
            Err(err) => warn!("bluetooth: could not trust {}: {err}", path.as_str()),
        },
        Err(err) => debug!("bluetooth: {err}"),
    }
}

async fn forget(connection: &Connection, adapter: &AdapterProxy<'_>, address: &str) {
    let Ok(objects) = managed_objects(connection).await else {
        return;
    };
    for (path, interfaces) in objects {
        let Some(properties) = interface(&interfaces, "org.bluez.Device1") else {
            continue;
        };
        if string_property(properties, "Address").as_deref() != Some(address) {
            continue;
        }
        match adapter.remove_device(&path.as_ref()).await {
            Ok(()) => info!("bluetooth: forgot {address}"),
            Err(err) => warn!("bluetooth: could not forget {address}: {err}"),
        }
        return;
    }
    debug!("bluetooth: {address} was not paired here");
}

/// An agent that unregisters itself when this stops.
struct AgentGuard {
    connection: Connection,
}

impl AgentGuard {
    async fn register(connection: &Connection, pin: Option<String>) -> Result<Self> {
        let path = ObjectPath::try_from(AGENT_PATH).expect("a literal path");
        // With a passkey to show, say so: bluez then picks a pairing method that uses it. Without
        // one, this device has no way to show or type anything, which is what NoInputNoOutput means
        // and what makes a phone pair with a single tap.
        let capability = if pin.is_some() {
            "DisplayOnly"
        } else {
            "NoInputNoOutput"
        };
        connection
            .object_server()
            .at(&path, Agent { pin })
            .await
            .context("serve the bluetooth agent")?;
        let manager = AgentManagerProxy::new(connection)
            .await
            .context("talk to bluez")?;
        manager
            .register_agent(&path, capability)
            .await
            .context("register the bluetooth agent")?;
        if let Err(err) = manager.request_default_agent(&path).await {
            debug!("bluetooth: another agent is the default one: {err}");
        }
        info!("bluetooth: pairing agent registered as {capability}");
        Ok(Self {
            connection: connection.clone(),
        })
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        tokio::spawn(async move {
            let path = ObjectPath::try_from(AGENT_PATH).expect("a literal path");
            if let Ok(manager) = AgentManagerProxy::new(&connection).await {
                let _ = manager.unregister_agent(&path).await;
            }
            let _ = connection.object_server().remove::<Agent, _>(&path).await;
        });
    }
}

pub(crate) type Properties = HashMap<String, OwnedValue>;
pub(crate) type ManagedObjects = HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, Properties>>;

pub(crate) fn interface<'a>(
    interfaces: &'a HashMap<OwnedInterfaceName, Properties>,
    name: &str,
) -> Option<&'a Properties> {
    interfaces
        .iter()
        .find(|(interface, _)| interface.as_str() == name)
        .map(|(_, properties)| properties)
}

pub(crate) async fn managed_objects(connection: &Connection) -> Result<ManagedObjects> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)?
        .path("/")?
        .build()
        .await
        .context("talk to bluez")?;
    Ok(manager.get_managed_objects().await?)
}

pub(crate) fn string_property(properties: &Properties, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|value| !value.is_empty())
}

pub(crate) fn bool_property(properties: &Properties, key: &str) -> Option<bool> {
    properties
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_audio_profiles_are_allowed_through() {
        assert!(is_audio_service("0000110a-0000-1000-8000-00805f9b34fb"));
        assert!(is_audio_service("0000110D-0000-1000-8000-00805F9B34FB"));
        assert!(is_audio_service("0000110e-0000-1000-8000-00805f9b34fb"));
        // Handsfree would take the phone into call mode and replace the music with a telephone.
        assert!(!is_audio_service("0000111e-0000-1000-8000-00805f9b34fb"));
        // And nothing else has any business here.
        assert!(!is_audio_service("00001105-0000-1000-8000-00805f9b34fb"));
        assert!(!is_audio_service("00001812-0000-1000-8000-00805f9b34fb"));
    }

    #[test]
    fn only_things_that_can_play_to_us_count_as_phones() {
        let uuids = |list: &[&str]| {
            let values: Vec<String> = list.iter().map(|uuid| (*uuid).to_string()).collect();
            Properties::from([(
                "UUIDs".to_string(),
                OwnedValue::try_from(zbus::zvariant::Value::from(values)).expect("uuids"),
            )])
        };

        // A phone: it has audio to send.
        assert!(offers_audio(&uuids(&[
            "0000110a-0000-1000-8000-00805f9b34fb",
            "0000110e-0000-1000-8000-00805f9b34fb",
        ])));
        // A Beoremote One: a keyboard and a battery. Listing it as a phone puts a "forget" button
        // next to the room's remote.
        assert!(!offers_audio(&uuids(&[
            "00001812-0000-1000-8000-00805f9b34fb",
            "0000180f-0000-1000-8000-00805f9b34fb",
        ])));
        // A speaker offers a Sink, not a Source: it cannot play to us either.
        assert!(!offers_audio(&uuids(&[
            "0000110b-0000-1000-8000-00805f9b34fb"
        ])));
        assert!(!offers_audio(&Properties::new()));
    }

    #[test]
    fn a_zone_that_did_not_ask_for_bluetooth_gets_none() {
        let off = DesiredBluetooth {
            enabled: Some(false),
            zone_id: Some(3),
            ..Default::default()
        };
        assert!(BluetoothConfig::from_desired(&off, None, "dev").is_none());

        // Enabled but nameless: a zone id is what makes it mean anything.
        let no_zone = DesiredBluetooth {
            enabled: Some(true),
            ..Default::default()
        };
        assert!(BluetoothConfig::from_desired(&no_zone, None, "dev").is_none());
    }

    #[test]
    fn the_name_a_phone_sees_comes_from_the_server() {
        let desired = DesiredBluetooth {
            enabled: Some(true),
            zone_id: Some(12),
            name: Some("Keuken".to_string()),
            discoverable_seconds: Some(45),
            pin: Some("  ".to_string()),
            control: None,
            client_id: None,
            sample_rate: None,
        };
        let config = BluetoothConfig::from_desired(&desired, None, "dev").expect("a config");
        assert_eq!(config.name, "Keuken");
        assert_eq!(config.discoverable, Duration::from_secs(45));
        // A pin of spaces is not a pin; it would otherwise turn every pairing into a passkey dance.
        assert_eq!(config.pin, None);
        // Control is on unless it is switched off.
        assert!(config.control);
    }

    #[test]
    fn a_new_pin_needs_the_radio_set_up_again_but_a_rename_does_not() {
        let base = DesiredBluetooth {
            enabled: Some(true),
            zone_id: Some(12),
            name: Some("Keuken".to_string()),
            ..Default::default()
        };
        let renamed = DesiredBluetooth {
            name: Some("Woonkamer".to_string()),
            ..base.clone()
        };
        let with_pin = DesiredBluetooth {
            pin: Some("1234".to_string()),
            ..base.clone()
        };
        let key = |desired: &DesiredBluetooth| {
            BluetoothConfig::from_desired(desired, None, "dev")
                .expect("a config")
                .restart_key()
        };
        assert_eq!(key(&base), key(&renamed));
        assert_ne!(key(&base), key(&with_pin));
    }
}
