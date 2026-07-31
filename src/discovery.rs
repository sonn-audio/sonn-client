//! Finding the audioserver on the LAN.
//!
//! Adapted from `linein-bridge`, which learned the awkward parts the hard way: an mDNS address set
//! is unordered and a server running containers advertises its Docker bridges right next to its LAN
//! address, so "the first IPv4 entry" is a coin flip that can attach a device to the wrong machine.
//! The endpoint paths come from TXT records, with defaults, so a server can move them without a
//! client release.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};
use tracing::warn;

const SERVICE_TYPE: &str = "_sonncore._tcp.local.";
const DEFAULT_REGISTER_PATH: &str = "/api/sonnclients/register";
const DEFAULT_STATUS_PATH: &str = "/api/sonnclients/{device_id}/status";

#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub base_url: String,
    pub register_path: String,
    pub status_path: String,
    pub txt: HashMap<String, String>,
    /// Instance name as advertised, e.g. `Test Audioserver` from
    /// `Test Audioserver._sonncore._tcp.local.` -- what `preferred_server_name` matches against.
    pub instance_name: String,
}

impl DiscoveredServer {
    /// A server named outright in config.toml. Discovery is skipped, but the default endpoint paths
    /// still apply, so a pinned server behaves like a discovered one.
    pub fn from_base_url(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            register_path: DEFAULT_REGISTER_PATH.to_string(),
            status_path: DEFAULT_STATUS_PATH.to_string(),
            txt: HashMap::new(),
            instance_name: String::new(),
        }
    }
}

