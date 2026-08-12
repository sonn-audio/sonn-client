//! Software this device manages on the server's behalf.
//!
//! There is exactly one so far: `sonn-beoremote`, our build of Bang & Olufsen's patched BlueZ 5.45. It is
//! what turns a Beoremote One from a keyboard into something that serves menus, and it cannot be part
//! of this binary for two reasons that both matter. It is **GPLv2** -- B&O publish their patches
//! because BlueZ leaves them no choice, and linking a GPL daemon into this client would relicense the
//! client. And it is a whole `bluetoothd` that owns the Bluetooth adapter, which most devices running
//! this client have no use for.
//!
//! So it arrives as its own artifact: the server says which version and where, this fetches it,
//! verifies it, installs it and writes its unit. A device with no B&O remote never downloads it.
//!
//! One trap, learned the hard way in the reference install script: **the install prefix is baked into
//! the binary** by `./configure --prefix`, and it is not where the binary ends up. Guessing it wrong
//! is silent -- the daemon starts cleanly, reads no config at all, and stores pairings in a directory
//! nobody looks at, so "the remote pairs but is gone after a reboot". The prefix is therefore read
//! back out of the binary rather than assumed.

use crate::models::{ComponentStatus, DesiredComponent};
use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// The only component this build knows how to install. Anything else is refused rather than guessed
/// at: this installs a daemon that takes over the Bluetooth adapter.
pub const SONN_BEOREMOTE: &str = "sonn-beoremote";

const INSTALL_ROOT: &str = "/opt/beocore";
const STATE_DIR: &str = "/var/lib/sonn-client/components";
const UNIT_PATH: &str = "/etc/systemd/system/sonn-beoremote.service";
const SERVICE_NAME: &str = "sonn-beoremote";

pub const STATE_ABSENT: &str = "absent";
pub const STATE_INSTALLED: &str = "installed";
pub const STATE_RUNNING: &str = "running";
pub const STATE_FAILED: &str = "failed";

/// Bring the installed components in line with what the server asked for.
///
/// Returns the status of everything it knows about, installed or not, so the server can offer (or
/// grey out) the features that depend on them. `busy` says whether this device is playing, which
/// only the client's own update waits for -- nothing else here interrupts audio.
pub async fn reconcile(desired: &[DesiredComponent], busy: bool) -> Vec<ComponentStatus> {
    let mut reports = Vec::new();
    let mut seen = false;

    for component in desired {
        if component.name == crate::update::SONN_CLIENT {
            // The client updating itself is the same shape as installing anything else: a version,
            // a url and a hash. What differs is that it ends by replacing the running process, so
            // it lives in its own module.
            reports.push(crate::update::reconcile(component, busy).await);
            continue;
        }
        if component.name != SONN_BEOREMOTE {
            reports.push(ComponentStatus {
                name: component.name.clone(),
                version: None,
                state: STATE_FAILED.to_string(),
                last_error: Some("unknown component".to_string()),
            });
            continue;
        }
        seen = true;
        reports.push(reconcile_bluetoothd(component).await);
    }

    if !seen {
        // Report it even when nobody asked, so the server can see whether the B&O features are
        // available on this device without having to request an install to find out.
        reports.push(inspect_bluetoothd());
    }
    reports
}

/// Current state of the B&O daemon on this device, without touching anything.
pub fn inspect_bluetoothd() -> ComponentStatus {
    let installed = installed_version(SONN_BEOREMOTE);
    let binary = PathBuf::from(INSTALL_ROOT).join("libexec/bluetoothd");
    if !binary.exists() {
        return ComponentStatus {
            name: SONN_BEOREMOTE.to_string(),
            version: None,
            state: STATE_ABSENT.to_string(),
            last_error: None,
        };
    }
    let state = if service_is_active(SERVICE_NAME) {
        STATE_RUNNING
    } else {
        STATE_INSTALLED
    };
    ComponentStatus {
        name: SONN_BEOREMOTE.to_string(),
        version: installed,
        state: state.to_string(),
        last_error: None,
    }
}

async fn reconcile_bluetoothd(component: &DesiredComponent) -> ComponentStatus {
    if !component.is_enabled() {
        return match remove_bluetoothd() {
            Ok(()) => ComponentStatus {
                name: component.name.clone(),
                version: None,
                state: STATE_ABSENT.to_string(),
                last_error: None,
            },
            Err(err) => failed(&component.name, err),
        };
    }

    let current = inspect_bluetoothd();
    let wanted = component.version.as_deref();
    let up_to_date = current.state != STATE_ABSENT
        && match (wanted, current.version.as_deref()) {
            (Some(wanted), Some(installed)) => wanted == installed,
            // No version named: whatever is installed counts as current. The server can force a
            // reinstall by naming a version.
            (None, _) => true,
            (Some(_), None) => false,
        };

    if up_to_date {
        if current.state == STATE_INSTALLED {
            if let Err(err) = start_service() {
                return failed(&component.name, err);
            }
            return inspect_bluetoothd();
        }
        return current;
    }

    match install_bluetoothd(component).await {
        Ok(status) => status,
        Err(err) => {
            warn!("installing {} failed: {:#}", component.name, err);
            failed(&component.name, err)
        }
    }
}

