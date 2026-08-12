//! The BeoRemote One service, served by us over BlueZ's D-Bus GATT API.
//!
//! B&O ship this service inside a patched `bluetoothd`: a GPLv2 daemon that has to own the adapter,
//! be installed as its own component, and be kept out of this binary for licensing. None of that is
//! necessary. Every attribute the remote reads is a plain GATT characteristic on B&O's own UUID base
//! (`0000xxxx-0000-1000-1000-00805f9b34fb` -- note `1000-1000`, which is *not* the Bluetooth base),
//! so a stock BlueZ can serve the same thing through `org.bluez.GattManager1`.
//!
//! Measured on a real Beoremote One against BlueZ 5.82 with no patches at all: after a fresh pairing
//! the remote reads VERSION, FEATURES, TV_SOURCES, MUSIC_SOURCES, SOURCE_CONTENT_1 and ACTIVE_SOURCE
//! and renders the menu. What B&O's patch adds is the *legacy* attribute server their in-daemon
//! plugin needed in BlueZ 5.45; the remote itself is happy with the modern one.
//!
//! Two details that are easy to get wrong and fail quietly:
//!
//! * **Offsets.** The lists are longer than one ATT packet, so the remote reads them again with an
//!   offset. Ignoring `options["offset"]` returns the first bytes over and over, and the menu on the
//!   remote repeats its first entries instead of scrolling.
//! * **Registration order.** Handles are assigned in the order characteristics are registered, and
//!   the remote caches handles per product. B&O keep their order stable for that reason, and so do
//!   we -- it costs nothing and a remote that reconnects finds what it remembers.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::fdo::DBusProxy;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{interface, proxy, Connection};

/// Where our application lives on the bus. BlueZ only needs it to be ours and stable.
const APP_PATH: &str = "/sonn/beoremote";
const SERVICE_PATH: &str = "/sonn/beoremote/service0";
const DIS_PATH: &str = "/sonn/beoremote/service1";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
/// Where the two services are asked to sit. Clear of anything bluez puts in the database itself,
/// and far enough apart that the whole attribute table fits between them.
const SERVICE_HANDLE: u16 = 0x0100;
const DIS_HANDLE: u16 = 0x0200;
/// Characteristics ask for nothing: `0x0000` means "allocate it", which is what bluez requires here.
///
/// Pinning them individually is refused -- a service owns one contiguous block of handles, and
/// spaced-out requests fall outside it ("Failed to create characteristic entry in database"). It is
/// also unnecessary: the block starts at the service's own pinned handle and is filled in the order
/// the characteristics are registered, which is fixed in `REGISTRATION_ORDER`.
const AUTO_HANDLE: u16 = 0x0000;
/// How often to check that bluez is still there.
const BLUEZ_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// The only attribute the remote subscribes to, and so the only one that needs a `Value` property.
const NOTIFYING_ATTRIBUTE: &str = "FEATURES_CHANGED";

/// B&O's UUID base. The service is `0000` on it; every attribute is its own number.
fn uuid_for(number: u8) -> String {
    format!("0000{number:04x}-0000-1000-1000-00805f9b34fb")
}

/// Device Information, carrying the PnP ID that names Bang & Olufsen (`0x0103`) as the vendor.
///
/// The patched daemon publishes this from `DeviceID` in its own `main.conf`; a modern BlueZ knows
/// Device Information only as a client and publishes nothing, so we serve it ourselves. It was
/// present in the run that worked and it is what a remote would look at to decide it is talking to
/// a B&O product, so it stays.
const DIS_UUID: &str = "0000180a-0000-1000-8000-00805f9b34fb";
const PNP_ID_UUID: &str = "00002a50-0000-1000-8000-00805f9b34fb";
const MANUFACTURER_UUID: &str = "00002a29-0000-1000-8000-00805f9b34fb";
const MODEL_UUID: &str = "00002a24-0000-1000-8000-00805f9b34fb";
const FIRMWARE_UUID: &str = "00002a26-0000-1000-8000-00805f9b34fb";
const PNP_ID: [u8; 7] = [0x01, 0x03, 0x01, 0x0B, 0x10, 0x00, 0x00];
const MANUFACTURER: &[u8] = b"Bang & Olufsen";
/// The name B&O's own daemon answers with. All four of these were present in the run the remote
/// accepted; a characteristic it asks for and does not find is an ATT error mid-introduction.
const MODEL: &[u8] = b"StreamSDK";
const FIRMWARE: &[u8] = b"1.0";

