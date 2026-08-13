//! Pairing a Beoremote One, without a terminal.
//!
//! Pairing normally means SSH-ing in and typing four `bluetoothctl` commands, which is exactly the
//! kind of thing this client exists to remove. The server queues a `pair_remote` command, a window
//! opens here, and the next remote put into pairing mode is paired and trusted.
//!
//! This talks to BlueZ over D-Bus rather than driving `bluetoothctl`, and that is the whole point
//! rather than a matter of taste. bluetoothd will not pair without an *agent* registered to answer
//! its questions, and the agent has to stay alive for the entire window: on a Beoremote One the
//! daemon calls `RequestAuthorization` on it a few hundred milliseconds into the pairing. A
//! `bluetoothctl` child process brings an agent that dies with the process, so every one-shot
//! `bluetoothctl pair` produced the same trace on the hardware --
//!
//! ```text
//! new_auth() Requesting agent authentication for 48:D0:CF:9D:36:7D
//! btd_adapter_confirm_reply() success 0
//! pair_device_complete() Authentication Failed (0x05)
//! ```
//!
//! -- all three in the same millisecond, because there was nobody left to ask. With the agent below
//! held open the same remote pairs in four seconds.
//!
//! `trust` is the step that is easy to forget and annoying to debug: without it every reconnect needs
//! re-authorising, so a remote works once and then appears dead.

use crate::models::{PairedRemote, PairingStatusReport};
use crate::status::Registry;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};
use zbus::names::OwnedInterfaceName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{interface, proxy, Connection};

/// How long to leave the window open. Long enough to walk to the remote and hold its buttons.
const DEFAULT_WINDOW: Duration = Duration::from_secs(90);
/// B&O remotes advertise with this name prefix, which is also what the daemon's legacy-GATT check
/// looks for.
const REMOTE_NAME_PREFIX: &str = "BEORC";
/// The Beoremote Essence advertises under the name of the product it shipped with, so it needs
/// naming separately. It is a remote all the same: BLE HID, appearance 0x03c0, five buttons and a
/// wheel. B&O's own BlueZ patches single it out by this exact string.
const ESSENCE_NAME: &str = "BeoSound Essence";
/// How the two models are spelled where the server and the admin screen read them.
pub const KIND_ONE: &str = "one";
pub const KIND_ESSENCE: &str = "essence";
/// Where our agent lives on the bus. Anything unclaimed will do; this says who it belongs to.
const AGENT_PATH: &str = "/sonn/agent";
/// What this box claims it can do during pairing.
///
/// `KeyboardDisplay`, not the honest `NoInputNoOutput`, and the difference decides whether a
/// Beoremote Essence can pair at all. Both ends of that pairing ask for MITM protection, and MITM
/// cannot be satisfied by just-works — the only method two `NoInputNoOutput` peers can agree on. So
/// the negotiation fails: measured, every attempt died about 150 ms after our authorisation with
/// SMP status 0x05, "authentication requirements".
///
/// Claiming a keyboard and a display we do not have is safe because the methods it unlocks are
/// answered below in software: a numeric comparison is confirmed (nobody can read the number off a
/// remote with no screen), and a passkey we would have to type is offered as zero rather than
/// pretended. A Beoremote One pairs just-works either way.
const AGENT_CAPABILITY: &str = "KeyboardDisplay";
const BLUEZ: &str = "org.bluez";
/// How long to wait for the adapter to answer a pairing request. Pairing itself takes seconds; this
/// is the point at which the remote clearly is not answering.
const PAIR_TIMEOUT: Duration = Duration::from_secs(40);
/// How long to wait for the first connection. Short: it is a courtesy, not the pairing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for the remote to advertise again after its old bond was cleared. It is in
/// pairing mode and advertising every fraction of a second; this only has to outlast bluez taking
/// the object away and building it back.
const REDISCOVER_TIMEOUT: Duration = Duration::from_secs(20);
/// How many times to ask for a pairing before giving up. See `pair_with_retries`.
const PAIR_ATTEMPTS: u32 = 3;
/// How long to wait between those attempts, so the record can be filled in from an advertisement.
const RETRY_PAUSE: Duration = Duration::from_secs(3);
/// How long to hold the connection open after pairing, at most.
///
/// bluez ties a connection to the D-Bus client that asked for it. When that is this process running
/// `pair-remote` from a terminal, exiting hangs up on the remote while it is still discovering our
/// service -- which the remote itself then reports as a failed pairing, with nothing on this side to
/// show for it. A Beoremote One walks all forty-odd characteristics and takes twenty seconds and
/// more over it, so this waits for the remote to let go rather than counting seconds at it. The
/// daemon keeps its connection anyway; this only matters for the one-shot command.
const HOLD_AFTER_PAIRING: Duration = Duration::from_secs(120);
/// How long to stay put before a disconnect counts as "the remote is finished". Its discovery of
/// forty-odd characteristics takes twenty seconds and more, and the link dips once at the start.
const HOLD_MINIMUM: Duration = Duration::from_secs(45);
/// How often to look at what discovery has turned up. A remote advertises in short bursts, so this
/// is deliberately quicker than a human notices.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

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

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter {
    fn set_discovery_filter(
        &self,
        filter: HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device {
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    fn pair(&self) -> zbus::Result<()>;
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_trusted(&self, trusted: bool) -> zbus::Result<()>;
}

/// The agent bluetoothd asks when it needs a human.
///
/// Every method says yes. The device being paired is the one the user just put into pairing mode
/// while holding a window open from the server, so there is nothing left to confirm -- and a
/// Beoremote One has no display to compare a passkey on anyway.
struct Agent;

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".to_string()
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, _pin_code: String) {}

    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }

    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    fn request_confirmation(&self, device: OwnedObjectPath, _passkey: u32) {
        debug!("agent: confirming {}", device.as_str());
    }

    /// The one that matters for a Beoremote One: this is what the daemon asks for, and what nobody
    /// was there to answer before.
    fn request_authorization(&self, device: OwnedObjectPath) {
        debug!("agent: authorising {}", device.as_str());
    }

    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}

    fn cancel(&self) {
        debug!("agent: the daemon cancelled the request");
    }
}

