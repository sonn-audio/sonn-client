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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{interface, proxy, Connection};

/// Where our application lives on the bus. BlueZ only needs it to be ours and stable.
const APP_PATH: &str = "/sonn/beoremote";
const SERVICE_PATH: &str = "/sonn/beoremote/service0";
const DIS_PATH: &str = "/sonn/beoremote/dis";
const ADAPTER_PATH: &str = "/org/bluez/hci0";

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
const PNP_ID: [u8; 7] = [0x01, 0x03, 0x01, 0x0B, 0x10, 0x00, 0x00];
const MANUFACTURER: &[u8] = b"Bang & Olufsen";

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
    fn as_str(self) -> &'static str {
        match self {
            // "write" is a write *with* a response; the remote waits for one.
            Flag::Read => "read",
            Flag::Write => "write",
            Flag::Notify => "notify",
            Flag::Indicate => "indicate",
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

/// A write the remote made, as an attribute name and its value.
pub type Write = (String, Vec<u8>);

struct Service {
    uuid: String,
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
}

struct Characteristic {
    name: String,
    uuid: String,
    service: OwnedObjectPath,
    flags: Vec<String>,
    values: Values,
    writes: mpsc::Sender<Write>,
}

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

    /// The current value, which is also what BlueZ sends when a notify goes out.
    #[zbus(property, name = "Value")]
    fn value(&self) -> Vec<u8> {
        self.stored()
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

/// The service, registered with BlueZ for as long as this is alive.
pub struct BeoremoteGatt {
    connection: Connection,
    values: Values,
    paths: HashMap<String, OwnedObjectPath>,
}

impl BeoremoteGatt {
    /// Serve the service and hand BlueZ the application. Writes from the remote arrive on `writes`.
    pub async fn register(writes: mpsc::Sender<Write>) -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("connect to the system bus (is dbus running?)")?;
        let values: Values = Arc::new(Mutex::new(HashMap::new()));
        let server = connection.object_server();

        // BlueZ walks the application with ObjectManager, so the root has to answer for one.
        server
            .at(APP_PATH, zbus::fdo::ObjectManager)
            .await
            .context("serve the object manager")?;
        server
            .at(
                SERVICE_PATH,
                Service {
                    uuid: uuid_for(0),
                },
            )
            .await
            .context("serve the beoremote service")?;
        server
            .at(
                DIS_PATH,
                Service {
                    uuid: DIS_UUID.to_string(),
                },
            )
            .await
            .context("serve device information")?;

        let service_path = OwnedObjectPath::try_from(SERVICE_PATH)?;
        let dis_path = OwnedObjectPath::try_from(DIS_PATH)?;
        let mut paths = HashMap::new();

        for (index, (name, flags)) in REGISTRATION_ORDER.iter().enumerate() {
            let Some(number) = attribute_uuid_number(name) else {
                continue;
            };
            let path = OwnedObjectPath::try_from(format!("{SERVICE_PATH}/char{index}"))?;
            server
                .at(
                    &path,
                    Characteristic {
                        name: (*name).to_string(),
                        uuid: uuid_for(number),
                        service: service_path.clone(),
                        flags: flags.iter().map(|flag| flag.as_str().to_string()).collect(),
                        values: Arc::clone(&values),
                        writes: writes.clone(),
                    },
                )
                .await
                .with_context(|| format!("serve characteristic {name}"))?;
            paths.insert((*name).to_string(), path);
        }

        for (index, (name, uuid, value)) in [
            ("PNP_ID", PNP_ID_UUID, PNP_ID.to_vec()),
            ("MANUFACTURER", MANUFACTURER_UUID, MANUFACTURER.to_vec()),
        ]
        .into_iter()
        .enumerate()
        {
            values
                .lock()
                .map(|mut values| values.insert(name.to_string(), value))
                .ok();
            let path = OwnedObjectPath::try_from(format!("{DIS_PATH}/char{index}"))?;
            server
                .at(
                    &path,
                    Characteristic {
                        name: name.to_string(),
                        uuid: uuid.to_string(),
                        service: dis_path.clone(),
                        flags: vec![Flag::Read.as_str().to_string()],
                        values: Arc::clone(&values),
                        writes: writes.clone(),
                    },
                )
                .await
                .with_context(|| format!("serve {name}"))?;
            paths.insert(name.to_string(), path);
        }

        let manager = GattManagerProxy::builder(&connection)
            .path(ADAPTER_PATH)?
            .build()
            .await
            .context("talk to bluez (is bluetoothd running?)")?;
        manager
            .register_application(&ObjectPath::try_from(APP_PATH)?, HashMap::new())
            .await
            .context("register the beoremote service with bluez")?;

        Ok(Self {
            connection,
            values,
            paths,
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
            .interface::<_, Characteristic>(path)
            .await?;
        interface
            .get()
            .await
            .value_changed(&emitter)
            .await
            .with_context(|| format!("announce {name}"))?;
        Ok(())
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