/// The attributes, in the order B&O register them.
const REGISTRATION_ORDER: [(&str, &[Flag]); 44] = [
    ("VOLUME", &[Flag::Read, Flag::Write, Flag::Notify]),
    ("HOME_CONTROL_SCENES", &[Flag::Read]),
    ("ACTIVE_HOME_CONTROL_SCENE", &[Flag::Read, Flag::Write]),
    ("CINEMA_MODE", &[Flag::Read, Flag::Write]),
    ("EXPERIENCES", &[Flag::Read]),
    ("ACTIVE_EXPERIENCE", &[Flag::Read, Flag::Write]),
    ("CONTROL_1", &[Flag::Read, Flag::Write]),
    ("CONTROL_2", &[Flag::Read, Flag::Write]),
    ("SOURCE_CONTENT_1", &[Flag::Read]),
    ("SOURCE_CONTENT_2", &[Flag::Read]),
    ("SOURCE_CONTENT_3", &[Flag::Read]),
    ("SOURCE_CONTENT_4", &[Flag::Read]),
    ("SOURCE_CONTENT_5", &[Flag::Read]),
    ("SOURCE_CONTENT_6", &[Flag::Read]),
    ("SOURCE_CONTENT_7", &[Flag::Read]),
    ("SOURCE_CONTENT_8", &[Flag::Read]),
    ("SOURCE_CONTENT_9", &[Flag::Read]),
    ("SOURCE_CONTENT_10", &[Flag::Read]),
    ("ACTIVE_SOURCE_CONTENT", &[Flag::Read, Flag::Write]),
    ("VERSION", &[Flag::Read]),
    ("FEATURES", &[Flag::Read]),
    ("FEATURES_CHANGED", &[Flag::Read, Flag::Indicate]),
    ("INJECT_PRESS", &[Flag::Write]),
    ("INJECT_RELEASE", &[Flag::Write]),
    ("DISC_TRACK", &[Flag::Read, Flag::Write]),
    ("STAND_POSITIONS", &[Flag::Read]),
    ("ACTIVE_STAND_POSITION", &[Flag::Read, Flag::Write]),
    ("SPEAKER_GROUPS", &[Flag::Read]),
    ("ACTIVE_SPEAKER_GROUP", &[Flag::Read, Flag::Write]),
    ("SOUND_MODES", &[Flag::Read]),
    ("ACTIVE_SOUND_MODE", &[Flag::Read, Flag::Write]),
    ("PICTURE_FORMATS", &[Flag::Read]),
    ("ACTIVE_PICTURE_FORMAT", &[Flag::Read, Flag::Write]),
    ("PICTURE_MODES", &[Flag::Read]),
    ("ACTIVE_PICTURE_MODE", &[Flag::Read, Flag::Write]),
    ("PICTURE_MUTE", &[Flag::Read, Flag::Write]),
    ("2D_3D_MODES", &[Flag::Read]),
    ("ACTIVE_2D_3D_MODE", &[Flag::Read, Flag::Write]),
    ("TV_SOURCES", &[Flag::Read]),
    ("MUSIC_SOURCES", &[Flag::Read]),
    ("ACTIVE_SOURCE", &[Flag::Read, Flag::Write]),
    ("CUSTOM_COMMANDS", &[Flag::Read]),
    ("ACTIVE_CUSTOM_COMMAND", &[Flag::Read, Flag::Write]),
    ("MY_BUTTONS", &[Flag::Read]),
];

#[derive(Clone, Copy)]
enum Flag {
    Read,
    Write,
    Notify,
    Indicate,
}

impl Flag {
    /// What bluez has to be told this characteristic accepts.
    ///
    /// A writable one advertises *both* write forms. The remote sends its selections as
    /// `Write Command` -- a write without a response -- and bluez drops one of those on a
    /// characteristic that only declares `write`, without an error anywhere: the press reaches the
    /// daemon and never reaches us, which looks exactly like a menu item that does nothing.
    fn as_strs(self) -> &'static [&'static str] {
        match self {
            Flag::Read => &["read"],
            Flag::Write => &["write", "write-without-response"],
            Flag::Notify => &["notify"],
            Flag::Indicate => &["indicate"],
        }
    }
}