/// Whether a pairing window is open. See [`pair_remote`].
static PAIRING: AtomicBool = AtomicBool::new(false);

/// Clears [`PAIRING`] however the window ends, including a panic.
struct WindowGuard;

impl Drop for WindowGuard {
    fn drop(&mut self) {
        PAIRING.store(false, Ordering::SeqCst);
    }
}

/// Open a pairing window and report what happened.
///
/// `address` pairs one specific device; without it, the first `BEORC*` device that turns up wins.
///
/// One window at a time, and a second ask is dropped rather than queued. There is one radio, and a
/// window that starts while another is running does real damage: it forgets the bond the first one
/// has just made and then races it for the same device, which is how a remote ends up unpaired
/// after someone pressed the button twice. The running window keeps the status it is reporting --
/// overwriting it with "scanning" for a window that never opened would be a lie the screen shows.
pub async fn pair_remote(
    statuses: &Registry,
    address: Option<String>,
    window: Option<Duration>,
) -> Result<()> {
    if PAIRING.swap(true, Ordering::SeqCst) {
        info!("a pairing window is already open; ignoring this one");
        return Ok(());
    }
    let _guard = WindowGuard;
    let window = window.unwrap_or(DEFAULT_WINDOW);
    statuses.set_pairing(Some(PairingStatusReport {
        state: "scanning".to_string(),
        address: address.clone(),
        name: None,
        message: None,
    }));
    info!("pairing window open for {}s", window.as_secs());

    // No outer timeout around this: the window is the scan's own deadline, and pairing gets as long
    // as it needs once a remote has actually answered. Cutting a pairing off halfway is how this
    // used to report nothing at all.
    let report = match run_pairing(address.clone(), window).await {
        Ok(Some(paired)) => {
            info!("paired {} ({:?})", paired.address, paired.name);
            PairingStatusReport {
                state: "paired".to_string(),
                address: Some(paired.address),
                name: paired.name,
                message: None,
            }
        }
        // Nothing ever advertised. That is not a failed pairing -- nobody pressed anything.
        Ok(None) => {
            warn!("pairing window closed with nothing paired");
            PairingStatusReport {
                state: "timeout".to_string(),
                address: address.clone(),
                name: None,
                message: Some(format!(
                    "{} advertised in {}s -- put the remote into pairing mode (LIST > SETTINGS > \
                     PAIRING) and try again while its screen says it is open for pairing",
                    match &address {
                        Some(address) => format!("{address} never"),
                        None => "no remote".to_string(),
                    },
                    window.as_secs()
                )),
            }
        }
        Err(err) => {
            warn!("pairing failed: {:#}", err);
            PairingStatusReport {
                state: "failed".to_string(),
                address,
                name: None,
                message: Some(format!("{err:#}")),
            }
        }
    };
    statuses.set_pairing(Some(report));
    Ok(())
}

