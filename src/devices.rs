//! Sound card enumeration and lookup.
//!
//! The device reports what it has and the server decides which one to use; nothing here chooses. The
//! reported `id` is a cpal device id -- the host prefix followed by the platform name, so on Linux
//! `alsa:hw:CARD=DAC,DEV=0` -- because it has to survive a round trip through the server's config
//! and still resolve months later. Treat it as opaque: it is cpal's spelling, not ALSA's.
//!
//! Everything a device says about itself is best-effort. A card that refuses to enumerate its
//! configs is skipped rather than reported as broken; nothing here may fail the whole listing. What
//! it does do is say *why* at debug level -- a card that quietly fails to appear is otherwise only
//! diagnosable by SSH-ing in, which is what this client exists to avoid.

use crate::models::OutputDeviceInfo;
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::BTreeSet;
use tracing::debug;

/// Rates worth naming in a picker.
///
/// cpal reports a card's *range*, and a plugin device answers with the whole span it is willing to
/// resample -- 4000 Hz to 4294967295 Hz, which is true and useless. Reporting the standard rates
/// that fall inside the range says the same thing in the language the server and the user think in.
const STANDARD_RATES: [u32; 12] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    384_000,
];

/// ALSA's discard device. Enumerated on every Linux box, useful to nobody picking a speaker.
const NULL_DEVICE_SUFFIX: &str = ":null";

/// Every output this machine can play through, ranked for a picker.
pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>> {
    Ok(list_devices(Direction::Output))
}

/// Every input this machine can capture from, for the source role. Same shape and ordering rules as
/// the output listing: to the server a sound card is a sound card.
pub fn list_input_devices() -> Result<Vec<OutputDeviceInfo>> {
    Ok(list_devices(Direction::Input))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Output,
    Input,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::Output => "output",
            Direction::Input => "input",
        }
    }
}

/// Enumerate one direction, deduplicated by id and ranked.
///
/// Ordering is stable: cpal's enumeration order is not guaranteed, and a picker that reshuffles
/// itself on every poll is worse than one that is merely long.
fn list_devices(direction: Direction) -> Vec<OutputDeviceInfo> {
    let default_id = default_device_id(direction);
    let mut results: Vec<OutputDeviceInfo> = Vec::new();

    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.devices() else {
            continue;
        };
        for device in devices {
            let Ok(id) = device.id() else {
                continue;
            };
            let id = id.to_string();
            if id.ends_with(NULL_DEVICE_SUFFIX) {
                continue;
            }
            if results.iter().any(|entry| entry.id == id) {
                continue;
            }

            let configs = match direction {
                Direction::Output => device.supported_output_configs().map(|c| c.collect()),
                Direction::Input => device.supported_input_configs().map(|c| c.collect()),
            };
            // A card someone is already playing through. `hw:` devices are exclusive, so this is the
            // normal state of the very card this client was given -- and dropping it from the listing
            // makes the server report the speaker it is using as "not currently connected". It is
            // reported without its formats rather than not at all.
            let mut in_use = false;
            let configs: Vec<_> = match configs {
                Ok(configs) => configs,
                Err(err) if is_busy(&err) => {
                    debug!("{} is in use; listing it without its formats", id);
                    in_use = true;
                    Vec::new()
                }
                Err(err) => {
                    debug!("{} skipped for {}: {}", id, direction.label(), err);
                    continue;
                }
            };
            if configs.is_empty() && !in_use {
                // Not an error: this is simply a device for the other direction.
                debug!("{} has no {} configs", id, direction.label());
                continue;
            }

            let mut channels = 0u16;
            let mut rates = BTreeSet::new();
            for config in &configs {
                channels = channels.max(config.channels());
                let (min, max) = (config.min_sample_rate(), config.max_sample_rate());
                rates.extend(
                    STANDARD_RATES
                        .iter()
                        .copied()
                        .filter(|rate| (min..=max).contains(rate)),
                );
                if !STANDARD_RATES.iter().any(|rate| (min..=max).contains(rate)) {
                    // A card nobody has ever seen, reported as-is rather than as nothing.
                    rates.insert(min);
                    rates.insert(max);
                }
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

    drop_numeric_card_duplicates(&mut results);
    results.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| rank(&a.id).cmp(&rank(&b.id)))
            .then_with(|| a.id.cmp(&b.id))
    });
    results
}

fn default_device_id(direction: Direction) -> Option<String> {
    let host = cpal::default_host();
    let device = match direction {
        Direction::Output => host.default_output_device(),
        Direction::Input => host.default_input_device(),
    }?;
    device.id().ok().map(|id| id.to_string())
}

/// Where an id sorts in a picker: the card itself first, then the converting alias, then whatever
/// else ALSA's configuration invents. All of it stays available -- a HAT with an unusual format may
/// well need `plughw` -- but the entry someone actually wants is the one they see first.
fn rank(id: &str) -> u8 {
    let name = id.split_once(':').map(|(_, rest)| rest).unwrap_or(id);
    if name.starts_with("hw:") {
        0
    } else if name.starts_with("plughw:") {
        1
    } else if name.starts_with("sysdefault:") || name.starts_with("default") {
        2
    } else {
        3
    }
}

