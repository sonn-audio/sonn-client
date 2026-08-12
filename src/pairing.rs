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
use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// How long to leave the window open. Long enough to walk to the remote and hold its buttons.
const DEFAULT_WINDOW: Duration = Duration::from_secs(90);
/// B&O remotes advertise with this name prefix, which is also what the daemon's legacy-GATT check
/// looks for.
const REMOTE_NAME_PREFIX: &str = "BEORC";
/// How long to wait for the adapter to answer a pairing request.
const PAIR_TIMEOUT_S: u64 = 30;
/// How long to wait for the first connection. Short: it is a courtesy, not the pairing.
const CONNECT_TIMEOUT_S: u64 = 15;

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

    let result = tokio::time::timeout(window, run_pairing(address.clone(), window)).await;
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

async fn run_pairing(address: Option<String>, window: Duration) -> Result<PairedDevice> {
    let target = match address {
        Some(address) => PairedDevice {
            address,
            name: None,
        },
        None => discover_remote(window).await?,
    };

    // Whatever this adapter thinks it knows about the remote goes first.
    //
    // A bond has two halves, and only one of them is here. Clearing it on the remote -- which is
    // what someone does when pairing stopped working -- leaves this side convinced the two are
    // still paired, so `pair` opens a link the remote refuses, which reads as a failed pairing with
    // nothing to act on. Pressing pair means "start over", so this starts over.
    if let Err(err) = run_bluetoothctl(&["remove".to_string(), target.address.clone()]).await {
        // Not knowing it is the normal case, and the message says so plainly.
        debug!("nothing to forget for {}: {:#}", target.address, err);
    } else {
        info!("forgot the previous pairing for {}", target.address);
    }

    // Three separate one-shot calls with a timeout each, rather than one script.
    //
    // Pairing is asynchronous: bluetoothctl accepts `pair` and the result arrives later, so a script
    // that sends pair, trust, connect and quit back to back can quit before the pairing it asked for
    // has been answered. `--timeout` is what makes each call wait for its own result.
    let addr = target.address.as_str();
    info!("pairing {}", addr);
    let paired = run_bluetoothctl(&[
        "--timeout".to_string(),
        PAIR_TIMEOUT_S.to_string(),
        "pair".to_string(),
        addr.to_string(),
    ])
    .await?;
    if !(paired.contains("Pairing successful") || paired.contains("already paired")) {
        anyhow::bail!(
            "bluetoothctl did not report a successful pairing: {}",
            last_lines(&paired, 5)
        );
    }

    // Without this a remote works exactly once: every reconnect wants authorising again, and
    // nothing on the remote says so -- it simply stops responding after a while.
    let trusted = run_bluetoothctl(&["trust".to_string(), addr.to_string()]).await?;
    if !trusted.contains("trust succeeded") {
        warn!(
            "{} was paired but not trusted: {}",
            addr,
            last_lines(&trusted, 2)
        );
    }

    // Connecting is a courtesy: the remote reconnects by itself once it is trusted, and a failure
    // here is not a failed pairing.
    if let Err(err) = run_bluetoothctl(&[
        "--timeout".to_string(),
        CONNECT_TIMEOUT_S.to_string(),
        "connect".to_string(),
        addr.to_string(),
    ])
    .await
    {
        info!(
            "{} is paired; it will connect when it is used ({:#})",
            addr, err
        );
    }

    Ok(target)
}

/// Watch for a remote advertising itself and return the first one.
async fn discover_remote(window: Duration) -> Result<PairedDevice> {
    // A remote that has been seen before is already in the adapter's list and may not advertise
    // again, so the cheap answer comes first.
    if let Some(found) = find_remote(&run_bluetoothctl(&["devices".to_string()]).await?) {
        info!("{} is already known to the adapter", found.address);
        return Ok(found);
    }

    // `--timeout` rather than feeding `scan on` to a session: bluetoothctl reads its commands from
    // stdin, and closing stdin -- which is what happens the moment the script is written -- makes it
    // exit. That is how this scanned for fourteen milliseconds and reported that nothing was
    // advertising.
    // Half the window, so the pairing that follows has the other half. Scanning for the whole
    // window guarantees the outer timeout fires mid-pair, which is a pairing that reports nothing
    // at all -- the worst of both.
    let seconds = (window.as_secs() / 2).clamp(10, 45);
    info!("scanning {}s for a {}* remote", seconds, REMOTE_NAME_PREFIX);
    let scanned = run_bluetoothctl(&[
        "--timeout".to_string(),
        seconds.to_string(),
        "scan".to_string(),
        "on".to_string(),
    ])
    .await?;

    find_remote(&scanned).ok_or_else(|| {
        anyhow!(
            "no {}* device advertised in {}s -- hold the remote's centre and back buttons until its \
             screen says it is pairing",
            REMOTE_NAME_PREFIX,
            seconds
        )
    })
}

/// Pick a Beoremote out of whatever bluetoothctl printed.
///
/// Both `devices` and a scan use the same line shape, so one parser covers listing and discovery:
/// `[NEW] Device 48:D0:CF:9D:36:7D BEORC-1234`.
fn find_remote(output: &str) -> Option<PairedDevice> {
    for line in output.lines() {
        // `continue`, not `?`: a listing starts with the adapter itself, and giving up on the first
        // line without a device in it is how this found nothing on a scan that saw the remote.
        let Some(rest) = line.split("Device ").nth(1) else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let Some(address) = parts.next() else {
            continue;
        };
        let name = parts.collect::<Vec<_>>().join(" ");
        if name.to_uppercase().starts_with(REMOTE_NAME_PREFIX) {
            return Some(PairedDevice {
                address: address.to_string(),
                name: (!name.is_empty()).then_some(name),
            });
        }
    }
    None
}

/// Run `bluetoothctl` with arguments and return what it printed.
async fn run_bluetoothctl(args: &[String]) -> Result<String> {
    let output = Command::new("bluetoothctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("run bluetoothctl (is bluez installed?)")?;
    if !output.status.success() {
        anyhow::bail!(
            "bluetoothctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_is_found_in_either_listing() {
        // What a scan prints, and what `devices` prints for one already known. Same shape, so one
        // parser covers "it is advertising now" and "we have seen it before".
        let scanned = "[NEW] Controller AA:BB:CC:DD:EE:FF beosound9000\n                       [NEW] Device 11:22:33:44:55:66 Some Phone\n                       [NEW] Device 48:D0:CF:9D:36:7D BEORC-1234\n";
        let found = find_remote(scanned).expect("the remote");
        assert_eq!(found.address, "48:D0:CF:9D:36:7D");
        assert_eq!(found.name.as_deref(), Some("BEORC-1234"));

        let known = "Device 48:D0:CF:9D:36:7D BEORC-1234\n";
        assert_eq!(
            find_remote(known).expect("the remote").address,
            "48:D0:CF:9D:36:7D"
        );

        // Anything else on the air is not a remote, however loudly it advertises.
        assert!(find_remote("[NEW] Device 11:22:33:44:55:66 Some Phone\n").is_none());
        assert!(find_remote("").is_none());
    }

    #[test]
    fn the_tail_of_the_output_is_what_gets_reported() {
        let text = "one\n\ntwo\nthree\nfour\n";
        assert_eq!(last_lines(text, 2), "three | four");
        assert_eq!(last_lines("", 3), "");
    }
}