struct PairedDevice {
    address: String,
    name: Option<String>,
}

/// What discovery turned up, and where BlueZ keeps it.
struct Candidate {
    path: OwnedObjectPath,
    address: String,
    name: Option<String>,
}

/// Returns the remote that was paired, or `None` if the window closed with nothing on the air.
async fn run_pairing(address: Option<String>, window: Duration) -> Result<Option<PairedDevice>> {
    let connection = Connection::system()
        .await
        .context("connect to the system bus (is dbus running?)")?;

    // Serve the agent before anything else touches the adapter, and keep it served until this
    // function returns. Registered-and-then-gone is the same as never registered.
    let _agent = AgentGuard::register(&connection).await?;

    let adapter_path = first_adapter(&connection).await?;
    let adapter = AdapterProxy::builder(&connection)
        .path(adapter_path)?
        .build()
        .await
        .context("talk to the adapter")?;

    // LE only. Without the filter bluez discovers dual-mode, files a remote that is LE-only as a
    // BR/EDR device, and then pairs by *paging* it -- which it never answers: "Page Timeout".
    if let Err(err) = adapter
        .set_discovery_filter(HashMap::from([("Transport", "le".into())]))
        .await
    {
        debug!("could not ask for an LE-only scan: {err}");
    }
    adapter.start_discovery().await.context("start scanning")?;
    info!("scanning up to {}s for a remote", window.as_secs());

    let found = match timeout(window, watch_for_remote(&connection, address.as_deref())).await {
        Ok(found) => found?,
        Err(_) => {
            let _ = adapter.stop_discovery().await;
            return Ok(None);
        }
    };
    info!(
        "found {} ({})",
        found.address,
        found.name.as_deref().unwrap_or("no name")
    );

    // Whatever this adapter thinks it knows about *this* remote goes first -- but only a real bond,
    // and only this one's.
    //
    // A bond has two halves and only one of them is here. Clearing it on the remote -- which is what
    // someone does when pairing stopped working -- leaves this side convinced the two are still
    // paired, so pairing opens a link the remote refuses. Pressing pair means "start over" for the
    // remote in your hand.
    //
    // It emphatically does not mean "start over" for the others. Dropping every bond when a window
    // opens is what this did once, and pairing a second remote then silently unpaired the first: a
    // room can have a Beoremote One on the table and an Essence in the kitchen, and neither is a
    // draft of the other.
    //
    // A device that is merely *listed* is not a bond, it is the discovery cache, and it is worth
    // keeping: the name only appears in a scan response, so between advertising bursts that cache
    // entry is the only place the name is written down. Removing it cost an entire window once.
    let forgotten = forget_existing_bonds(&connection, &adapter, Some(&found.address)).await;

    // Removing a bond takes the device object with it, so the path just discovered no longer
    // exists. Pairing on it fails with "Method Pair doesn't exist" -- three times, because the
    // retries use the same dead path -- and the remote is left unpaired: the bond was dropped and
    // nothing replaced it. So wait for bluez to rebuild the object from the next advertisement,
    // which a remote in pairing mode sends constantly, and keep scanning until it does.
    let found = if forgotten {
        match timeout(
            REDISCOVER_TIMEOUT,
            watch_for_remote(&connection, Some(&found.address)),
        )
        .await
        {
            Ok(found) => found?,
            Err(_) => {
                let _ = adapter.stop_discovery().await;
                anyhow::bail!(
                    "{} did not advertise again after its old pairing was cleared; put it back \
                     into pairing mode and try once more",
                    found.address
                );
            }
        }
    } else {
        found
    };

    // Stop scanning before connecting. An adapter that is still sweeping channels is a slower and
    // less reliable one to establish a link with, and BlueZ would suspend the discovery anyway.
    if let Err(err) = adapter.stop_discovery().await {
        debug!("could not stop discovery: {err}");
    }

    let device = DeviceProxy::builder(&connection)
        .path(found.path.clone())?
        .build()
        .await
        .context("talk to the remote")?;

    info!("pairing {}", found.address);
    pair_with_retries(&device, &found.address).await?;

    // Without this a remote works exactly once: every reconnect wants authorising again, and
    // nothing on the remote says so -- it simply stops responding after a while.
    if let Err(err) = device.set_trusted(true).await {
        warn!("{} was paired but not trusted: {err}", found.address);
    }

    // Connecting is a courtesy: the remote reconnects by itself once it is trusted, and a failure
    // here is not a failed pairing.
    match timeout(CONNECT_TIMEOUT, device.connect()).await {
        Ok(Ok(())) => info!("{} connected", found.address),
        Ok(Err(err)) => info!(
            "{} is paired; it will connect when it is used ({err})",
            found.address
        ),
        Err(_) => info!(
            "{} is paired; it will connect when it is used",
            found.address
        ),
    }

    hold_until_the_remote_is_done(&device, &found.address).await;

    Ok(Some(PairedDevice {
        address: found.address,
        name: found.name,
    }))
}