#[proxy(
    interface = "org.bluez.GattManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait GattManager {
    fn register_application(
        &self,
        application: &ObjectPath<'_>,
        options: HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_application(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
}

type Values = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// The connection our application is registered on.
///
/// bluez lets only the client that registered an application take it back: asking from a fresh
/// connection is answered with `DoesNotExist` and changes nothing, which is how a "graceful" exit
/// still left the service occupying its handles.
static OWNER: Mutex<Option<Connection>> = Mutex::new(None);

/// A write the remote made, as an attribute name and its value.
pub type Write = (String, Vec<u8>);

struct Service {
    uuid: String,
    /// Where this service is asked to sit in bluez's database.
    ///
    /// bluez hands out handles from a counter that only ever goes up, so a service that is
    /// registered again after a client restart lands somewhere new -- and a Beoremote One caches
    /// handles and subscribes to nothing, not even Service Changed, so it goes on writing to where
    /// the service used to be. Asking for a fixed handle is what keeps it in one place. bluez may
    /// refuse if the range is taken, in which case it allocates as before and the remote has to be
    /// paired again; there is nothing better available.
    handle: u16,
}

#[interface(name = "org.bluez.GattService1")]
impl Service {
    // Spelled out: zbus would derive `Uuid` from the method name, and BlueZ looks for `UUID` --
    // which fails as "Failed to read UUID property of service", nowhere near the real cause.
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.uuid.clone()
    }

    #[zbus(property, name = "Primary")]
    fn primary(&self) -> bool {
        true
    }

    #[zbus(property, name = "Handle")]
    fn handle(&self) -> u16 {
        self.handle
    }

    #[zbus(property, name = "Handle")]
    fn set_handle(&mut self, handle: u16) {
        // bluez writes back what it actually allocated.
        self.handle = handle;
    }
}

struct Characteristic {
    name: String,
    uuid: String,
    /// Where this characteristic is asked to sit; see `Service::handle`.
    handle: u16,
    service: OwnedObjectPath,
    flags: Vec<String>,
    values: Values,
    writes: mpsc::Sender<Write>,
}

/// Deliberately *without* a `Value` property.
///
/// With one, bluez can answer a read straight from the cached property; without one it has to call
/// `ReadValue`, where the offset is honoured and the answer is cut to the negotiated MTU. The lists
/// here run to a few hundred bytes, and handing a Beoremote One more than it asked for reboots it --
/// which is what a table with `Value` on every characteristic did, repeatedly, on real hardware.
#[interface(name = "org.bluez.GattCharacteristic1")]
impl Characteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.uuid.clone()
    }

    #[zbus(property, name = "Service")]
    fn service(&self) -> OwnedObjectPath {
        self.service.clone()
    }

    #[zbus(property, name = "Flags")]
    fn flags(&self) -> Vec<String> {
        self.flags.clone()
    }

    #[zbus(property, name = "Handle")]
    fn handle(&self) -> u16 {
        self.handle
    }

    #[zbus(property, name = "Handle")]
    fn set_handle(&mut self, handle: u16) {
        self.handle = handle;
    }

    fn read_value(&self, options: HashMap<String, OwnedValue>) -> zbus::fdo::Result<Vec<u8>> {
        let offset = options
            .get("offset")
            .and_then(|value| u16::try_from(value.clone()).ok())
            .unwrap_or(0) as usize;
        let value = self.stored();
        // Past the end is not an error: it is how a client learns it has read everything.
        let tail = value.get(offset..).unwrap_or(&[]).to_vec();
        debug!(
            "beoremote read {} offset {} -> {} bytes",
            self.name,
            offset,
            tail.len()
        );
        Ok(tail)
    }

    fn write_value(
        &self,
        value: Vec<u8>,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        debug!("beoremote write {} <- {:?}", self.name, value);
        self.values
            .lock()
            .map(|mut values| values.insert(self.name.clone(), value.clone()))
            .ok();
        // A full queue means the bridge is wedged, not that the remote should see an error: it
        // would show a failure for a key that will be acted on a moment later anyway.
        if let Err(err) = self.writes.try_send((self.name.clone(), value)) {
            warn!("beoremote write dropped: {err}");
        }
        Ok(())
    }

    fn start_notify(&self) {
        debug!("beoremote notifications on for {}", self.name);
    }

    fn stop_notify(&self) {
        debug!("beoremote notifications off for {}", self.name);
    }
}

