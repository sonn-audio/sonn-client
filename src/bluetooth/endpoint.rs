//! The A2DP sink: what we tell a phone we can play, and how the audio arrives.
//!
//! BlueZ does the radio and the profile; the endpoint is where an application says "I am a
//! loudspeaker, and this is the audio I accept". A phone offers its capabilities, `SelectConfiguration`
//! picks from them, and once the two agree BlueZ hands over an `org.bluez.MediaTransport1` whose
//! `Acquire()` returns a socket with RTP-framed audio on it.
//!
//! The one place quality is decided is `select`, and it is decided in four bytes. SBC's ceiling is
//! set by its *bitpool*: the default most stacks settle on is 53, which is the 328 kbps that gave
//! SBC its reputation. The maximum a phone will accept here is asked for instead -- that is all
//! "SBC-XQ" is, and on a joint-stereo 48 kHz stream it is the difference between "fine for a phone
//! call" and something worth putting through a BeoLab.

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{interface, proxy, Connection};

/// Where our endpoint lives on the bus.
const ENDPOINT_PATH: &str = "/sonn/bluetooth/sink";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
/// A2DP Sink, which is what a loudspeaker is.
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
/// SBC. Every phone has it, and with the bitpool opened up it is good.
const SBC_CODEC: u8 = 0x00;

/// The four bytes a phone and a speaker agree on, as SBC defines them.
///
/// Byte 0 is sampling frequency and channel mode, byte 1 block length, subbands and allocation,
/// bytes 2 and 3 the bitpool range. Every bit set means "I accept this"; the answer picks one of
/// each and narrows the bitpool to what will actually be sent.
mod sbc {
    pub const FREQ_44100: u8 = 1 << 5;
    pub const FREQ_48000: u8 = 1 << 4;
    pub const CHANNEL_JOINT_STEREO: u8 = 1 << 0;
    pub const CHANNEL_STEREO: u8 = 1 << 1;

    /// Block length and subbands, as A2DP lays the byte out: the bits run worst to best, so 4 blocks
    /// is bit 7 and 16 blocks is bit 4, and 4 subbands is bit 3 against 8 subbands at bit 2. More of
    /// either means fewer header bits per sample, so the *lowest* set bit in each field is the one
    /// worth having -- which is the opposite of what "take the highest" would give.
    pub const BLOCKS_16: u8 = 1 << 4;
    pub const BLOCKS_FIELD: u8 = 0xF0;
    pub const SUBBANDS_8: u8 = 1 << 2;
    pub const SUBBANDS_FIELD: u8 = 0x0C;
    /// Loudness over SNR: it is what every encoder uses and what SBC was tuned on.
    pub const ALLOCATION_LOUDNESS: u8 = 1 << 0;
    pub const ALLOCATION_FIELD: u8 = 0x03;

    /// The lowest bitpool worth accepting, and the highest worth asking for.
    ///
    /// 53 is where most stacks stop; 76 is the ceiling for two channels at 48 kHz within the packet
    /// size A2DP allows, and it is what everyone means by "SBC-XQ".
    pub const BITPOOL_MIN: u8 = 2;
    pub const BITPOOL_MAX: u8 = 76;
}

