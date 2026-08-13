//! Keys from the remote, straight off the kernel's input devices.
//!
//! On a stock BlueZ the remote is an ordinary HID peripheral: the kernel turns its reports into
//! `/dev/input/event*` nodes and hands us Linux key codes. That is the whole story here -- there is
//! no vendor socket in the path, and no translation either. What the kernel calls a key is what the
//! server is told, because only the server knows what a zone is playing and therefore what a key
//! should do.
//!
//! Two details that are not obvious:
//!
//! * The nodes are **grabbed** (`EVIOCGRAB`). Without that, every press is also a key press on the
//!   Pi's console -- and the standby button is `KEY_POWER`, which logind acts on.
//! * They come and go. The remote sleeps, its nodes disappear, and new ones appear when it wakes,
//!   so this rescans rather than opening once at startup.

use crate::beoremote::api::BeoremoteApi;
use crate::status::Registry;
use crate::supervisor::{VolumeIntent, VolumeRequest};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Devices a remote registers. All of them are grabbed: the volume keys and the button page are
/// separate nodes, and which is which is not fixed.
///
/// Matched on the name the kernel gives them, which is the remote's own Bluetooth name -- so a
/// Beoremote One appears as `BEORC…` and an Essence as `BeoSound Essence Keyboard`, under the name
/// of the product it shipped with.
const REMOTE_NAMES: [&str; 2] = ["BEORC", "BeoSound Essence"];
/// How often to look for input devices that appeared or went away.
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// `EV_KEY`, the only event type worth reading here.
const EV_KEY: u16 = 0x01;
/// A press. Releases are 0 and auto-repeats are 2; both are ignored, because the server acts on the
/// press and a held key would otherwise fire until it is let go.
const KEY_PRESS: i32 = 1;
/// `KEY_VOLUMEUP` / `KEY_VOLUMEDOWN`.
const KEY_VOLUME_UP: u16 = 115;
const KEY_VOLUME_DOWN: u16 = 114;

/// What a reader thread reports: a press, or that its device is gone.
enum Event {
    Key(u16),
    Closed(PathBuf),
}

/// One `struct input_event`: a timeval, then type, code and value.
const EVENT_SIZE: usize = std::mem::size_of::<libc_input_event>();

