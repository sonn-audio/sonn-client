//! Sound card enumeration and lookup.
//!
//! The device reports what it has and the server decides which one to use; nothing here chooses. The
//! reported `id` is a cpal device id -- the host prefix followed by the platform name, so on Linux
//! `alsa:hw:CARD=DAC,DEV=0` -- because it has to survive a round trip through the server's config
//! and still resolve months later. Treat it as opaque: it is cpal's spelling, not ALSA's.
//!
//! Everything a device says about itself is best-effort. A card that refuses to enumerate its
//! configs is skipped rather than reported as broken; nothing here may fail the whole listing.

use crate::models::OutputDeviceInfo;
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::BTreeSet;

/// Every output this machine can play through, deduplicated by id.
///
/// Ordering is stable (default first, then by id): cpal's enumeration order is not guaranteed, and a
/// picker that reshuffles itself on every poll is worse than one that is merely long.
pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>> {
    let default_id = cpal::default_host()
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());

    let mut results: Vec<OutputDeviceInfo> = Vec::new();
    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        for device in devices {
            let configs: Vec<_> = match device.supported_output_configs() {
                Ok(configs) => configs.collect(),
                Err(_) => Vec::new(),
            };
            // No output configs means this is a capture-only device.
            if configs.is_empty() {
                continue;
            }
            let Ok(id) = device.id() else {
                continue;
            };
            let id = id.to_string();
            if results.iter().any(|entry| entry.id == id) {
                continue;
            }

            let mut channels = 0u16;
            let mut rates = BTreeSet::new();
            for config in &configs {
                channels = channels.max(config.channels());
                rates.insert(config.min_sample_rate());
                rates.insert(config.max_sample_rate());
            }

            let name = device
                .description()
                .map(|description| description.name().to_string())
                .unwrap_or_else(|_| id.clone());
            results.push(OutputDeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
                channels: (channels > 0).then_some(channels),
                sample_rates: rates.into_iter().collect(),
            });
        }
    }

    results.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(results)
}

/// Every input this machine can capture from, for the source role. Same shape and ordering rules as
/// the output listing: to the server a sound card is a sound card.
pub fn list_input_devices() -> Result<Vec<OutputDeviceInfo>> {
    let default_id = cpal::default_host()
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());

    let mut results: Vec<OutputDeviceInfo> = Vec::new();
    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        for device in devices {
            let configs: Vec<_> = match device.supported_input_configs() {
                Ok(configs) => configs.collect(),
                Err(_) => Vec::new(),
            };
            if configs.is_empty() {
                continue;
            }
            let Ok(id) = device.id() else {
                continue;
            };
            let id = id.to_string();
            if results.iter().any(|entry| entry.id == id) {
                continue;
            }

            let mut channels = 0u16;
            let mut rates = BTreeSet::new();
            for config in &configs {
                channels = channels.max(config.channels());
                rates.insert(config.min_sample_rate());
                rates.insert(config.max_sample_rate());
            }

            let name = device
                .description()
                .map(|description| description.name().to_string())
                .unwrap_or_else(|_| id.clone());
            results.push(OutputDeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
                channels: (channels > 0).then_some(channels),
                sample_rates: rates.into_iter().collect(),
            });
        }
    }

    results.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(results)
}

/// Resolve a capture device the server named.
pub fn find_input_device(query: &str) -> Option<cpal::Device> {
    find_device(query, false)
}

/// Resolve what the server asked for back to a device.
///
/// Accepts a cpal device id (what we reported), a description, or an index into the listing -- the
/// last two so `--audio-device` stays usable by hand while troubleshooting.
pub fn find_output_device(query: &str) -> Option<cpal::Device> {
    find_device(query, true)
}

fn find_device(query: &str, output: bool) -> Option<cpal::Device> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let wanted_index = trimmed.parse::<usize>().ok();
    let mut index = 0usize;

    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        for device in devices {
            let usable = if output {
                device.supports_output()
            } else {
                device.supports_input()
            };
            if !usable {
                continue;
            }
            if let Some(wanted) = wanted_index {
                if index == wanted {
                    return Some(device);
                }
            } else {
                let id_matches = device
                    .id()
                    .map(|id| id.to_string() == trimmed)
                    .unwrap_or(false);
                let description_matches = device
                    .description()
                    .map(|description| description.name() == trimmed)
                    .unwrap_or(false);
                if id_matches || description_matches {
                    return Some(device);
                }
            }
            index += 1;
        }
    }
    None
}

/// Print the listing for `sonn-client devices`, so an installer can see what the server will offer.
pub fn print_devices() -> Result<()> {
    println!("Outputs (players):");
    print_listing(&list_output_devices()?);
    println!("\nInputs (sources):");
    print_listing(&list_input_devices()?);
    println!("\n* = host default. The server picks by id; these are the ids it is offered.");
    Ok(())
}

fn print_listing(devices: &[OutputDeviceInfo]) {
    if devices.is_empty() {
        println!("  none found");
        return;
    }
    for (index, device) in devices.iter().enumerate() {
        let marker = if device.is_default { "*" } else { " " };
        println!("{} [{}] {}", marker, index, device.id);
        println!("       name: {}", device.name);
        if let Some(channels) = device.channels {
            println!("       channels: up to {}", channels);
        }
        if !device.sample_rates.is_empty() {
            let rates = device
                .sample_rates
                .iter()
                .map(|rate| rate.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("       sample rates: {} Hz", rates);
        }
    }
}