/// Stay out of the way until the remote has finished with us.
async fn hold_until_the_remote_is_done(device: &DeviceProxy<'_>, address: &str) {
    info!("{address} is paired; holding the connection while it reads our service");
    let start = tokio::time::Instant::now();
    let deadline = start + HOLD_AFTER_PAIRING;
    while tokio::time::Instant::now() < deadline {
        sleep(POLL_INTERVAL).await;
        // `Connected` dips to false for a moment right after pairing, while the link is brought up
        // again encrypted. Leaving on that first dip hangs up on a remote that has only just started
        // reading -- which is exactly what it reports as a failed pairing.
        if start.elapsed() < HOLD_MINIMUM {
            continue;
        }
        match device.connected().await {
            Ok(false) => {
                debug!(
                    "{address} disconnected on its own after {:?}; done",
                    start.elapsed()
                );
                return;
            }
            Ok(true) => {}
            Err(err) => {
                debug!("could not read {address}'s connection state: {err}");
                return;
            }
        }
    }
}

/// Pair, allowing for the first attempt on a freshly seen device to fail.
///
/// A device record bluez has only just built carries no LE detail yet, and the first `Pair()` on one
/// reaches for BR/EDR and times out paging. The advertisement that follows fills the record in, and
/// the next attempt goes over LE. Measured repeatedly on the hardware, on two different daemons --
/// so it is retried here rather than reported as a failure the user can do nothing with.
async fn pair_with_retries(device: &DeviceProxy<'_>, address: &str) -> Result<()> {
    let mut last = None;
    for attempt in 1..=PAIR_ATTEMPTS {
        match timeout(PAIR_TIMEOUT, device.pair()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                debug!("pairing {address} attempt {attempt} failed: {err}");
                last = Some(anyhow!("the adapter refused to pair with {address}: {err}"));
            }
            Err(_) => {
                last = Some(anyhow!(
                    "{address} did not answer within {}s -- it may have left pairing mode",
                    PAIR_TIMEOUT.as_secs()
                ));
            }
        }
        if attempt < PAIR_ATTEMPTS {
            sleep(RETRY_PAUSE).await;
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("pairing {address} failed")))
}

/// Keep polling what BlueZ has discovered until a remote is on the air.
async fn watch_for_remote(connection: &Connection, target: Option<&str>) -> Result<Candidate> {
    loop {
        for (path, interfaces) in managed_objects(connection).await?.into_iter() {
            let Some(properties) = interface(&interfaces, "org.bluez.Device1") else {
                continue;
            };
            if let Some(candidate) = candidate_from(&path, properties, target) {
                return Ok(candidate);
            }
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// The remotes this device has paired, whether or not one is in the room right now.
///
/// Same rule as pairing uses to recognise one, and for the same reason: the name. An HID service
/// looks like the obvious filter and is not one -- a BeoSound Essence advertises it too.
pub async fn paired_remotes(connection: &Connection) -> Vec<PairedRemote> {
    let Ok(objects) = managed_objects(connection).await else {
        return Vec::new();
    };
    let mut remotes: Vec<PairedRemote> = objects
        .into_values()
        .filter_map(|interfaces| {
            let properties = interfaces.get("org.bluez.Device1")?;
            if !bool_property(properties, "Paired").unwrap_or(false) {
                return None;
            }
            let name = string_property(properties, "Alias")
                .or_else(|| string_property(properties, "Name"))
                .unwrap_or_default();
            let kind = remote_kind(&name)?;
            Some(PairedRemote {
                address: string_property(properties, "Address")?,
                name,
                connected: bool_property(properties, "Connected").unwrap_or(false),
                kind: kind.to_string(),
            })
        })
        .collect();
    // Connected first, then by address, so the one in someone's hand is at the top and the order
    // does not shuffle between polls.
    remotes.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| a.address.cmp(&b.address))
    });
    remotes
}