/// Whether an enumeration failure means "in use" rather than "not that kind of device".
///
/// Matched on the message because that is where ALSA's `EBUSY` ends up by the time cpal has wrapped
/// it: the backend-specific error carries the text and nothing structured. Getting this wrong in one
/// direction lists a card that cannot be used; in the other it hides a card that is working.
fn is_busy<E: std::fmt::Display>(err: &E) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("busy") || text.contains("ebusy")
}

fn is_numeric(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_digit())
}

/// The card id ALSA was addressed by, e.g. `CDCACM` in `alsa:hw:CARD=CDCACM,DEV=0`.
fn card_token(id: &str) -> Option<&str> {
    let rest = id.split_once("CARD=")?.1;
    Some(rest.split(',').next().unwrap_or(rest))
}

/// Drop `CARD=<number>` entries whose card also appears by name.
///
/// ALSA enumerates both forms for the same card. The number is not stable: it follows probe order,
/// so a USB speaker that is card 3 today is card 1 after a reboot with one device fewer -- and a
/// zone that stored the number would then quietly play out of a different card. The name is the
/// robust form, so it is the only one offered when both exist.
fn drop_numeric_card_duplicates(devices: &mut Vec<OutputDeviceInfo>) {
    // Collected up front: which (description, spelling) pairs are available by name. The description
    // is the one thing both spellings of a card agree on, and the rank keeps `hw:` from standing in
    // for `plughw:`.
    let named: Vec<(String, u8)> = devices
        .iter()
        .filter(|device| card_token(&device.id).is_some_and(|token| !is_numeric(token)))
        .map(|device| (device.name.clone(), rank(&device.id)))
        .collect();
    if named.is_empty() {
        return;
    }

    devices.retain(|device| {
        let Some(token) = card_token(&device.id) else {
            return true;
        };
        if !is_numeric(token) {
            return true;
        }
        let has_named_twin = named
            .iter()
            .any(|(name, spelling)| name == &device.name && *spelling == rank(&device.id));
        if has_named_twin {
            debug!("{} hidden: the same card is offered by name", device.id);
        }
        !has_named_twin
    });
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
    println!("Run with --log-level debug to see the devices that were left out, and why.");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> OutputDeviceInfo {
        OutputDeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            channels: Some(2),
            sample_rates: vec![48_000],
            is_default: false,
        }
    }

    #[test]
    fn a_card_is_offered_by_name_not_by_number() {
        const HIFIBERRY: &str = "snd_rpi_hifiberry_dacplusadcpro, HiFiBerry DAC+ADC PRO";
        let mut devices = vec![
            device("alsa:hw:CARD=2,DEV=0", HIFIBERRY),
            device("alsa:hw:CARD=sndrpihifiberry,DEV=0", HIFIBERRY),
            device("alsa:plughw:CARD=2,DEV=0", HIFIBERRY),
            device("alsa:plughw:CARD=sndrpihifiberry,DEV=0", HIFIBERRY),
        ];
        drop_numeric_card_duplicates(&mut devices);

        let ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "alsa:hw:CARD=sndrpihifiberry,DEV=0",
                "alsa:plughw:CARD=sndrpihifiberry,DEV=0"
            ],
            "the numbers move across reboots; the names do not"
        );
    }

    #[test]
    fn a_card_that_is_being_played_through_is_still_a_card() {
        // ALSA's EBUSY, as cpal hands it over. The card this client was told to use is exclusive
        // while it plays, so treating "busy" as "gone" makes a working speaker report itself as
        // missing the next time it reconnects.
        assert!(is_busy(
            &"ALSA function 'snd_pcm_open' failed: Device or resource busy"
        ));
        assert!(is_busy(&"EBUSY"));
        // Everything else is a device for the other direction, or no device at all.
        assert!(!is_busy(&"The dmix plugin supports only playback stream"));
        assert!(!is_busy(&"No such file or directory"));
    }

    #[test]
    fn a_card_that_only_has_a_number_is_still_offered() {
        let mut devices = vec![device("alsa:hw:CARD=1,DEV=0", "Nameless USB thing")];
        drop_numeric_card_duplicates(&mut devices);
        assert_eq!(devices.len(), 1, "offering nothing would be worse");
    }

    #[test]
    fn the_real_card_sorts_above_the_aliases() {
        let mut ids = [
            "alsa:sysdefault:CARD=DAC",
            "alsa:dmix:CARD=DAC,DEV=0",
            "alsa:hw:CARD=DAC,DEV=0",
            "alsa:plughw:CARD=DAC,DEV=0",
        ];
        ids.sort_by_key(|id| rank(id));
        assert_eq!(ids[0], "alsa:hw:CARD=DAC,DEV=0");
        assert_eq!(ids[1], "alsa:plughw:CARD=DAC,DEV=0");
    }
}
