//! Hardware volume through the sound card's own mixer.
//!
//! A speaker with real volume of its own should use it. Attenuating in software costs bits that the
//! card would not have cost, and on a device like a B&O BeoLab over USB the mixer *is* the speaker's
//! volume -- what the display shows and what the remote moves. So when a card has a playback volume
//! element, that is where volume goes, and the software mixer stays at unity.
//!
//! The one subtlety is which scale to address it on, and it is the subtlety that made the reference
//! client wrong on this hardware. `amixer -M` spreads a percentage perceptually across the raw
//! register range, which is right for the DAC HATs whose mixers are linear in register steps --
//! without it, 50% lands halfway down the register and sounds far quieter than expected. It is wrong
//! for a mixer already calibrated in dB, where one step is one dB: there the hardware does the
//! perceptual mapping itself, and `-M` lays a second curve on top, so a percentage no longer
//! corresponds to a known attenuation. Measured on a BeoLab: 30% is -63 dB with `-M` and -30 dB
//! without.
//!
//! Rather than ask, this reads the mixer and works it out -- a card whose current level in dB equals
//! its distance from the top in steps is calibrated in dB. The server can still say outright, for
//! the cards that cannot be read.

use crate::models::DesiredPlayer;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Elements worth trying first, in the order the reference client tries them.
const PREFERRED_ELEMENTS: [&str; 3] = ["Digital", "Master", "PCM"];
/// How close the dB reading has to be to one-dB-per-step to count as calibrated.
const DB_PER_STEP_TOLERANCE: f32 = 0.5;

/// A card's playback volume element, and how to address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlsaMixer {
    /// Card as ALSA names it, e.g. `CDCACM`. Passed to `amixer -c`.
    card: String,
    element: String,
    /// Whether to let `amixer` map percentages perceptually (`-M`).
    mapped: bool,
}

impl AlsaMixer {
    /// Find the mixer for the card a player was given, if it has one.
    ///
    /// `None` means software volume: no card in the id, no mixer, no volume element, or no `amixer`.
    /// Every one of those is a normal machine, not a fault.
    pub async fn discover(player: &DesiredPlayer) -> Option<Self> {
        let card = card_of(player.output.as_deref()?)?;
        let element = match player.mixer_element.as_deref().map(str::trim) {
            Some(element) if !element.is_empty() => element.to_string(),
            _ => pick_element(&card).await?,
        };

        let mapped = match player.mixer_mapped {
            Some(mapped) => {
                debug!(
                    "mixer {}:{} mapping set by the server: {}",
                    card, element, mapped
                );
                mapped
            }
            None => detect_mapping(&card, &element).await,
        };
        info!(
            "hardware volume on {}:{} ({})",
            card,
            element,
            if mapped {
                "percentages mapped perceptually"
            } else {
                "mixer is calibrated in dB, addressed directly"
            }
        );
        Some(Self {
            card,
            element,
            mapped,
        })
    }

    /// Apply level and mute, the way the reference client does: muted is simply 0.
    pub async fn apply(&self, volume: u8, muted: bool) {
        let effective = if muted { 0 } else { volume.min(100) };
        let mut args: Vec<String> = Vec::new();
        if self.mapped {
            args.push("-M".to_string());
        }
        args.extend([
            "-c".to_string(),
            self.card.clone(),
            "sset".to_string(),
            self.element.clone(),
            format!("{}%", effective),
            if muted { "mute" } else { "unmute" }.to_string(),
        ]);

        match run(&args).await {
            Some(_) => debug!("{}:{} set to {}%", self.card, self.element, effective),
            None => warn!(
                "could not set {}:{} to {}%; the level on the card is now unknown",
                self.card, self.element, effective
            ),
        }
    }
}

/// `alsa:hw:CARD=CDCACM,DEV=0` -> `CDCACM`.
///
/// Only the by-name spelling is accepted. A number would name whichever card ALSA happened to
/// enumerate first this boot, and setting the volume of the wrong card is worse than not setting it.
fn card_of(device_id: &str) -> Option<String> {
    let card = device_id.split_once("CARD=")?.1;
    let card = card.split(',').next().unwrap_or(card).trim();
    if card.is_empty() || card.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(card.to_string())
}

/// The element to drive, preferring the names that are conventionally the main output.
async fn pick_element(card: &str) -> Option<String> {
    let output = run(&["-c".to_string(), card.to_string(), "scontrols".to_string()]).await?;

    // "Simple mixer control 'PCM',0"
    let names: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_once('\'')?.1.rsplit_once('\''))
        .map(|(name, _)| name.to_string())
        .collect();

    for preferred in PREFERRED_ELEMENTS {
        if let Some(name) = names.iter().find(|name| name.as_str() == preferred) {
            return Some(name.clone());
        }
    }
    // Anything with a playback volume will do; a card with one element usually calls it something
    // of its own.
    for name in &names {
        if read_element(card, name).await.is_some() {
            return Some(name.clone());
        }
    }
    debug!("{} has no mixer element with a playback volume", card);
    None
}

