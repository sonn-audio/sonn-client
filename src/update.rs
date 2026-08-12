//! Replacing this binary with the one the server asked for.
//!
//! Updating a speaker should not mean finding it on the network and SSH-ing in, which is the same
//! reason the sound card is picked from the server rather than from a config file on the device. So
//! the client is a component like any other: the server names a version, a URL and a hash, and the
//! device fetches it, checks it and swaps itself.
//!
//! The swap is the easy half. The hard half is what happens when the new binary does not come up,
//! because a speaker that fails to start is silent in a room with nobody to read its journal. Three
//! things make that recoverable:
//!
//! - The binary it replaces is kept, right beside it.
//! - A marker records that an update is in flight, and which version it was going to.
//! - The systemd unit counts start attempts against that marker before running anything, and puts
//!   the old binary back when they run out. That check is a shell one-liner on purpose: it has to
//!   work when the new binary is too broken to run at all, which is exactly the case it exists for.
//!
//! The marker is cleared once the new binary has registered with a server, which is the first moment
//! anything can honestly be called working.

use crate::models::{ComponentStatus, DesiredComponent};
use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// The component name the server uses for the client itself.
pub const SONN_CLIENT: &str = "sonn-client";

/// Where the in-flight marker lives. Beside the component state, and on the root filesystem so it
/// survives the reboot it may well be racing.
const STATE_DIR: &str = "/var/lib/sonn-client";
const MARKER: &str = "update-pending";
/// Suffix of the binary kept for the way back.
pub const PREVIOUS_SUFFIX: &str = ".previous";
/// Start attempts the unit allows before it puts the old binary back.
pub const MAX_START_ATTEMPTS: u32 = 3;

/// Whether this component asks for a version other than the one running.
pub fn is_wanted(component: &DesiredComponent) -> bool {
    component
        .version
        .as_deref()
        .map(str::trim)
        .map(|target| target.trim_start_matches('v'))
        .is_some_and(|target| !target.is_empty() && target != env!("CARGO_PKG_VERSION"))
}

/// What the server asked for, compared with what is running.
///
/// Returns a status either way, because "up to date" is worth reporting: it is how the server can
/// show a park of speakers and see that they all took the version it set.
pub async fn reconcile(component: &DesiredComponent, busy: bool) -> ComponentStatus {
    let running = env!("CARGO_PKG_VERSION");
    let Some(target) = component
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return status(crate::components::STATE_INSTALLED, running, None);
    };

    if target.trim_start_matches('v') == running {
        return status(crate::components::STATE_INSTALLED, running, None);
    }

    if busy {
        // Never mid-stream. A speaker that goes quiet in the middle of a record to install
        // something is worse than one that installs it a track later, and the server will ask again
        // on the next poll anyway.
        info!("update to {target} is waiting for this speaker to stop playing");
        return status(crate::components::STATE_INSTALLED, running, None);
    }

    match install(component, target).await {
        // Not reached: `install` replaces this process. Kept honest rather than `unreachable!`.
        Ok(()) => status(crate::components::STATE_INSTALLED, running, None),
        Err(err) => {
            warn!("update to {target} failed: {err:#}");
            status(
                crate::components::STATE_FAILED,
                running,
                Some(format!("{err:#}")),
            )
        }
    }
}

/// Fetch, verify, swap, and hand the process back to systemd.
async fn install(component: &DesiredComponent, target: &str) -> Result<()> {
    let url = component
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("no url for version {}", target))?;
    let expected = component.sha256.as_deref().ok_or_else(|| {
        anyhow!("no sha256 for version {target}; refusing to install unverified code")
    })?;

    info!("updating to {} from {}", target, url);
    let bytes = reqwest::get(url)
        .await
        .context("download update")?
        .error_for_status()
        .context("update download status")?
        .bytes()
        .await
        .context("read update body")?;

    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !digest.eq_ignore_ascii_case(expected) {
        return Err(anyhow!(
            "sha256 mismatch: expected {}, got {}",
            expected,
            digest
        ));
    }

    let current = std::env::current_exe().context("find the running binary")?;
    let staged = staged_path(&current);
    unpack(&bytes, &staged)
        .await
        .with_context(|| format!("unpack into {}", staged.display()))?;

    // Kept before the swap, not after: if anything goes wrong from here on, the way back has to
    // already exist.
    let previous = previous_path(&current);
    fs::copy(&current, &previous)
        .with_context(|| format!("keep the running binary at {}", previous.display()))?;
    arm_marker(target)?;

    fs::rename(&staged, &current)
        .with_context(|| format!("move {} into place", staged.display()))?;
    info!("updated to {}; restarting", target);

    // systemd's `Restart=always` covers a clean exit too, so leaving is the restart. Exiting rather
    // than exec-ing keeps the old process's sockets and sound cards closed before the new one opens
    // them.
    std::process::exit(0);
}

