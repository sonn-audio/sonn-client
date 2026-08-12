//! Turning what the server asked for into a Sendspin player.
//!
//! The session is the crate's. `sendspin::player::Player` owns the connection, the decoder, the
//! card and the drift correction between them, and it reconnects on its own. What is left here is
//! the half only this device can answer -- which card, whose volume, what the server decided to
//! call this speaker -- and the bridge that turns what the player reports about itself into what
//! this device reports upstream.
//!
//! What used to be here was a second implementation of the protocol living beside the crate's. It
//! agreed with the server it was written against and with nothing else, which is exactly the kind
//! of fault that stays invisible until there is another implementation to test against.

use crate::models::{DesiredPlayer, VolumeControl};
use crate::status::{PlayerHandle, STATE_CONNECTED, STATE_CONNECTING, STATE_IDLE, STATE_STREAMING};
use anyhow::{anyhow, Result};
use sendspin::audio::devices::{find_device, output_rates};
use sendspin::audio::Codec;
use sendspin::hooks::Hooks;
use sendspin::player::{CodecOffer, ConnectionState, Player, PlayerConfig, PlayerStatus};
use sendspin::protocol::messages::AudioFormatSpec;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

/// Fallback when the server seeds no level: full, and let the first server command decide.
const DEFAULT_VOLUME: u8 = 100;
/// Channels to assume when a format is pinned without naming them. Stereo, like everything else.
const DEFAULT_CHANNELS: u8 = 2;
/// How long to wait before dialling again. The crate doubles this up to a minute.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// What can change without rebuilding the player.
///
/// The level is here as well as in `client/hello`, because two things move it locally: the server
/// changing the seed it configured, and a press on the remote wired to this speaker. Both go to the
/// player the same way, and the crate decides where a level actually lands -- a script, the card's
/// mixer, or its own gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSettings {
    pub volume: u8,
    pub muted: bool,
    pub static_delay_ms: u16,
}

impl LiveSettings {
    pub fn from_desired(player: &DesiredPlayer) -> Self {
        Self {
            volume: player.volume.unwrap_or(DEFAULT_VOLUME).min(100),
            muted: player.muted.unwrap_or(false),
            static_delay_ms: player.static_delay_ms.unwrap_or(0),
        }
    }
}

/// What this build can decode, for the capabilities this device reports to its server.
pub fn supported_codecs() -> Vec<String> {
    let mut codecs: Vec<String> = Vec::new();
    for offer in CodecOffer::all() {
        if !codecs.contains(&offer.codec) {
            codecs.push(offer.codec);
        }
    }
    codecs
}

/// Build a player for one desired speaker.
///
/// Fails only where the device cannot do what was asked -- a named card that is not there. A missing
/// mixer or an unusable hook is reported and stepped over, because software volume still plays.
pub fn build(
    desired: &DesiredPlayer,
    name: String,
    fallback_hook: Option<&str>,
    settings: &LiveSettings,
) -> Result<Player> {
    let mut config = PlayerConfig::new(desired.client_id.clone(), name);

    if let Some(id) = desired
        .output
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let device = find_device(id).map_err(|err| anyhow!("sound card {}: {}", id, err))?;
        config.rates = output_rates(Some(&device));
        config.device = Some(device);
    }

    config.static_delay = settings.static_delay_ms;
    config.initial_volume = settings.volume;
    config.initial_muted = settings.muted;
    config.buffer_ms = desired.buffer_ms;
    config.required_lead_time_ms = desired.required_lead_time_ms;
    config.format = pinned_format(desired);
    if let Some(codecs) = offered_codecs(desired) {
        config.codecs = codecs;
    }

    let control = desired.volume_control();
    // A hook is a deliberate act, so it wins over a mixer we merely found.
    let hook = match control {
        VolumeControl::Software | VolumeControl::Alsa => None,
        VolumeControl::Hook | VolumeControl::Auto => desired
            .volume_hook
            .as_deref()
            .or(fallback_hook)
            .map(str::trim)
            .filter(|command| !command.is_empty()),
    };
    if matches!(control, VolumeControl::Hook) && hook.is_none() {
        warn!("volume_control is hook but no volume_hook was given; using software gain");
    }
    config.hooks = Hooks::new(None, None, hook).map_err(|err| anyhow!("volume hook: {}", err))?;

    if desired.mixer_element.is_some() {
        // The crate picks the element from the card's own list, and there is no way to name one.
        // Saying so beats a setting that reads as configuration and is not.
        warn!("mixer_element is set but the card's mixer element is chosen by the client");
    }

    #[cfg(target_os = "linux")]
    if hook.is_none() && matches!(control, VolumeControl::Alsa | VolumeControl::Auto) {
        config.mixer = open_mixer(desired.output.as_deref(), control, mixer_scale(desired));
    }

    Ok(Player::new(config))
}