/// Whether percentages should be mapped perceptually for this element.
///
/// True unless the mixer turns out to be calibrated in dB, which is what the reference client
/// assumes and what most HAT mixers are.
async fn detect_mapping(card: &str, element: &str) -> bool {
    let Some(reading) = read_element(card, element).await else {
        debug!(
            "{}:{} could not be read; mapping percentages",
            card, element
        );
        return true;
    };

    let calibrated = is_calibrated(&reading);
    debug!(
        "{}:{} is at {} of {} ({}): {}",
        card,
        element,
        reading.value,
        reading.max,
        reading
            .decibels
            .map(|db| format!("{db} dB"))
            .unwrap_or_else(|| "no dB reported".to_string()),
        if calibrated {
            "one step is one dB"
        } else {
            "steps are not dB"
        }
    );
    !calibrated
}

/// Whether one step of this mixer is one dB.
///
/// True when the level, counted down from the top of the range, is the level in dB. At the very top
/// both are zero, which is true of every mixer and says nothing, so that reading decides nothing and
/// the mapped default stands until the level is somewhere it can be read.
fn is_calibrated(reading: &Reading) -> bool {
    let steps_below_max = reading.max.saturating_sub(reading.value);
    if steps_below_max == 0 {
        return false;
    }
    match reading.decibels {
        Some(decibels) => (decibels + steps_below_max as f32).abs() <= DB_PER_STEP_TOLERANCE,
        None => false,
    }
}

#[derive(Debug, PartialEq)]
struct Reading {
    min: u32,
    max: u32,
    value: u32,
    decibels: Option<f32>,
}

async fn read_element(card: &str, element: &str) -> Option<Reading> {
    let output = run(&[
        "-c".to_string(),
        card.to_string(),
        "sget".to_string(),
        element.to_string(),
    ])
    .await?;
    parse_reading(&output)
}

/// Pull the numbers out of `amixer sget`:
///
/// ```text
///   Limits: Playback 0 - 90
///   Front Left: Playback 45 [50%] [-45.00dB] [on]
/// ```
fn parse_reading(output: &str) -> Option<Reading> {
    let mut min_max: Option<(u32, u32)> = None;
    let mut value: Option<u32> = None;
    let mut decibels: Option<f32> = None;

    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Limits: Playback ") {
            let (low, high) = rest.split_once(" - ")?;
            min_max = Some((low.trim().parse().ok()?, high.trim().parse().ok()?));
            continue;
        }
        // The first channel is enough: every channel is set to the same level anyway.
        if value.is_none() && line.contains('[') {
            if let Some((_, rest)) = line.split_once("Playback ") {
                value = rest.split_whitespace().next().and_then(|v| v.parse().ok());
                // The bracketed parts are the level as a percentage, in dB, and the switch state.
                decibels = rest
                    .split('[')
                    .filter_map(|part| part.split(']').next())
                    .find_map(|part| part.strip_suffix("dB"))
                    .and_then(|part| part.trim().parse::<f32>().ok());
            }
        }
    }

    let (min, max) = min_max?;
    Some(Reading {
        min,
        max,
        value: value?,
        decibels,
    })
}

/// Run `amixer` and return its output, or `None` if it is missing or refused.
async fn run(args: &[String]) -> Option<String> {
    let output = Command::new("amixer")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        debug!(
            "amixer {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_named_card_is_accepted() {
        assert_eq!(
            card_of("alsa:hw:CARD=CDCACM,DEV=0").as_deref(),
            Some("CDCACM")
        );
        assert_eq!(
            card_of("alsa:plughw:CARD=sndrpihifiberry,DEV=0").as_deref(),
            Some("sndrpihifiberry")
        );
        // A number names whichever card enumerated first this boot. Setting the volume of the wrong
        // card is worse than leaving it in software.
        assert_eq!(card_of("alsa:hw:CARD=3,DEV=0"), None);
        assert_eq!(card_of("alsa:null"), None);
    }

    #[test]
    fn a_mixer_reading_is_read_out_of_amixer() {
        let output = "Simple mixer control 'PCM',0\n  \
             Capabilities: pvolume pswitch\n  \
             Playback channels: Front Left - Front Right\n  \
             Limits: Playback 0 - 90\n  \
             Front Left: Playback 45 [50%] [-45.00dB] [on]\n  \
             Front Right: Playback 45 [50%] [-45.00dB] [on]\n";
        let reading = parse_reading(output).expect("reading");
        assert_eq!(reading.min, 0);
        assert_eq!(reading.max, 90);
        assert_eq!(reading.value, 45);
        assert_eq!(reading.decibels, Some(-45.0));
    }

    #[test]
    fn a_db_calibrated_mixer_is_addressed_directly() {
        // A BeoLab over USB: 0-90 spanning -90..0 dB, so 45 steps below the top is -45 dB. Mapping
        // percentages here lays a second curve on the one the hardware already applies -- 30% became
        // -63 dB instead of -30 dB.
        let beolab = Reading {
            min: 0,
            max: 90,
            value: 45,
            decibels: Some(-45.0),
        };
        assert!(is_calibrated(&beolab), "one step is one dB");

        // A HAT whose mixer is linear in register steps: the same position is far further down in dB.
        let hat = Reading {
            min: 0,
            max: 207,
            value: 100,
            decibels: Some(-53.5),
        };
        assert!(!is_calibrated(&hat));

        // At the top, both readings are zero whatever the mixer is, so nothing can be concluded.
        let at_max = Reading {
            min: 0,
            max: 90,
            value: 90,
            decibels: Some(0.0),
        };
        assert!(!is_calibrated(&at_max));
    }
}