impl Characteristic {
    fn stored(&self) -> Vec<u8> {
        self.values
            .lock()
            .ok()
            .and_then(|values| values.get(&self.name).cloned())
            .unwrap_or_default()
    }
}

/// The one characteristic that carries a `Value`: it is 16 bytes, and a notification needs a
/// property to change.
struct NotifyingCharacteristic {
    inner: Characteristic,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl NotifyingCharacteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.inner.uuid.clone()
    }

    #[zbus(property, name = "Service")]
    fn service(&self) -> OwnedObjectPath {
        self.inner.service.clone()
    }

    #[zbus(property, name = "Flags")]
    fn flags(&self) -> Vec<String> {
        self.inner.flags.clone()
    }

    #[zbus(property, name = "Handle")]
    fn handle(&self) -> u16 {
        self.inner.handle
    }

    #[zbus(property, name = "Handle")]
    fn set_handle(&mut self, handle: u16) {
        self.inner.handle = handle;
    }

    #[zbus(property, name = "Value")]
    fn value(&self) -> Vec<u8> {
        self.inner.stored()
    }

    fn read_value(&self, options: HashMap<String, OwnedValue>) -> zbus::fdo::Result<Vec<u8>> {
        self.inner.read_value(options)
    }

    fn write_value(
        &self,
        value: Vec<u8>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        self.inner.write_value(value, options)
    }

    fn start_notify(&self) {
        debug!("beoremote notifications on for {}", self.inner.name);
    }

    fn stop_notify(&self) {
        debug!("beoremote notifications off for {}", self.inner.name);
    }
}

/// Unregister whatever beoremote application this process left with bluez.
///
/// Handles are handed out by walking the database for free space, and an application bluez still
/// believes in occupies its range. Leaving on a signal without saying so pushes the next
/// registration to higher handles -- and the remote caches handles and subscribes to nothing, not
/// even Service Changed, so it goes on writing to where the service used to be. Menu picks then
/// vanish with no error at either end.
pub async fn unregister_leftovers() {
    // The connection this process registered on, if it still has one. Anything else can only ask
    // about somebody else's application, which bluez rightly refuses.
    let owned = OWNER.lock().ok().and_then(|owner| owner.clone());
    let connection = match owned {
        Some(connection) => connection,
        None => match Connection::system().await {
            Ok(connection) => connection,
            Err(_) => return,
        },
    };
    let Ok(path) = ObjectPath::try_from(APP_PATH) else {
        return;
    };
    let Ok(builder) = GattManagerProxy::builder(&connection).path(ADAPTER_PATH) else {
        return;
    };
    if let Ok(manager) = builder.build().await {
        match manager.unregister_application(&path).await {
            Ok(()) => debug!("released the beoremote service"),
            Err(err) => debug!("nothing to release: {err}"),
        }
    }
}

/// The application object, which is what bluez walks to find our services.
///
/// This answers `GetManagedObjects` itself instead of leaning on zbus's own object manager, for one
/// reason: **order**. bluez numbers the attributes in the order it receives them, and zbus keeps its
/// objects in a hash map, so every run handed them over in a different order and every restart gave
/// the characteristics different handles -- measured as char21 at 258, char20 at 267, char00 at 285.
/// The remote caches handles, so after a restart its menu picks went to whatever now sat where
/// ACTIVE_SOURCE used to be. A `BTreeMap` keyed by object path is ordered, and the paths are padded
/// (`char00`, `char01`, ...) so that order is the registration order.
struct Application {
    services: Vec<(OwnedObjectPath, String, u16)>,
    characteristics: Vec<CharacteristicEntry>,
}

struct CharacteristicEntry {
    path: OwnedObjectPath,
    uuid: String,
    service: OwnedObjectPath,
    flags: Vec<String>,
}

type Properties = BTreeMap<String, OwnedValue>;
type Interfaces = BTreeMap<String, Properties>;

/// An object path that can be a `BTreeMap` key.
///
/// `OwnedObjectPath` deliberately does not order, and the ordering is the entire point here: it is
/// what decides the order bluez receives the attributes in, and therefore which handles they get.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PathKey(String);