/// The card's own volume control, where one is wanted and there is one.
///
/// Never fatal: a card with no gain stage is ordinary, and software volume still plays. Said out
/// loud only when the server asked for the mixer outright, since that is the case where silence
/// about it would be misleading.
#[cfg(target_os = "linux")]
fn open_mixer(
    output: Option<&str>,
    control: VolumeControl,
    scale: sendspin::audio::volume_scale::VolumeScale,
) -> Option<std::sync::Arc<sendspin::audio::mixer::Mixer>> {
    let insisted = matches!(control, VolumeControl::Alsa);
    let Some(card) = output.and_then(mixer_card) else {
        if insisted {
            warn!("volume_control is alsa but the card is not named by name; using software gain");
        }
        return None;
    };
    match sendspin::audio::mixer::Mixer::open(&card, scale) {
        Ok(mixer) => {
            info!("hardware volume on {} ({})", mixer.card(), mixer.element());
            Some(std::sync::Arc::new(mixer))
        }
        Err(err) if insisted => {
            warn!("volume_control is alsa but {}; using software gain", err);
            None
        }
        Err(err) => {
            info!("no hardware volume on {}: {}", card, err);
            None
        }
    }
}

/// How to map a percentage onto this card, when the server has an opinion.
///
/// Absent means the crate asks the card, which is right wherever a card describes itself honestly.
/// `mixer_mapped` is for the one that does not, and it is a per-installation fact: somebody measured
/// this speaker.
#[cfg(target_os = "linux")]
fn mixer_scale(desired: &DesiredPlayer) -> sendspin::audio::volume_scale::VolumeScale {
    use sendspin::audio::volume_scale::VolumeScale;
    match desired.mixer_mapped {
        Some(true) => VolumeScale::Decibel,
        Some(false) => VolumeScale::Raw,
        None => VolumeScale::Automatic,
    }
}

/// `alsa:hw:CARD=CDCACM,DEV=0` -> `hw:CARD=CDCACM`, which is what a mixer is addressed by.
///
/// Only the by-name spelling is accepted. A number names whichever card enumerated first this boot,
/// and setting the volume of the wrong card is worse than not setting it.
#[cfg(target_os = "linux")]
fn mixer_card(device_id: &str) -> Option<String> {
    let card = device_id.split_once("CARD=")?.1;
    let card = card.split(',').next().unwrap_or(card).trim();
    if card.is_empty() || card.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("hw:CARD={}", card))
}

/// A format the server pinned, or nothing at all.
///
/// Pinning is for hardware that genuinely accepts one format; without both a rate and a depth there
/// is nothing to pin, and the client offers everything its card can open instead.
fn pinned_format(desired: &DesiredPlayer) -> Option<AudioFormatSpec> {
    let sample_rate = desired.sample_rate?;
    let bit_depth = desired.bit_depth?;
    Some(AudioFormatSpec {
        codec: desired
            .codecs
            .as_ref()
            .and_then(|codecs| codecs.first())
            .cloned()
            .unwrap_or_else(|| "pcm".to_string()),
        channels: desired
            .channels
            .and_then(|channels| u8::try_from(channels).ok())
            .unwrap_or(DEFAULT_CHANNELS),
        sample_rate,
        bit_depth,
    })
}

/// The codecs the server allowed, in the crate's own preference order.
///
/// `None` leaves the crate's offer alone. A name this build cannot decode is dropped rather than
/// passed on: an offer has to be something the client can actually play, or it promises a server
/// audio that will arrive and go nowhere.
fn offered_codecs(desired: &DesiredPlayer) -> Option<Vec<CodecOffer>> {
    let wanted = desired.codecs.as_ref()?;
    if wanted.is_empty() {
        return None;
    }
    let allowed: Vec<String> = wanted
        .iter()
        .map(|codec| codec.trim().to_ascii_lowercase())
        .collect();
    let offer: Vec<CodecOffer> = CodecOffer::all()
        .into_iter()
        .filter(|offer| allowed.contains(&offer.codec))
        .collect();
    if offer.is_empty() {
        warn!(
            "none of the codecs the server named can be decoded here ({}); offering all of them",
            wanted.join(", ")
        );
        return None;
    }
    Some(offer)
}

