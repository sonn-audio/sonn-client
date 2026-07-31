//! `sonn-client install`: make this a service and get out of the way.

use crate::config;
use anyhow::{Context, Result};
use std::fs;
use std::process::Command;

const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/sonn-client.service";
const SERVICE_NAME: &str = "sonn-client";

pub async fn run_install() -> Result<()> {
    let (config, config_path) = config::load_or_create_config()?;
    println!("Device id: {}", config.device_id);
    println!("Config:    {}", config_path.display());

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
fn systemd_unit() -> String {
    [
        "[Unit]",
        "Description=Sonn Client (Sendspin audio endpoint)",
        "After=network-online.target sound.target",
        "Wants=network-online.target",
        "",
        "[Service]",
        "Type=simple",
        "ExecStart=/usr/local/bin/sonn-client run",
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