impl zbus::zvariant::Type for PathKey {
    const SIGNATURE: &'static zbus::zvariant::Signature =
        <ObjectPath<'_> as zbus::zvariant::Type>::SIGNATURE;
}

impl serde::Serialize for PathKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ObjectPath::try_from(self.0.as_str())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl From<&OwnedObjectPath> for PathKey {
    fn from(path: &OwnedObjectPath) -> Self {
        PathKey(path.as_str().to_string())
    }
}

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl Application {
    fn get_managed_objects(&self) -> zbus::fdo::Result<BTreeMap<PathKey, Interfaces>> {
        let mut objects: BTreeMap<PathKey, Interfaces> = BTreeMap::new();
        for (path, uuid, handle) in &self.services {
            let mut properties = Properties::new();
            properties.insert("UUID".to_string(), value(uuid.as_str())?);
            properties.insert("Primary".to_string(), value(true)?);
            properties.insert("Handle".to_string(), value(*handle)?);
            objects.insert(
                PathKey::from(path),
                Interfaces::from([("org.bluez.GattService1".to_string(), properties)]),
            );
        }
        for entry in &self.characteristics {
            let mut properties = Properties::new();
            properties.insert("UUID".to_string(), value(entry.uuid.as_str())?);
            properties.insert("Service".to_string(), value(entry.service.clone())?);
            properties.insert("Flags".to_string(), value(entry.flags.clone())?);
            // Asked for as "allocate one", which is what bluez requires inside a service; it writes
            // back what it chose, and that is what makes the layout checkable from the outside.
            properties.insert("Handle".to_string(), value(AUTO_HANDLE)?);
            objects.insert(
                PathKey::from(&entry.path),
                Interfaces::from([("org.bluez.GattCharacteristic1".to_string(), properties)]),
            );
        }
        Ok(objects)
    }
}

fn value<'a, T: Into<zbus::zvariant::Value<'a>>>(from: T) -> zbus::fdo::Result<OwnedValue> {
    OwnedValue::try_from(from.into())
        .map_err(|err| zbus::fdo::Error::Failed(format!("property: {err}")))
}

/// The service, registered with BlueZ for as long as this is alive.
pub struct BeoremoteGatt {
    connection: Connection,
    values: Values,
    paths: HashMap<String, OwnedObjectPath>,
    /// Which bluetoothd we registered with, so a replacement is recognised as one.
    owner: Option<String>,
}