/// Ask a paired remote to come back.
///
/// Costs nothing when it is asleep or out of range -- bluez answers "not available" and the next
/// tick tries again -- and it is the only way a remote that wakes with undirected advertisements
/// reaches a stock daemon at all.
pub async fn call_remote(connection: &Connection, address: &str) {
    let path = format!("/org/bluez/hci0/dev_{}", address.replace(':', "_"));
    let Ok(builder) = DeviceProxy::builder(connection).path(path) else {
        return;
    };
    let Ok(device) = builder.build().await else {
        return;
    };
    match device.connect().await {
        Ok(()) => info!("remote {address} is back"),
        Err(err) => debug!("remote {address} did not answer: {err}"),
    }
}

/// Forget one remote, so a replacement can take its place.
pub async fn forget_remote(address: &str) -> Result<()> {
    let connection = Connection::system()
        .await
        .context("talk to bluez (is bluetoothd running?)")?;
    let adapter_path = first_adapter(&connection).await?;
    let adapter = AdapterProxy::builder(&connection)
        .path(adapter_path)?
        .build()
        .await
        .context("talk to the adapter")?;
    for (path, interfaces) in managed_objects(&connection).await? {
        let Some(properties) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        if string_property(properties, "Address").as_deref() != Some(address) {
            continue;
        }
        adapter
            .remove_device(&path.as_ref())
            .await
            .with_context(|| format!("forget {address}"))?;
        info!("forgot remote {address}");
        return Ok(());
    }
    Err(anyhow!("{address} is not paired to this device"))
}

/// Whether a name belongs to a remote this client knows how to serve.
fn is_remote_name(name: &str) -> bool {
    remote_kind(name).is_some()
}

/// Which model advertises under this name, if it is a remote at all.
pub fn remote_kind(name: &str) -> Option<&'static str> {
    if name.starts_with(REMOTE_NAME_PREFIX) {
        Some(KIND_ONE)
    } else if name == ESSENCE_NAME {
        Some(KIND_ESSENCE)
    } else {
        None
    }
}

/// Decide whether a discovered device is the remote we are waiting for.
///
/// Matching is on the name, or on an address the caller asked for by hand -- and on nothing else.
/// An HID service looks like the obvious filter and is not one: every Bluetooth keyboard in the
/// house advertises the same `00001812`, and using it meant this grabbed a speaker in the living
/// room 14 milliseconds after opening the window.
fn candidate_from(
    path: &OwnedObjectPath,
    properties: &Properties,
    target: Option<&str>,
) -> Option<Candidate> {
    let address = string_property(properties, "Address")?;
    let name = string_property(properties, "Alias").or_else(|| string_property(properties, "Name"));

    let wanted = match target {
        Some(target) => address.eq_ignore_ascii_case(target),
        None => name.as_deref().is_some_and(is_remote_name),
    };
    if !wanted {
        return None;
    }

    // Proof that it is actually there, one way or the other.
    //
    // RSSI means BlueZ heard it just now. Without it, a device entry is usually one BlueZ remembers,
    // and connecting to a remote that is asleep burns the window on a link that cannot be made. But
    // an already-connected device has no RSSI either and is as present as it gets -- an Essence
    // whose battery was reinserted gets picked up by the kernel before anyone opens this window, and
    // then waiting for an advertisement waits forever.
    let present =
        properties.contains_key("RSSI") || bool_property(properties, "Connected").unwrap_or(false);
    if !present {
        return None;
    }

    Some(Candidate {
        path: path.clone(),
        address,
        name,
    })
}