#[proxy(
    interface = "org.bluez.Media1",
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0"
)]
trait Media {
    fn register_endpoint(
        &self,
        endpoint: &ObjectPath<'_>,
        properties: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_endpoint(&self, endpoint: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.MediaTransport1", default_service = "org.bluez")]
pub(super) trait MediaTransport {
    /// The socket carrying the audio, with the read and write sizes bluez negotiated.
    fn acquire(&self) -> zbus::Result<(zbus::zvariant::OwnedFd, u16, u16)>;
    fn release(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn configuration(&self) -> zbus::Result<Vec<u8>>;
    /// AVRCP absolute volume, 0-127. What the slider on the phone means.
    #[zbus(property)]
    fn volume(&self) -> zbus::Result<u16>;
    #[zbus(property)]
    fn set_volume(&self, volume: u16) -> zbus::Result<()>;
}

/// What the endpoint has been told to carry, once a phone has agreed to it.
#[derive(Debug, Clone, Default)]
pub struct Transport {
    pub path: Option<OwnedObjectPath>,
    /// The device object the transport belongs to, which is how a stream is tied to a phone.
    pub device: Option<OwnedObjectPath>,
    /// Sample rate and channels, read back from the configuration both ends settled on.
    pub sample_rate: u32,
    pub channels: u8,
    pub bitpool: u8,
}

/// Our end of A2DP, registered with bluez for as long as this is alive.
pub struct A2dpEndpoint {
    connection: Connection,
    transport: Arc<Mutex<Transport>>,
}

struct Endpoint {
    transport: Arc<Mutex<Transport>>,
}

#[interface(name = "org.bluez.MediaEndpoint1")]
impl Endpoint {
    /// Pick what will be sent, from what the phone says it can do.
    ///
    /// This is the whole quality decision. The phone hands its capabilities as the same four bytes;
    /// the answer sets exactly one frequency and one channel mode, and takes the bitpool as high as
    /// both ends allow.
    fn select_configuration(&self, capabilities: Vec<u8>) -> zbus::fdo::Result<Vec<u8>> {
        let chosen = select(&capabilities).ok_or_else(|| {
            zbus::fdo::Error::NotSupported("no sbc configuration in common".to_string())
        })?;
        info!(
            "bluetooth: accepting SBC {} Hz, {}, bitpool {}",
            frequency_of(chosen[0]).unwrap_or(0),
            if chosen[0] & sbc::CHANNEL_JOINT_STEREO != 0 {
                "joint stereo"
            } else {
                "stereo"
            },
            chosen[3]
        );
        Ok(chosen.to_vec())
    }

    /// A phone agreed; this is the transport the audio will arrive on.
    fn set_configuration(&self, transport: OwnedObjectPath, properties: super::Properties) {
        let configuration = properties
            .get("Configuration")
            .and_then(|value| Vec::<u8>::try_from(value.clone()).ok())
            .unwrap_or_default();
        let device = properties
            .get("Device")
            .and_then(|value| OwnedObjectPath::try_from(value.clone()).ok());
        let (sample_rate, channels, bitpool) = describe(&configuration);
        info!(
            "bluetooth: stream configured on {} -- {} Hz, {}ch, bitpool {}",
            transport.as_str(),
            sample_rate,
            channels,
            bitpool
        );
        if let Ok(mut slot) = self.transport.lock() {
            *slot = Transport {
                path: Some(transport),
                device,
                sample_rate,
                channels,
                bitpool,
            };
        }
    }

    /// The phone went away, or stopped.
    fn clear_configuration(&self, transport: OwnedObjectPath) {
        debug!("bluetooth: stream cleared on {}", transport.as_str());
        if let Ok(mut slot) = self.transport.lock() {
            if slot.path.as_ref() == Some(&transport) {
                *slot = Transport::default();
            }
        }
    }

    fn release(&self) {
        debug!("bluetooth: bluez released the endpoint");
        if let Ok(mut slot) = self.transport.lock() {
            *slot = Transport::default();
        }
    }
}

impl A2dpEndpoint {
    pub async fn register(connection: &Connection) -> Result<Self> {
        let transport = Arc::new(Mutex::new(Transport::default()));
        let path = ObjectPath::try_from(ENDPOINT_PATH).expect("a literal path");
        connection
            .object_server()
            .at(
                &path,
                Endpoint {
                    transport: Arc::clone(&transport),
                },
            )
            .await
            .context("serve the a2dp endpoint")?;

        let media = MediaProxy::builder(connection)
            .path(ADAPTER_PATH)?
            .build()
            .await
            .context("talk to bluez media")?;
        let capabilities = capabilities();
        let properties = std::collections::HashMap::from([
            ("UUID", zbus::zvariant::Value::from(A2DP_SINK_UUID)),
            ("Codec", zbus::zvariant::Value::from(SBC_CODEC)),
            (
                "Capabilities",
                zbus::zvariant::Value::from(capabilities.to_vec()),
            ),
        ]);
        media
            .register_endpoint(&path, properties)
            .await
            .context("register the a2dp sink with bluez")?;
        info!(
            "bluetooth: A2DP sink registered, SBC up to bitpool {}",
            sbc::BITPOOL_MAX
        );

        Ok(Self {
            connection: connection.clone(),
            transport,
        })
    }

    /// The phone currently sending audio, if any.
    pub async fn streaming_address(&self) -> Option<String> {
        let device = self.transport.lock().ok()?.device.clone()?;
        let properties = zbus::fdo::PropertiesProxy::builder(&self.connection)
            .destination("org.bluez")
            .ok()?
            .path(device)
            .ok()?
            .build()
            .await
            .ok()?;
        let value = properties
            .get(
                zbus::names::InterfaceName::try_from("org.bluez.Device1").ok()?,
                "Address",
            )
            .await
            .ok()?;
        String::try_from(OwnedValue::from(value)).ok()
    }

    /// What the stream is, for whoever has to turn it into audio.
    pub fn transport(&self) -> Transport {
        self.transport
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }
}

impl Drop for A2dpEndpoint {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        tokio::spawn(async move {
            let path = ObjectPath::try_from(ENDPOINT_PATH).expect("a literal path");
            if let Ok(media) = MediaProxy::builder(&connection)
                .path(ADAPTER_PATH)
                .expect("a literal path")
                .build()
                .await
            {
                let _ = media.unregister_endpoint(&path).await;
            }
            let _ = connection.object_server().remove::<Endpoint, _>(&path).await;
        });
    }
}

/// What this speaker accepts: both rates, both stereo modes, and a bitpool as wide as SBC allows.
fn capabilities() -> [u8; 4] {
    [
        sbc::FREQ_44100 | sbc::FREQ_48000 | sbc::CHANNEL_JOINT_STEREO | sbc::CHANNEL_STEREO,
        sbc::BLOCKS_16 | sbc::SUBBANDS_8 | sbc::ALLOCATION_LOUDNESS,
        sbc::BITPOOL_MIN,
        sbc::BITPOOL_MAX,
    ]
}

/// Choose one configuration out of what a phone offers.
///
/// One frequency, one channel mode, one block length, one allocation -- a configuration with two
/// bits set in a field is not a configuration. 48 kHz over 44.1 where both are offered, joint stereo
/// over plain stereo because it costs nothing and helps at any bitpool, and the highest bitpool both
/// ends allow, which is the entire difference between ordinary SBC and SBC-XQ.
fn select(capabilities: &[u8]) -> Option<[u8; 4]> {
    if capabilities.len() < 4 {
        return None;
    }
    let frequency = if capabilities[0] & sbc::FREQ_48000 != 0 {
        sbc::FREQ_48000
    } else if capabilities[0] & sbc::FREQ_44100 != 0 {
        sbc::FREQ_44100
    } else {
        return None;
    };
    let channels = if capabilities[0] & sbc::CHANNEL_JOINT_STEREO != 0 {
        sbc::CHANNEL_JOINT_STEREO
    } else if capabilities[0] & sbc::CHANNEL_STEREO != 0 {
        sbc::CHANNEL_STEREO
    } else {
        return None;
    };

    // The best of each field, which is its lowest set bit -- see the note on the constants.
    let blocks = best_in_field(capabilities[1], sbc::BLOCKS_FIELD)?;
    let subbands = best_in_field(capabilities[1], sbc::SUBBANDS_FIELD)?;
    let allocation = best_in_field(capabilities[1], sbc::ALLOCATION_FIELD)?;

    let min = capabilities[2].max(sbc::BITPOOL_MIN);
    let max = capabilities[3].min(sbc::BITPOOL_MAX);
    if max < min {
        return None;
    }
    Some([frequency | channels, blocks | subbands | allocation, min, max])
}

/// The best option a phone offers within one field: its lowest set bit.
fn best_in_field(offered: u8, field: u8) -> Option<u8> {
    let available = offered & field;
    if available == 0 {
        return None;
    }
    Some(1 << available.trailing_zeros())
}

fn frequency_of(byte: u8) -> Option<u32> {
    if byte & sbc::FREQ_48000 != 0 {
        Some(48_000)
    } else if byte & sbc::FREQ_44100 != 0 {
        Some(44_100)
    } else {
        None
    }
}

/// Read a settled configuration back: what is actually going to arrive.
fn describe(configuration: &[u8]) -> (u32, u8, u8) {
    if configuration.len() < 4 {
        return (48_000, 2, 0);
    }
    let rate = frequency_of(configuration[0]).unwrap_or(48_000);
    let channels = if configuration[0] & (sbc::CHANNEL_JOINT_STEREO | sbc::CHANNEL_STEREO) != 0 {
        2
    } else {
        1
    };
    (rate, channels, configuration[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a phone typically offers: both rates, every channel mode, bitpool 2..53.
    fn phone() -> [u8; 4] {
        [0x3F, 0xFF, 2, 53]
    }

    #[test]
    fn the_answer_names_exactly_one_of_each() {
        let chosen = select(&phone()).expect("a configuration");
        // One frequency bit and one channel bit, never two.
        assert_eq!(chosen[0] & 0xF0, sbc::FREQ_48000);
        assert_eq!(chosen[0] & 0x0F, sbc::CHANNEL_JOINT_STEREO);
        // 16 blocks, 8 subbands and loudness: the best of each field, not the first bit in it.
        assert_eq!(chosen[1] & sbc::BLOCKS_FIELD, sbc::BLOCKS_16);
        assert_eq!(chosen[1] & sbc::SUBBANDS_FIELD, sbc::SUBBANDS_8);
        assert_eq!(chosen[1] & sbc::ALLOCATION_FIELD, sbc::ALLOCATION_LOUDNESS);
    }

    #[test]
    fn the_bitpool_goes_as_high_as_the_phone_allows() {
        // A phone that stops at 53 gets 53 -- asking for more than it offered would be refused.
        assert_eq!(select(&phone()).expect("a configuration")[3], 53);
        // One that allows more gets our ceiling, which is what SBC-XQ is.
        let generous = [0x3F, 0xFF, 2, 250];
        assert_eq!(select(&generous).expect("a configuration")[3], sbc::BITPOOL_MAX);
    }

    #[test]
    fn a_field_offering_only_the_lesser_option_takes_it() {
        // 4 blocks (bit 7) and 4 subbands (bit 3) only, with SNR allocation (bit 1).
        let sparse = [0x3F, 0b1000_1010, 2, 53];
        let chosen = select(&sparse).expect("a configuration");
        assert_eq!(chosen[1] & sbc::BLOCKS_FIELD, 1 << 7);
        assert_eq!(chosen[1] & sbc::SUBBANDS_FIELD, 1 << 3);
        assert_eq!(chosen[1] & sbc::ALLOCATION_FIELD, 1 << 1);
    }

    #[test]
    fn a_phone_that_only_does_44_1_gets_44_1() {
        let old = [sbc::FREQ_44100 | sbc::CHANNEL_STEREO, 0xFF, 2, 35];
        let chosen = select(&old).expect("a configuration");
        assert_eq!(chosen[0] & 0xF0, sbc::FREQ_44100);
        assert_eq!(chosen[0] & 0x0F, sbc::CHANNEL_STEREO);
        assert_eq!(chosen[3], 35);
        assert_eq!(frequency_of(chosen[0]), Some(44_100));
    }

    #[test]
    fn nothing_in_common_is_refused_rather_than_guessed() {
        // No frequency we can take.
        assert!(select(&[0x0F, 0xFF, 2, 53]).is_none());
        // No channel mode.
        assert!(select(&[0x30, 0xFF, 2, 53]).is_none());
        // A bitpool range that does not overlap ours.
        assert!(select(&[0x3F, 0xFF, 250, 255]).is_none());
        // And a truncated offer is not an offer.
        assert!(select(&[0x3F, 0xFF]).is_none());
    }

    #[test]
    fn a_settled_configuration_reads_back_as_what_will_arrive() {
        let chosen = select(&phone()).expect("a configuration");
        assert_eq!(describe(&chosen), (48_000, 2, 53));
    }
}