impl BeoremoteGatt {
    /// Serve the service and hand BlueZ the application. Writes from the remote arrive on `writes`.
    pub async fn register(writes: mpsc::Sender<Write>) -> Result<Self> {
        // A previous run that was killed rather than asked to stop leaves its application behind,
        // and bluez would then put ours above it -- at handles the remote does not know.
        //
        // NOT by restarting bluetoothd, which was tried and reverted: bluez does hand out fresh
        // handles after a restart, but a Beoremote One whose link is cut that way stops advertising
        // altogether and only comes back after a factory reset. Measured three times in one evening.
        // Whatever the answer to the handles is, it cannot be that.
        unregister_leftovers().await;

        let connection = Connection::system()
            .await
            .context("connect to the system bus (is dbus running?)")?;
        let values: Values = Arc::new(Mutex::new(HashMap::new()));
        let server = connection.object_server();

        // Built up in registration order and handed to bluez in that order; see `Application`.
        let mut listing = Application {
            services: Vec::new(),
            characteristics: Vec::new(),
        };
        server
            .at(
                SERVICE_PATH,
                Service {
                    uuid: uuid_for(0),
                    handle: SERVICE_HANDLE,
                },
            )
            .await
            .context("serve the beoremote service")?;
        server
            .at(
                DIS_PATH,
                Service {
                    uuid: DIS_UUID.to_string(),
                    handle: DIS_HANDLE,
                },
            )
            .await
            .context("serve device information")?;

        let service_path = OwnedObjectPath::try_from(SERVICE_PATH)?;
        let dis_path = OwnedObjectPath::try_from(DIS_PATH)?;
        listing
            .services
            .push((service_path.clone(), uuid_for(0), SERVICE_HANDLE));
        listing
            .services
            .push((dis_path.clone(), DIS_UUID.to_string(), DIS_HANDLE));
        let mut paths = HashMap::new();

        for (index, (name, flags)) in REGISTRATION_ORDER.iter().enumerate() {
            let Some(number) = attribute_uuid_number(name) else {
                continue;
            };
            // Zero-padded, and Device Information sits at service1: bluez lays the database out in
            // the order it walks our objects, and it walks them by path. Unpadded indices put char10
            // before char2, and a "dis" path sorts before "service0" -- which interleaves two
            // services' characteristics into one another. B&O's own source says the remote caches
            // handles and that the order must not move; a shuffled, interleaved table is worse than
            // that, and a remote that reboots when it reads one is not a remote to hand that to.
            let path = OwnedObjectPath::try_from(format!("{SERVICE_PATH}/char{index:02}"))?;
            let characteristic = Characteristic {
                name: (*name).to_string(),
                uuid: uuid_for(number),
                handle: AUTO_HANDLE,
                service: service_path.clone(),
                flags: flags
                    .iter()
                    .flat_map(|flag| flag.as_strs().iter().map(|name| (*name).to_string()))
                    .collect(),
                values: Arc::clone(&values),
                writes: writes.clone(),
            };
            if *name == NOTIFYING_ATTRIBUTE {
                server
                    .at(
                        &path,
                        NotifyingCharacteristic {
                            inner: characteristic,
                        },
                    )
                    .await
                    .with_context(|| format!("serve characteristic {name}"))?;
            } else {
                server
                    .at(&path, characteristic)
                    .await
                    .with_context(|| format!("serve characteristic {name}"))?;
            }
            listing.characteristics.push(CharacteristicEntry {
                path: path.clone(),
                uuid: uuid_for(number),
                service: service_path.clone(),
                flags: flags
                    .iter()
                    .flat_map(|flag| flag.as_strs().iter().map(|name| (*name).to_string()))
                    .collect(),
            });
            paths.insert((*name).to_string(), path);
        }

        for (index, (name, uuid, value)) in [
            ("PNP_ID", PNP_ID_UUID, PNP_ID.to_vec()),
            ("MANUFACTURER", MANUFACTURER_UUID, MANUFACTURER.to_vec()),
            ("MODEL", MODEL_UUID, MODEL.to_vec()),
            ("FIRMWARE", FIRMWARE_UUID, FIRMWARE.to_vec()),
        ]
        .into_iter()
        .enumerate()
        {
            values
                .lock()
                .map(|mut values| values.insert(name.to_string(), value))
                .ok();
            let path = OwnedObjectPath::try_from(format!("{DIS_PATH}/char{index:02}"))?;
            server
                .at(
                    &path,
                    Characteristic {
                        name: name.to_string(),
                        uuid: uuid.to_string(),
                        handle: AUTO_HANDLE,
                        service: dis_path.clone(),
                        flags: vec!["read".to_string()],
                        values: Arc::clone(&values),
                        writes: writes.clone(),
                    },
                )
                .await
                .with_context(|| format!("serve {name}"))?;
            listing.characteristics.push(CharacteristicEntry {
                path: path.clone(),
                uuid: uuid.to_string(),
                service: dis_path.clone(),
                flags: vec![Flag::Read.as_strs()[0].to_string()],
            });
            paths.insert(name.to_string(), path);
        }

        server
            .at(APP_PATH, listing)
            .await
            .context("serve the application")?;

        let manager = GattManagerProxy::builder(&connection)
            .path(ADAPTER_PATH)?
            .build()
            .await
            .context("talk to bluez (is bluetoothd running?)")?;
        manager
            .register_application(&ObjectPath::try_from(APP_PATH)?, HashMap::new())
            .await
            .context("register the beoremote service with bluez")?;

        let owner = match DBusProxy::new(&connection).await {
            Ok(dbus) => dbus
                .get_name_owner(BusName::try_from("org.bluez")?)
                .await
                .ok()
                .map(|owner| owner.to_string()),
            Err(_) => None,
        };

        if let Ok(mut slot) = OWNER.lock() {
            *slot = Some(connection.clone());
        }

        Ok(Self {
            connection,
            values,
            paths,
            owner,
        })
    }

    /// Set an attribute's value. The remote picks it up the next time it reads.
    pub fn set(&self, name: &str, value: &[u8]) {
        if let Ok(mut values) = self.values.lock() {
            values.insert(name.to_string(), value.to_vec());
        }
    }