/// Pull `sonn-client` out of the release tarball and make it executable.
///
/// Through `tar` rather than a crate, like the component installer next door: it is one process on a
/// machine that has it, against two dependencies compiled into every build.
async fn unpack(bytes: &[u8], destination: &Path) -> Result<()> {
    let staging = PathBuf::from(STATE_DIR).join("staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    let archive = staging.join("update.tar.gz");
    fs::write(&archive, bytes).with_context(|| format!("write {}", archive.display()))?;

    let output = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .output()
        .await
        .context("run tar")?;
    if !output.status.success() {
        return Err(anyhow!(
            "tar failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let unpacked = staging.join(SONN_CLIENT);
    if !unpacked.exists() {
        return Err(anyhow!("the archive contains no {} binary", SONN_CLIENT));
    }
    fs::set_permissions(&unpacked, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("make {} executable", unpacked.display()))?;
    // Onto the same filesystem as the binary it will replace, because the swap itself has to be a
    // rename: anything that copies leaves a window where the file is half a binary.
    fs::rename(&unpacked, destination)
        .with_context(|| format!("stage {}", destination.display()))?;
    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

/// Note that an update is in flight, and which version it was going to.
fn arm_marker(target: &str) -> Result<()> {
    fs::create_dir_all(STATE_DIR).with_context(|| format!("create {}", STATE_DIR))?;
    // Attempts first, so the unit's counter can read and rewrite the first line without caring what
    // else is here.
    fs::write(marker_path(), format!("0\n{}\n", target))
        .with_context(|| format!("write {}", marker_path().display()))
}

/// Say that this version came up and reached a server.
///
/// Called after registration rather than at startup: a binary that starts and cannot talk to
/// anything is exactly what the way back exists for, and clearing the marker earlier would throw it
/// away before it was needed.
pub fn confirm_started() {
    let path = marker_path();
    if !path.exists() {
        return;
    }
    match fs::remove_file(&path) {
        Ok(()) => {
            info!(
                "update to {} confirmed; the previous binary is no longer needed",
                env!("CARGO_PKG_VERSION")
            );
            if let Ok(current) = std::env::current_exe() {
                let _ = fs::remove_file(previous_path(&current));
            }
        }
        Err(err) => warn!("could not clear the update marker: {err}"),
    }
}

fn marker_path() -> PathBuf {
    PathBuf::from(STATE_DIR).join(MARKER)
}

fn staged_path(current: &Path) -> PathBuf {
    with_suffix(current, ".new")
}

pub fn previous_path(current: &Path) -> PathBuf {
    with_suffix(current, PREVIOUS_SUFFIX)
}

fn with_suffix(current: &Path, suffix: &str) -> PathBuf {
    let mut name = current.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    current.with_file_name(name)
}

fn status(state: &str, version: &str, last_error: Option<String>) -> ComponentStatus {
    ComponentStatus {
        name: SONN_CLIENT.to_string(),
        version: Some(version.to_string()),
        state: state.to_string(),
        last_error,
    }
}

/// Where the guard script lives. Its own file rather than a line in the unit: systemd expands `$`
/// and `%` in command lines itself, so a shell one-liner there is a quoting puzzle that fails
/// silently — and this is the one piece that has to work when everything else does not.
pub const GUARD_PATH: &str = "/usr/local/lib/sonn-client/rollback-guard.sh";

/// The script systemd runs before every start of the client.
///
/// It counts the starts an in-flight update has had and puts the previous binary back when they run
/// out. Deliberately not this program: the case it exists for is a binary that cannot run at all.
pub fn rollback_guard(binary: &Path) -> String {
    let marker = marker_path();
    let previous = previous_path(binary);
    format!(
        r#"#!/bin/sh
# Installed by sonn-client. Runs before every start of the service.
#
# An update leaves a marker naming the version it is moving to. Every start counts against it; when
# the count runs out, the binary that was replaced goes back and the marker is cleared, so the next
# start is an ordinary one. The client removes the marker itself once it has started and reached a
# server, which is the first moment an update can honestly be called successful.
set -eu

marker={marker}
previous={previous}
binary={binary}

[ -f "$marker" ] || exit 0

attempts=$(head -n1 "$marker" 2>/dev/null || echo 0)
attempts=$((attempts + 1))
target=$(sed -n 2p "$marker" 2>/dev/null || echo unknown)

if [ "$attempts" -gt {max} ]; then
    echo "sonn-client $target failed to start $attempts times; restoring the previous binary" >&2
    if [ -f "$previous" ]; then
        mv -f "$previous" "$binary"
    fi
    rm -f "$marker"
else
    printf '%s
%s
' "$attempts" "$target" > "$marker"
fi
"#,
        marker = marker.display(),
        previous = previous.display(),
        binary = binary.display(),
        max = MAX_START_ATTEMPTS,
    )
}

/// Put the guard script in place, so an update has a way back before it is ever needed.
pub fn install_guard(binary: &Path) -> Result<()> {
    let path = PathBuf::from(GUARD_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, rollback_guard(binary))
        .with_context(|| format!("write {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("make {} executable", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kept_binary_sits_beside_the_running_one() {
        let current = Path::new("/usr/local/bin/sonn-client");
        assert_eq!(
            previous_path(current),
            PathBuf::from("/usr/local/bin/sonn-client.previous")
        );
        assert_eq!(
            staged_path(current),
            PathBuf::from("/usr/local/bin/sonn-client.new")
        );
    }

    #[test]
    fn the_guard_is_a_shell_script_that_parses() {
        // The only check there is: nothing else runs this until the moment it has to work, and by
        // then the binary that could have complained is the broken one.
        let dir = std::env::temp_dir().join(format!("sonn-guard-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("rollback-guard.sh");
        fs::write(
            &script,
            rollback_guard(Path::new("/usr/local/bin/sonn-client")),
        )
        .expect("write");

        let checked = std::process::Command::new("sh")
            .arg("-n")
            .arg(&script)
            .output()
            .expect("run sh -n");
        assert!(
            checked.status.success(),
            "guard does not parse: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_guard_restores_without_needing_the_binary_it_guards() {
        let guard = rollback_guard(Path::new("/usr/local/bin/sonn-client"));
        // Everything it does has to be doable by a shell alone: the case it exists for is a binary
        // that cannot run.
        assert!(guard.starts_with("#!/bin/sh"));
        assert!(!guard.contains("sonn-client run"));
        assert!(guard.contains("previous=/usr/local/bin/sonn-client.previous"));
        assert!(guard.contains("update-pending"));
        assert!(guard.contains("-gt 3"));
    }
}