#[repr(C)]
#[derive(Clone, Copy)]
struct libc_input_event {
    tv_sec: i64,
    tv_usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

/// Read every remote key until the bridge stops, forwarding each one.
pub async fn run(
    api_base_url: String,
    zone_id: u32,
    volume_player: Option<String>,
    volume_step: u8,
    volume_tx: mpsc::Sender<VolumeRequest>,
    statuses: Registry,
) {
    let api = BeoremoteApi::new(&api_base_url, zone_id).ok();
    let (keys_tx, mut keys) = mpsc::channel::<Event>(64);
    let mut open: HashSet<PathBuf> = HashSet::new();
    let mut rescan = tokio::time::interval(RESCAN_INTERVAL);

    loop {
        tokio::select! {
            _ = rescan.tick() => {
                for path in remote_event_devices() {
                    if open.contains(&path) {
                        continue;
                    }
                    match spawn_reader(path.clone(), keys_tx.clone()) {
                        Ok(()) => {
                            info!("beoremote reading keys from {}", path.display());
                            open.insert(path);
                            // "The remote's keys reach us", which is what the field has always
                            // meant; it is just no longer a socket that says so.
                            statuses.set_beoremote_hid(true);
                        }
                        Err(err) => debug!("beoremote could not read {}: {:#}", path.display(), err),
                    }
                }
            }
            event = keys.recv() => {
                match event {
                    None => return,
                    Some(Event::Key(code)) => {
                        handle(code, &api, &volume_player, volume_step, &volume_tx).await;
                    }
                    // Re-pairing the remote destroys its input devices and the kernel makes new ones
                    // under the same names. Forgetting the path is what lets the rescan pick the new
                    // device up; without it the keys stop at the next pairing and never come back.
                    Some(Event::Closed(path)) => {
                        debug!("beoremote input {} went away", path.display());
                        open.remove(&path);
                        statuses.set_beoremote_hid(!open.is_empty());
                    }
                }
            }
        }
    }
}

async fn handle(
    code: u16,
    api: &Option<BeoremoteApi>,
    volume_player: &Option<String>,
    volume_step: u8,
    volume_tx: &mpsc::Sender<VolumeRequest>,
) {
    if code == KEY_VOLUME_UP || code == KEY_VOLUME_DOWN {
        // Kept local. Volume arrives in bursts of six presses, and it has to keep working while the
        // server is briefly away; everything else is the server's decision.
        let step = i16::from(volume_step);
        let delta = if code == KEY_VOLUME_UP { step } else { -step };
        let _ = volume_tx
            .send(VolumeRequest {
                client_id: volume_player.clone(),
                intent: VolumeIntent::Step(delta),
            })
            .await;
        return;
    }

    let Some(api) = api else { return };
    match api.key(code).await {
        Ok(Some(name)) => info!("beoremote key {code} -> {name}"),
        // Logged with the number in plain sight: this is what someone reads off the screen when a
        // button has to be added to the server's table.
        Ok(None) => info!("beoremote key {code} is not bound to anything"),
        Err(err) => warn!("beoremote key {code} failed: {err:#}"),
    }
}

/// Every `/dev/input/event*` that belongs to the remote.
fn remote_event_devices() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        if device_name(&path)
            .is_some_and(|name| REMOTE_NAMES.iter().any(|prefix| name.starts_with(prefix)))
        {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The device's own name, via `EVIOCGNAME`.
fn device_name(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buffer = [0u8; 256];
    // EVIOCGNAME(len): _IOC(READ, 'E', 0x06, len)
    let request = 0x8000_0000u64 | ((buffer.len() as u64) << 16) | (u64::from(b'E') << 8) | 0x06;
    let read = unsafe { ioctl(file.as_raw_fd(), request, buffer.as_mut_ptr()) };
    if read <= 0 {
        return None;
    }
    let end = usize::try_from(read)
        .ok()?
        .saturating_sub(1)
        .min(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).trim().to_string())
}

/// Read one device on its own thread, sending every press up.
///
/// A thread rather than an async reader: these are blocking character devices, they are idle almost
/// all the time, and there are three of them.
fn spawn_reader(path: PathBuf, keys: mpsc::Sender<Event>) -> Result<()> {
    let mut file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    grab(&file).with_context(|| format!("grab {}", path.display()))?;

    std::thread::spawn(move || {
        let mut buffer = [0u8; EVENT_SIZE * 16];
        let ended = loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => break true,
                Ok(read) => read,
                // The remote slept, or was paired again and its devices were rebuilt; either way the
                // rescan opens whatever the kernel puts there next.
                Err(err) => {
                    debug!("beoremote input {} ended: {err}", path.display());
                    break true;
                }
            };
            for chunk in buffer[..read].chunks_exact(EVENT_SIZE) {
                let event: libc_input_event =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                if event.kind != EV_KEY || event.value != KEY_PRESS {
                    continue;
                }
                if keys.blocking_send(Event::Key(event.code)).is_err() {
                    return;
                }
            }
        };
        if ended {
            let _ = keys.blocking_send(Event::Closed(path));
        }
    });
    Ok(())
}

/// `EVIOCGRAB`: take the device for ourselves.
///
/// Without it the presses reach the console as well, and the standby button is `KEY_POWER` -- which
/// logind will act on, and a Pi that is switched off has no way back but the plug.
fn grab(file: &File) -> Result<()> {
    // EVIOCGRAB: _IOW('E', 0x90, int)
    let request = 0x4000_0000u64 | (4 << 16) | (u64::from(b'E') << 8) | 0x90;
    let result = unsafe { ioctl(file.as_raw_fd(), request, std::ptr::dangling_mut::<u8>()) };
    if result < 0 {
        anyhow::bail!("EVIOCGRAB refused (is another process holding this device?)");
    }
    Ok(())
}

extern "C" {
    fn ioctl(fd: i32, request: u64, arg: *mut u8) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_event_is_the_size_the_kernel_writes() {
        // timeval (two 64-bit words) + type + code + value. Getting this wrong shifts every field
        // and turns key presses into nonsense codes rather than into an error.
        assert_eq!(EVENT_SIZE, 24);
    }

    #[test]
    fn volume_keys_are_the_two_the_kernel_names() {
        assert_eq!(KEY_VOLUME_UP, 115);
        assert_eq!(KEY_VOLUME_DOWN, 114);
    }
}
