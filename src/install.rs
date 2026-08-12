//! `sonn-client install`: make this a service and get out of the way.

use crate::config;
use crate::update;
use anyhow::{Context, Result};
use std::fs;
use std::process::Command;

const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/sonn-client.service";
/// Where the installer puts the binary, and therefore what the unit and the rollback address.
const BINARY_PATH: &str = "/usr/local/bin/sonn-client";
const SERVICE_NAME: &str = "sonn-client";

pub async fn run_install() -> Result<()> {
    let (config, config_path) = config::load_or_create_config()?;
    println!("Device id: {}", config.device_id);
    println!("Config:    {}", config_path.display());

    update::install_guard(std::path::Path::new(BINARY_PATH))
        .context("install the update rollback guard")?;
    println!("Guard:     {}", update::GUARD_PATH);

    fs::write(SYSTEMD_UNIT_PATH, systemd_unit())
        .with_context(|| format!("write {} (run as root, or use sudo)", SYSTEMD_UNIT_PATH))?;
    println!("Unit:      {}", SYSTEMD_UNIT_PATH);

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", SERVICE_NAME])?;
    run_systemctl(&["restart", SERVICE_NAME])?;
    println!(
        "\n{} is running. It will find the audioserver over mDNS and show up there as a client;\n\
         pick its sound card and assign it to a zone from the server's admin UI.",
        SERVICE_NAME
    );
    Ok(())
}

/// The scheduling settings are the point of the unit: audio is handed to the card against a shared
/// clock, and a player that misses its wakeup is audible.
///
/// The `ExecStartPre` is the other half of updating in place. It counts the starts an in-flight
/// update has had and puts the previous binary back when they run out, and it is a shell line rather
/// than a call into this program because the only situation it exists for is a binary that will not
/// run.
fn systemd_unit() -> String {
    [
        "[Unit]",
        "Description=Sonn Client (Sendspin audio endpoint)",
        "After=network-online.target sound.target",
        "Wants=network-online.target",
        "",
        "[Service]",
        "Type=simple",
        &format!("ExecStartPre={}", crate::update::GUARD_PATH),
        &format!("ExecStart={} run", BINARY_PATH),
        "Nice=-5",
        "CPUSchedulingPolicy=rr",
        "CPUSchedulingPriority=50",
        "LimitRTPRIO=99",
        "Restart=always",
        "RestartSec=2",
        "",
        "[Install]",
        "WantedBy=multi-user.target",
        "",
    ]
    .join("\n")
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("run systemctl {}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("systemctl {} failed", args.join(" "));
    }
}