/// Run one player until it is told to stop, reporting what it does as it does it.
pub async fn run(
    player: Player,
    url: String,
    status: PlayerHandle,
    mut settings_rx: watch::Receiver<LiveSettings>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut player_status = player.status();
    report(&player_status.borrow(), &status);

    // The crate reconnects on its own, waiting longer each time up to a minute, so this returns only
    // when the player gives up for good.
    let session = player.run_outbound(&url, Some(RECONNECT_DELAY));
    tokio::pin!(session);

    loop {
        tokio::select! {
            outcome = &mut session => {
                if let Err(err) = outcome {
                    warn!("player session ended: {}", err);
                    status.set_error(err.to_string());
                }
                return;
            }
            Ok(()) = player_status.changed() => {
                report(&player_status.borrow(), &status);
            }
            Ok(()) = settings_rx.changed() => {
                let next = settings_rx.borrow_and_update().clone();
                player.set_static_delay(next.static_delay_ms);
                // The level too: this arm carries both the server's seed and a press on the
                // remote, and the crate reports the result back over `client/state` either way.
                player.set_volume(next.volume, next.muted);
                status.set_static_delay(next.static_delay_ms);
            }
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    // Dropping the session future closes the card, which is the point of being
                    // asked to stop.
                    return;
                }
            }
        }
    }
}

/// Copy what the player says about itself into what this device reports upstream.
fn report(from: &PlayerStatus, to: &PlayerHandle) {
    match &from.last_error {
        Some(error) => to.set_error(error.clone()),
        None => to.set_state_ok(state_name(from.connection)),
    }
    to.set_volume(from.volume, from.muted);
    match &from.format {
        Some(format) => to.set_format(
            codec_name(format.codec),
            format.sample_rate,
            format.bit_depth,
            u16::from(format.channels),
        ),
        None => to.clear_format(),
    }
}

fn state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => STATE_IDLE,
        ConnectionState::Connecting => STATE_CONNECTING,
        ConnectionState::Connected => STATE_CONNECTED,
        ConnectionState::Playing => STATE_STREAMING,
    }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::Pcm => "pcm",
        Codec::Opus => "opus",
        Codec::Flac => "flac",
        Codec::Mp3 => "mp3",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_codecs(codecs: Option<Vec<&str>>) -> DesiredPlayer {
        DesiredPlayer {
            codecs: codecs.map(|codecs| codecs.into_iter().map(str::to_string).collect()),
            ..DesiredPlayer::named("speaker")
        }
    }

    #[test]
    fn the_offer_is_narrowed_to_what_the_server_allowed() {
        let offer = offered_codecs(&with_codecs(Some(vec!["flac", "pcm"]))).expect("an offer");
        assert!(offer.iter().all(|entry| entry.codec != "opus"));
        assert!(offer.iter().any(|entry| entry.codec == "flac"));

        // Absent means "whatever you can decode", which is the crate's own offer.
        assert!(offered_codecs(&with_codecs(None)).is_none());
        assert!(offered_codecs(&with_codecs(Some(vec![]))).is_none());

        // A codec this build cannot decode would promise a server audio that arrives and goes
        // nowhere, so the whole offer falls back rather than narrowing to nothing.
        assert!(offered_codecs(&with_codecs(Some(vec!["ape"]))).is_none());
    }

    #[test]
    fn a_format_is_only_pinned_when_there_is_one_to_pin() {
        assert!(pinned_format(&with_codecs(None)).is_none());

        let pinned = pinned_format(&DesiredPlayer {
            sample_rate: Some(44_100),
            bit_depth: Some(24),
            ..DesiredPlayer::named("speaker")
        })
        .expect("a pinned format");
        assert_eq!(pinned.sample_rate, 44_100);
        assert_eq!(pinned.bit_depth, 24);
        assert_eq!(pinned.channels, DEFAULT_CHANNELS);
        assert_eq!(pinned.codec, "pcm");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_mixer_is_addressed_by_card_name_only() {
        assert_eq!(
            mixer_card("alsa:hw:CARD=CDCACM,DEV=0").as_deref(),
            Some("hw:CARD=CDCACM")
        );
        // The number moves with USB probe order, and setting the wrong card's volume is worse than
        // leaving it in software.
        assert_eq!(mixer_card("alsa:hw:CARD=3,DEV=0"), None);
        assert_eq!(mixer_card("alsa:null"), None);
    }
}