pub fn discover_server(
    preferred_name: Option<&str>,
    preferred_mac: Option<&str>,
) -> Result<DiscoveredServer> {
    let mdns = ServiceDaemon::new().context("start mDNS daemon")?;
    let receiver = mdns.browse(SERVICE_TYPE).context("browse mDNS services")?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut candidates = Vec::new();

    while Instant::now() < deadline {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(timeout) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let txt = info
                    .get_properties()
                    .iter()
                    .map(|prop| (prop.key().to_string(), prop.val_str().to_string()))
                    .collect::<HashMap<_, _>>();
                let host = resolve_host(info.get_addresses(), info.get_hostname());
                let base_url = format!("http://{}:{}", host, info.get_port());
                let api_prefix = txt
                    .get("api")
                    .cloned()
                    .unwrap_or_else(|| "/api".to_string());
                let register_path = normalize_path(
                    txt.get("client_register")
                        .cloned()
                        .unwrap_or_else(|| format!("{}/sonnclients/register", api_prefix)),
                );
                let status_path = normalize_path(
                    txt.get("client_status")
                        .cloned()
                        .unwrap_or_else(|| format!("{}/sonnclients/{{device_id}}/status", api_prefix)),
                );
                candidates.push(DiscoveredServer {
                    base_url,
                    register_path,
                    status_path,
                    txt,
                    instance_name: instance_name(info.get_fullname()),
                });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    shutdown_mdns(&mdns);
    if candidates.is_empty() {
        anyhow::bail!("no _sonncore._tcp services found");
    }
    Ok(select_server(candidates, preferred_name, preferred_mac))
}

/// Pick a server from what mDNS turned up, honouring the config's preferences.
///
/// Preferences are applied even when only one server answered: with several audioservers on one
/// network, silently attaching to the wrong one is worse than waiting. We still fall through to the
/// first candidate rather than failing, so a device whose preferred server is temporarily down keeps
/// working -- but it says so.
fn select_server(
    mut candidates: Vec<DiscoveredServer>,
    preferred_name: Option<&str>,
    preferred_mac: Option<&str>,
) -> DiscoveredServer {
    if let Some(mac) = preferred_mac.map(normalize_mac).filter(|m| !m.is_empty()) {
        if let Some(server) = candidates
            .iter()
            .find(|server| server.txt.get("mac").map(|v| normalize_mac(v)) == Some(mac.clone()))
        {
            return server.clone();
        }
        warn!(
            "no server advertising mac {}; ignoring preferred_server_mac",
            mac
        );
    }

    if let Some(name) = preferred_name.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(server) = candidates
            .iter()
            .find(|server| server.instance_name.eq_ignore_ascii_case(name))
        {
            return server.clone();
        }
        warn!(
            "no server named {:?}; ignoring preferred_server_name (saw: {})",
            name,
            candidates
                .iter()
                .map(|c| c.instance_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if candidates.len() > 1 {
        warn!(
            "{} servers found and no preference matched; using {}",
            candidates.len(),
            candidates[0].base_url
        );
    }
    candidates.remove(0)
}

/// Compare MAC addresses by their hex digits only: the server advertises `000C290E5497` while a
/// hand-written config says `00:0c:29:0e:54:97`, and both mean the same NIC.
fn normalize_mac(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// `Test Audioserver._sonncore._tcp.local.` -> `Test Audioserver`.
fn instance_name(fullname: &str) -> String {
    match fullname.find("._") {
        Some(idx) => fullname[..idx].to_string(),
        None => fullname.trim_end_matches('.').to_string(),
    }
}

/// How reachable an advertised address looks, lower is better.
fn address_rank(addr: &Ipv4Addr) -> u8 {
    let [a, b, ..] = addr.octets();
    match (a, b) {
        // Docker/Podman defaults. Legitimate LAN space too, so ranked last rather than rejected.
        (172, 16..=31) => 3,
        (169, 254) => 4, // link-local: nothing answered DHCP
        (192, 168) | (10, _) => 0,
        _ => 1,
    }
}

fn resolve_host(addresses: &std::collections::HashSet<IpAddr>, hostname: &str) -> String {
    let mut v4: Vec<Ipv4Addr> = addresses
        .iter()
        .filter_map(|addr| match addr {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .filter(|addr| !addr.is_loopback() && !addr.is_unspecified())
        .collect();
    // Sort by rank, then by value so the choice is stable across restarts even within one rank.
    v4.sort_by_key(|addr| (address_rank(addr), addr.octets()));
    if let Some(addr) = v4.first() {
        return addr.to_string();
    }
    hostname.trim_end_matches('.').to_string()
}

fn normalize_path(path: String) -> String {
    if path.starts_with('/') {
        path
    } else {
        format!("/{}", path)
    }
}

fn shutdown_mdns(mdns: &ServiceDaemon) {
    if let Ok(receiver) = mdns.shutdown() {
        let _ = receiver.recv_timeout(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn server(name: &str, mac: &str, url: &str) -> DiscoveredServer {
        let mut txt = HashMap::new();
        txt.insert("mac".to_string(), mac.to_string());
        DiscoveredServer {
            base_url: url.to_string(),
            register_path: DEFAULT_REGISTER_PATH.to_string(),
            status_path: DEFAULT_STATUS_PATH.to_string(),
            txt,
            instance_name: name.to_string(),
        }
    }

    fn addrs(list: &[&str]) -> HashSet<IpAddr> {
        list.iter().map(|s| s.parse::<IpAddr>().unwrap()).collect()
    }

    #[test]
    fn the_lan_address_beats_container_bridges() {
        let host = resolve_host(
            &addrs(&[
                "172.20.0.1",
                "172.21.0.1",
                "192.168.1.252",
                "172.17.0.1",
                "172.19.0.1",
            ]),
            "server.local.",
        );
        assert_eq!(host, "192.168.1.252");
    }

    #[test]
    fn address_choice_is_stable_within_a_rank() {
        let set = addrs(&["192.168.1.9", "192.168.1.20", "10.0.0.5"]);
        let first = resolve_host(&set, "server.local.");
        for _ in 0..20 {
            assert_eq!(resolve_host(&set, "server.local."), first);
        }
        assert_eq!(first, "10.0.0.5");
    }

    #[test]
    fn falls_back_to_the_hostname_without_usable_addresses() {
        assert_eq!(
            resolve_host(&addrs(&["127.0.0.1"]), "server.local."),
            "server.local"
        );
    }

    #[test]
    fn mac_matching_ignores_separators_and_case() {
        let candidates = vec![
            server("Audioserver", "000C29678C56", "http://192.168.1.252:7090"),
            server(
                "Test Audioserver",
                "000C290E5497",
                "http://192.168.1.209:7090",
            ),
        ];
        let picked = select_server(candidates.clone(), None, Some("00:0c:29:0e:54:97"));
        assert_eq!(picked.base_url, "http://192.168.1.209:7090");
        let picked = select_server(candidates, None, Some("000c290e5497"));
        assert_eq!(picked.base_url, "http://192.168.1.209:7090");
    }

    #[test]
    fn name_matching_uses_the_advertised_instance_name() {
        let candidates = vec![
            server("Audioserver", "000C29678C56", "http://192.168.1.252:7090"),
            server(
                "Test Audioserver",
                "000C290E5497",
                "http://192.168.1.209:7090",
            ),
        ];
        let picked = select_server(candidates, Some("Test Audioserver"), None);
        assert_eq!(picked.base_url, "http://192.168.1.209:7090");
    }

    #[test]
    fn instance_name_is_stripped_of_the_service_suffix() {
        assert_eq!(
            instance_name("Test Audioserver._sonncore._tcp.local."),
            "Test Audioserver"
        );
        assert_eq!(instance_name("plain."), "plain");
    }

    #[test]
    fn a_pinned_server_gets_the_default_paths() {
        let server = DiscoveredServer::from_base_url("http://192.168.1.209:7090/");
        assert_eq!(server.base_url, "http://192.168.1.209:7090");
        assert_eq!(server.register_path, DEFAULT_REGISTER_PATH);
    }
}
