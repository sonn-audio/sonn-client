//! Who and what this device is, as far as the server needs to know.
//!
//! All of it is best-effort: a missing MAC or an unreadable model string is worth a `None` in a
//! client list, never a failed startup.

use std::fs;

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub hostname: String,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub model: Option<String>,
    pub os: Option<String>,
    /// What the server picks a component build by.
    pub arch: String,
}

pub fn collect() -> DeviceIdentity {
    DeviceIdentity {
        hostname: hostname::get()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        ip: primary_ipv4(),
        mac: primary_mac(),
        model: hardware_model(),
        os: os_description(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

fn primary_ipv4() -> Option<String> {
    let ifaces = get_if_addrs::get_if_addrs().ok()?;
    ifaces.into_iter().find_map(|iface| {
        if iface.is_loopback() {
            return None;
        }
        match iface.ip() {
            std::net::IpAddr::V4(addr) => Some(addr.to_string()),
            std::net::IpAddr::V6(_) => None,
        }
    })
}

fn primary_mac() -> Option<String> {
    mac_address::get_mac_address()
        .ok()
        .flatten()
        .map(|mac| mac.to_string().to_uppercase())
}

/// The device-tree model on a Pi or comparable SBC; nothing on a generic PC.
fn hardware_model() -> Option<String> {
    for path in [
        "/proc/device-tree/model",
        "/sys/firmware/devicetree/base/model",
        "/sys/devices/virtual/dmi/id/product_name",
    ] {
        if let Ok(raw) = fs::read(path) {
            // Device-tree strings are NUL-terminated.
            let text = String::from_utf8_lossy(&raw)
                .trim_end_matches('\0')
                .trim()
                .to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn os_description() -> Option<String> {
    let release = fs::read_to_string("/etc/os-release").ok()?;
    let pretty = release.lines().find_map(|line| {
        line.strip_prefix("PRETTY_NAME=")
            .map(|value| value.trim_matches('"').to_string())
    })?;
    Some(format!("{} {}", pretty, std::env::consts::ARCH))
}