    /// Tell the remote an attribute changed, for the ones it subscribes to.
    ///
    /// This is what makes a menu that changed while the remote was looking at it get re-read; BlueZ
    /// turns a `Value` property change into the notification or indication on the wire.
    pub async fn announce(&self, name: &str) -> Result<()> {
        let Some(path) = self.paths.get(name) else {
            return Ok(());
        };
        let emitter = SignalEmitter::new(&self.connection, path)?;
        let interface = self
            .connection
            .object_server()
            .interface::<_, NotifyingCharacteristic>(path)
            .await?;
        interface
            .get()
            .await
            .value_changed(&emitter)
            .await
            .with_context(|| format!("announce {name}"))?;
        Ok(())
    }

    /// Resolve when bluez goes away.
    ///
    /// A registered application does not survive a bluetoothd restart -- and bluetoothd does restart:
    /// it segfaulted once during this work, and systemd brought it straight back. Without watching
    /// for that, the bridge sits there believing it is registered while the remote has nothing to
    /// read, which looks exactly like a remote that has stopped working.
    pub async fn wait_until_bluez_goes_away(&self) -> Result<()> {
        let dbus = DBusProxy::new(&self.connection)
            .await
            .context("talk to the bus")?;
        let name = BusName::try_from("org.bluez")?;
        // Compared by *owner*, not by existence. systemd restarts bluetoothd in well under a second,
        // so polling for the name to be missing sees nothing at all -- while the application we
        // registered is gone with the process that held it.
        loop {
            match dbus.get_name_owner(name.clone()).await {
                Err(_) => return Ok(()),
                Ok(owner) => {
                    let owner = owner.to_string();
                    if self.owner.as_deref().is_some_and(|known| known != owner) {
                        return Ok(());
                    }
                }
            }
            tokio::time::sleep(BLUEZ_CHECK_INTERVAL).await;
        }
    }

    pub async fn unregister(&self) {
        let Ok(path) = ObjectPath::try_from(APP_PATH) else {
            return;
        };
        let Ok(builder) = GattManagerProxy::builder(&self.connection).path(ADAPTER_PATH) else {
            return;
        };
        if let Ok(manager) = builder.build().await {
            let _ = manager.unregister_application(&path).await;
        }
    }
}

/// UUID number for an attribute name.
///
/// The attribute *enum* and the UUID number are not the same: the UUIDs skip `0x09`-`0x0C`, so
/// everything from `SPEAKER_GROUPS` on sits four higher than its enum. Getting this wrong writes a
/// different attribute, which is exactly the kind of mistake that shows up as a menu that is subtly
/// the wrong list.
fn attribute_uuid_number(name: &str) -> Option<u8> {
    let enumerated = crate::beoremote::protocol::attribute(name)?;
    Some(if enumerated >= 9 {
        enumerated + 4
    } else {
        enumerated
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_numbers_skip_the_gap_b_and_o_left() {
        // Below the gap the enum and the UUID agree...
        assert_eq!(attribute_uuid_number("VERSION"), Some(0x01));
        assert_eq!(attribute_uuid_number("ACTIVE_STAND_POSITION"), Some(0x08));
        // ...and above it the UUID is four higher, which is where B&O's table jumps.
        assert_eq!(attribute_uuid_number("SPEAKER_GROUPS"), Some(0x0D));
        assert_eq!(attribute_uuid_number("MUSIC_SOURCES"), Some(0x19));
        assert_eq!(attribute_uuid_number("SOURCE_CONTENT_1"), Some(0x24));
        assert_eq!(attribute_uuid_number("VOLUME"), Some(0x30));
        assert_eq!(attribute_uuid_number("NOT_AN_ATTRIBUTE"), None);
    }

    #[test]
    fn every_registered_attribute_has_a_uuid() {
        for (name, _) in REGISTRATION_ORDER {
            assert!(
                attribute_uuid_number(name).is_some(),
                "{name} has no UUID number"
            );
        }
    }

    #[test]
    fn the_service_uuid_is_b_and_os_own_base() {
        // Note `1000-1000`: this is not the Bluetooth base, and a UUID built on the usual
        // `1000-8000` one is a different service that the remote will not look at.
        assert_eq!(uuid_for(0), "00000000-0000-1000-1000-00805f9b34fb");
        assert_eq!(uuid_for(0x19), "00000019-0000-1000-1000-00805f9b34fb");
    }
}
