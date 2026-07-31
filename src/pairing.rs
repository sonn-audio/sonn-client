//! Pairing a Beoremote One, without a terminal.
//!
//! Pairing normally means SSH-ing in and typing four `bluetoothctl` commands, which is exactly the
//! kind of thing this client exists to remove. The server queues a `pair_remote` command, a window
//! opens here, and the next remote put into pairing mode is paired and trusted.
//!
//! `bluetoothctl` is driven as a child process rather than talking to BlueZ over D-Bus. That is not
//! laziness: bluetoothd refuses to pair without an *agent* registered to answer its questions, and
//! `bluetoothctl` brings one. Implementing an agent ourselves would mean a D-Bus service and a second
//! way for pairing to fail.
//!
//! `trust` is the step that is easy to forget and annoying to debug: without it every reconnect needs
//! re-authorising, so a remote works once and then appears dead.

use crate::models::PairingStatusReport;
use crate::status::Registry;
use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

/// How long to leave the window open. Long enough to walk to the remote and hold its buttons.
const DEFAULT_WINDOW: Duration = Duration::from_secs(90);
/// B&O remotes advertise with this name prefix, which is also what the daemon's legacy-GATT check
/// looks for.
const REMOTE_NAME_PREFIX: &str = "BEORC";

/// Open a pairing window and report what happened.
///
/// `address` pairs one specific device; without it, the first `BEORC*` device that turns up wins.
pub async fn pair_remote(
    statuses: &Registry,
    address: Option<String>,
    window: Option<Duration>,
) -> Result<()> {
    let window = window.unwrap_or(DEFAULT_WINDOW);
    statuses.set_pairing(Some(PairingStatusReport {
        state: "scanning".to_string(),
        address: address.clone(),
        name: None,
        message: None,
    }));
    info!("pairing window open for {}s", window.as_secs());

    let result = tokio::time::timeout(window, run_pairing(address.clone())).await;
    let report = match result {
        Ok(Ok(paired)) => {
            info!("paired {} ({:?})", paired.address, paired.name);
            PairingStatusReport {
                state: "paired".to_string(),
                address: Some(paired.address),
                name: paired.name,
                message: None,
            }
        }
        Ok(Err(err)) => {
            warn!("pairing failed: {:#}", err);
            PairingStatusReport {
                state: "failed".to_string(),
                address,
                name: None,
                message: Some(format!("{:#}", err)),
            }
        }
        Err(_) => {
            warn!("pairing window closed with nothing paired");
            PairingStatusReport {
                state: "timeout".to_string(),
                address,
                name: None,
                message: Some("no remote appeared in time".to_string()),
            }
        }
    };
    let failed = report.state != "paired";
    statuses.set_pairing(Some(report));
    if failed {
        // Best effort: leave the adapter out of discovery mode either way.
        let _ = bluetoothctl(&["scan", "off"]).await;
    }
    Ok(())
}

struct PairedDevice {
    address: String,
    name: Option<String>,
}

async fn run_pairing(address: Option<String>) -> Result<PairedDevice> {
    let target = match address {
        Some(address) => PairedDevice {
            address,
            name: None,
        },
        None => discover_remote().await?,
    };

    // One session for all three steps: the agent `bluetoothctl` registers lives only as long as it
    // runs, and pairing without one fails with "No agent available".
    let script = format!(
        "agent NoInputNoOutput\ndefault-agent\npair {addr}\ntrust {addr}\nconnect {addr}\nquit\n",
        addr = target.address
    );
    let output = feed_bluetoothctl(&script).await?;
    let text = String::from_utf8_lossy(&output);
    if !(text.contains("Pairing successful") || text.contains("already paired")) {
        anyhow::bail!(
            "bluetoothctl did not report a successful pairing: {}",
            last_lines(&text, 5)
        );
    }
    Ok(target)
}

/// Watch for a remote advertising itself and return the first one.
async fn discover_remote() -> Result<PairedDevice> {
    // Scanning and reading the device list in one session: `devices` on a fresh session only shows
    // what is already known, and a remote in pairing mode usually is not.
    let output = feed_bluetoothctl("scan on\n").await?;
    let text = String::from_utf8_lossy(&output);
    for line in text.lines() {
        // "[NEW] Device 48:D0:CF:9D:36:7D BEORC-1234"
        let Some(rest) = line.split("Device ").nth(1) else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let Some(address) = parts.next() else { continue };
        let name = parts.collect::<Vec<_>>().join(" ");
        if name.to_uppercase().starts_with(REMOTE_NAME_PREFIX) {
            return Ok(PairedDevice {
                address: address.to_string(),
                name: (!name.is_empty()).then_some(name),
            });
        }
    }
    anyhow::bail!("no {}* device advertised while scanning", REMOTE_NAME_PREFIX)
}

/// Run `bluetoothctl` with a script on stdin. Scanning has no natural end, so the caller's timeout is
/// what stops it -- killing the child on drop is what makes that safe.
async fn feed_bluetoothctl(script: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("bluetoothctl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start bluetoothctl (is bluez installed?)")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(script.as_bytes())
            .await
            .context("write bluetoothctl script")?;
        stdin.flush().await.ok();
    }
    let output = child
        .wait_with_output()
        .await
        .context("wait for bluetoothctl")?;
    Ok(output.stdout)
}

async fn bluetoothctl(args: &[&str]) -> Result<()> {
    Command::new("bluetoothctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("run bluetoothctl {}", args.join(" ")))?;
    Ok(())
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_of_the_output_is_what_gets_reported() {
        let text = "one\n\ntwo\nthree\nfour\n";
        assert_eq!(last_lines(text, 2), "three | four");
        assert_eq!(last_lines("", 3), "");
    }
}