fn failed(name: &str, err: anyhow::Error) -> ComponentStatus {
    ComponentStatus {
        name: name.to_string(),
        version: None,
        state: STATE_FAILED.to_string(),
        last_error: Some(format!("{:#}", err)),
    }
}

/// Fetch, verify and install the daemon, then write its unit and start it.
async fn install_bluetoothd(component: &DesiredComponent) -> Result<ComponentStatus> {
    let url = component
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("no url for {}", component.name))?;
    let expected = component.sha256.as_deref().ok_or_else(|| {
        anyhow!(
            "no sha256 for {}; refusing to install an unverified daemon",
            component.name
        )
    })?;

    info!("fetching {} from {}", component.name, url);
    let bytes = reqwest::get(url)
        .await
        .context("download component")?
        .error_for_status()
        .context("component download status")?
        .bytes()
        .await
        .context("read component body")?;

    let digest = hex_digest(&bytes);
    if !digest.eq_ignore_ascii_case(expected) {
        return Err(anyhow!(
            "sha256 mismatch: expected {}, got {}",
            expected,
            digest
        ));
    }

    let staging = PathBuf::from(STATE_DIR).join("staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    let archive = staging.join("component.tar.gz");
    fs::write(&archive, &bytes).with_context(|| format!("write {}", archive.display()))?;
    run(
        "tar",
        &[
            "-xzf",
            &archive.to_string_lossy(),
            "-C",
            &staging.to_string_lossy(),
        ],
    )
    .context("unpack component")?;

    let binary = find_file(&staging, "bluetoothd")
        .ok_or_else(|| anyhow!("archive contains no bluetoothd"))?;

    // The prefix the binary was configured with, read back rather than assumed.
    let build_prefix = read_build_prefix(&binary)?;
    info!("component was built with prefix {}", build_prefix.display());

    install_file(
        &binary,
        &PathBuf::from(INSTALL_ROOT).join("libexec/bluetoothd"),
        0o755,
    )?;

    // Storage has to live where the binary was told to look, and it has to be the real one so
    // pairings survive a reboot.
    let storage = build_prefix.join("var/lib");
    fs::create_dir_all(&storage).with_context(|| format!("create {}", storage.display()))?;
    let link = storage.join("bluetooth");
    let _ = fs::remove_dir_all(&link);
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("/var/lib/bluetooth", &link)
        .with_context(|| format!("symlink {}", link.display()))?;

    // The config the adapter needs to present itself as the right kind of product. Without it BlueZ
    // takes its own defaults for Class and DeviceID and a remote sees the wrong thing.
    if let Some(config) = find_file(&staging, "main.conf") {
        install_file(
            &config,
            &build_prefix.join("etc/bluetooth/main.conf"),
            0o644,
        )?;
    }

    fs::write(UNIT_PATH, systemd_unit()).with_context(|| format!("write {}", UNIT_PATH))?;
    // The stock daemon owns the same adapter and the same bus name; both cannot run.
    let _ = run("systemctl", &["disable", "--now", "bluetooth.service"]);
    run("systemctl", &["daemon-reload"])?;
    start_service()?;

    if let Some(version) = component.version.as_deref() {
        record_version(&component.name, version)?;
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(inspect_bluetoothd())
}

fn remove_bluetoothd() -> Result<()> {
    if PathBuf::from(UNIT_PATH).exists() {
        let _ = run("systemctl", &["disable", "--now", SERVICE_NAME]);
        fs::remove_file(UNIT_PATH).with_context(|| format!("remove {}", UNIT_PATH))?;
        run("systemctl", &["daemon-reload"])?;
    }
    let binary = PathBuf::from(INSTALL_ROOT).join("libexec/bluetoothd");
    if binary.exists() {
        fs::remove_file(&binary).with_context(|| format!("remove {}", binary.display()))?;
    }
    let _ = fs::remove_file(version_file(SONN_BEOREMOTE));
    // Deliberately left alone: /var/lib/bluetooth holds the pairings. Removing the daemon should not
    // make the user re-pair a remote when it is put back.
    let _ = run("systemctl", &["enable", "--now", "bluetooth.service"]);
    Ok(())
}

fn start_service() -> Result<()> {
    run("systemctl", &["enable", SERVICE_NAME])?;
    run("systemctl", &["restart", SERVICE_NAME])
}

/// The unit, with the two settings that are not optional.
///
/// `LEGACY_GATT_API_DEVICES=BEORC` is what B&O's patch turned their hardcoded check into. Without it
/// the modern GATT stack handles the remote, the plugin's attribute service is invisible, and the
/// MUSIC menu stays empty with no error anywhere. `Conflicts=bluetooth.service` is the other one:
/// both daemons claim `org.bluez` and the same adapter.
fn systemd_unit() -> String {
    [
        "[Unit]",
        "Description=Sonn BeoRemote One support (B&O's patched BlueZ 5.45, managed by sonn-client)",
        "Documentation=https://github.com/bang-olufsen/gpl",
        "Conflicts=bluetooth.service",
        "After=dbus.service",
        "",
        "[Service]",
        "Type=dbus",
        "BusName=org.bluez",
        "Environment=LEGACY_GATT_API_DEVICES=BEORC",
        "ExecStart=/opt/beocore/libexec/bluetoothd",
        "Restart=on-failure",
        "RestartSec=2",
        "NotifyAccess=main",
        "",
        "[Install]",
        "WantedBy=multi-user.target",
        "Alias=dbus-org.bluez.service",
        "",
    ]
    .join("\n")
}

/// Read `<prefix>/var/lib/bluetooth` out of the binary to recover its configured prefix.
fn read_build_prefix(binary: &Path) -> Result<PathBuf> {
    let mut file = fs::File::open(binary).with_context(|| format!("open {}", binary.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", binary.display()))?;
    const NEEDLE: &[u8] = b"/var/lib/bluetooth";

    for start in 0..bytes.len().saturating_sub(NEEDLE.len()) {
        if &bytes[start..start + NEEDLE.len()] != NEEDLE {
            continue;
        }
        // Walk back to the start of the C string this path sits in.
        let mut begin = start;
        while begin > 0 && bytes[begin - 1] != 0 {
            begin -= 1;
        }
        let text = String::from_utf8_lossy(&bytes[begin..start]).to_string();
        if text.starts_with('/') {
            return Ok(PathBuf::from(text));
        }
        // A bare "/var/lib/bluetooth" means the prefix is the filesystem root.
        if text.is_empty() {
            return Ok(PathBuf::from("/"));
        }
    }
    Err(anyhow!(
        "cannot find the storage path in {}; is this a bluetoothd?",
        binary.display()
    ))
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    let mut directories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            directories.push(path);
        } else if path.file_name().map(|file| file == name).unwrap_or(false) {
            return Some(path);
        }
    }
    directories
        .into_iter()
        .find_map(|directory| find_file(&directory, name))
}

/// Install to a temporary name and rename into place, so a running daemon is never handed a
/// half-written binary.
fn install_file(from: &Path, to: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let staged = to.with_extension("new");
    fs::copy(from, &staged).with_context(|| format!("copy to {}", staged.display()))?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", staged.display()))?;
    fs::rename(&staged, to).with_context(|| format!("install {}", to.display()))?;
    Ok(())
}

fn version_file(name: &str) -> PathBuf {
    PathBuf::from(STATE_DIR).join(format!("{}.version", name))
}

fn record_version(name: &str, version: &str) -> Result<()> {
    let path = version_file(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, version).with_context(|| format!("write {}", path.display()))
}

fn installed_version(name: &str) -> Option<String> {
    fs::read_to_string(version_file(name))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn service_is_active(name: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {} {}", program, args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{} {} failed", program, args.join(" ")))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{:02x}", byte)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_is_lowercase_hex() {
        // Known vector, so a change in how this is computed is caught rather than trusted.
        assert_eq!(
            hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_build_prefix_is_recovered_from_a_nul_terminated_path() {
        let dir = std::env::temp_dir().join("sonn-client-prefix-test");
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("fake-bluetoothd");
        let mut blob = b"\x7fELF junk\0".to_vec();
        blob.extend_from_slice(b"/opt/beocore/var/lib/bluetooth\0");
        blob.extend_from_slice(b"more junk\0");
        fs::write(&file, &blob).unwrap();
        assert_eq!(
            read_build_prefix(&file).unwrap(),
            PathBuf::from("/opt/beocore")
        );
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn a_binary_without_a_storage_path_is_rejected() {
        let dir = std::env::temp_dir().join("sonn-client-prefix-test");
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("not-bluetoothd");
        fs::write(&file, b"nothing to see here").unwrap();
        assert!(read_build_prefix(&file).is_err());
        let _ = fs::remove_file(&file);
    }
}