/// Drop any *bond* for the remote so pairing starts from scratch. Cache entries are left alone.
async fn forget_existing_bonds(
    connection: &Connection,
    adapter: &AdapterProxy<'_>,
    target: Option<&str>,
) -> bool {
    let objects = match managed_objects(connection).await {
        Ok(objects) => objects,
        Err(err) => {
            debug!("could not list what the adapter knows: {err:#}");
            return false;
        }
    };
    let mut removed = false;
    for (path, interfaces) in objects {
        let Some(properties) = interface(&interfaces, "org.bluez.Device1") else {
            continue;
        };
        let Some(address) = string_property(properties, "Address") else {
            continue;
        };
        let name = string_property(properties, "Alias")
            .or_else(|| string_property(properties, "Name"))
            .unwrap_or_default();
        let ours = match target {
            Some(target) => address.eq_ignore_ascii_case(target),
            None => is_remote_name(&name),
        };
        if !ours || !bool_property(properties, "Paired").unwrap_or(false) {
            continue;
        }
        match adapter.remove_device(&path.as_ref()).await {
            Ok(()) => {
                info!("forgot the previous pairing for {address}");
                removed = true;
            }
            Err(err) => warn!("could not forget the previous pairing for {address}: {err}"),
        }
    }
    removed
}

/// What `GetManagedObjects` hands back: every object BlueZ exposes, with its interfaces and their
/// properties.
type ManagedObjects = HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, Properties>>;
type Properties = HashMap<String, OwnedValue>;

/// Interface names are their own type on the bus, so a plain `get("org.bluez.Device1")` will not do.
fn interface<'a>(
    interfaces: &'a HashMap<OwnedInterfaceName, Properties>,
    name: &str,
) -> Option<&'a Properties> {
    interfaces
        .iter()
        .find(|(interface, _)| interface.as_str() == name)
        .map(|(_, properties)| properties)
}

async fn managed_objects(connection: &Connection) -> Result<ManagedObjects> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)?
        .path("/")?
        .build()
        .await
        .context("talk to bluez (is bluetoothd running?)")?;
    Ok(manager.get_managed_objects().await?)
}

/// The first adapter BlueZ exposes. Every board this runs on has exactly one.
async fn first_adapter(connection: &Connection) -> Result<OwnedObjectPath> {
    let mut adapters: Vec<OwnedObjectPath> = managed_objects(connection)
        .await?
        .into_iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.bluez.Adapter1"))
        .map(|(path, _)| path)
        .collect();
    adapters.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("this device has no Bluetooth adapter"))
}

/// An agent that unregisters itself when the window closes, however it closes.
struct AgentGuard {
    connection: Connection,
}

impl AgentGuard {
    async fn register(connection: &Connection) -> Result<Self> {
        let path = ObjectPath::try_from(AGENT_PATH).expect("a literal path");
        connection
            .object_server()
            .at(&path, Agent)
            .await
            .context("serve the pairing agent")?;

        let manager = AgentManagerProxy::new(connection)
            .await
            .context("talk to bluez (is bluetoothd running?)")?;
        manager
            .register_agent(&path, AGENT_CAPABILITY)
            .await
            .context("register the pairing agent with bluez")?;
        // Being *the* agent is what makes bluetoothd ask us rather than whoever registered first --
        // a leftover `bluetoothctl` session, say.
        if let Err(err) = manager.request_default_agent(&path).await {
            debug!("another agent is the default one: {err}");
        }
        debug!("pairing agent registered as {AGENT_CAPABILITY}");
        Ok(Self {
            connection: connection.clone(),
        })
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        // Drop is not async, and this is cleanup: let it finish on its own. If the process is on its
        // way out anyway, bluetoothd notices the name disappearing from the bus and drops the agent.
        tokio::spawn(async move {
            let path = ObjectPath::try_from(AGENT_PATH).expect("a literal path");
            if let Ok(manager) = AgentManagerProxy::new(&connection).await {
                let _ = manager.unregister_agent(&path).await;
            }
            let _ = connection.object_server().remove::<Agent, _>(&path).await;
        });
    }
}

fn string_property(properties: &Properties, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|value| !value.is_empty())
}

fn bool_property(properties: &Properties, key: &str) -> Option<bool> {
    properties
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(pairs: &[(&str, OwnedValue)]) -> Properties {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    fn owned(value: impl Into<zbus::zvariant::Value<'static>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).expect("a value")
    }

    fn path() -> OwnedObjectPath {
        OwnedObjectPath::try_from("/org/bluez/hci0/dev_48_D0_CF_9D_36_7D").expect("a path")
    }

    #[test]
    fn a_remote_that_is_advertising_is_the_one() {
        let remote = properties(&[
            ("Address", owned("48:D0:CF:9D:36:7D")),
            ("Alias", owned("BEORC")),
            ("RSSI", owned(-61i16)),
        ]);
        let found = candidate_from(&path(), &remote, None).expect("the remote");
        assert_eq!(found.address, "48:D0:CF:9D:36:7D");
        assert_eq!(found.name.as_deref(), Some("BEORC"));
    }

    #[test]
    fn a_remembered_remote_is_not_one_that_is_here() {
        // Same device, same name, but nothing on the air: BlueZ is quoting its cache. Pairing with
        // it means waiting two minutes for a connection that cannot happen.
        let remembered = properties(&[
            ("Address", owned("48:D0:CF:9D:36:7D")),
            ("Alias", owned("BEORC")),
        ]);
        assert!(candidate_from(&path(), &remembered, None).is_none());
    }

    #[test]
    fn a_connected_remote_counts_as_present() {
        // No RSSI, because BlueZ is not hearing advertisements from something it already has a link
        // to. Waiting for one would wait forever, which is exactly what happened to an Essence the
        // kernel had auto-connected before the window opened.
        let connected = properties(&[
            ("Address", owned("64:CF:D9:1B:AA:FC")),
            ("Alias", owned("BeoSound Essence")),
            ("Connected", owned(true)),
        ]);
        assert!(candidate_from(&path(), &connected, None).is_some());

        // Remembered and silent is still not present.
        let asleep = properties(&[
            ("Address", owned("64:CF:D9:1B:AA:FC")),
            ("Alias", owned("BeoSound Essence")),
            ("Connected", owned(false)),
        ]);
        assert!(candidate_from(&path(), &asleep, None).is_none());
    }

    #[test]
    fn a_beoremote_essence_is_a_remote_too() {
        // It advertises under the name of the product it shipped with, which reads like a speaker
        // and is not one: BLE HID, appearance 0x03c0, a wheel and five buttons. B&O's own BlueZ
        // patches single out this exact string for the same reason.
        let essence = properties(&[
            ("Address", owned("64:CF:D9:1B:AA:FC")),
            ("Alias", owned("BeoSound Essence")),
            ("RSSI", owned(-70i16)),
        ]);
        let found = candidate_from(&path(), &essence, None);
        assert_eq!(found.expect("the essence").address, "64:CF:D9:1B:AA:FC");
    }

    #[test]
    fn other_bluetooth_things_are_not_remotes() {
        // Everything with a keyboard service is not a remote, which is why the service list is not
        // part of the decision -- a speaker in the same room advertises one too.
        let speaker = properties(&[
            ("Address", owned("50:1E:2D:00:6E:24")),
            ("Alias", owned("BeoSound Shape")),
            ("RSSI", owned(-55i16)),
        ]);
        assert!(candidate_from(&path(), &speaker, None).is_none());

        // Unless it is the one that was asked for by address, which is the manual override.
        let found = candidate_from(&path(), &speaker, Some("50:1e:2d:00:6e:24"));
        assert_eq!(
            found.expect("the named device").address,
            "50:1E:2D:00:6E:24"
        );
    }

    #[test]
    fn an_address_that_was_asked_for_must_match() {
        let remote = properties(&[
            ("Address", owned("48:D0:CF:9D:36:7D")),
            ("Alias", owned("BEORC")),
            ("RSSI", owned(-61i16)),
        ]);
        assert!(candidate_from(&path(), &remote, Some("11:22:33:44:55:66")).is_none());
    }

    #[test]
    fn a_device_without_an_address_is_skipped() {
        let nameless = properties(&[("RSSI", owned(-61i16))]);
        assert!(candidate_from(&path(), &nameless, None).is_none());
    }
}
