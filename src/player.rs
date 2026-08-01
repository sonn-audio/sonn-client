//! One Sendspin player: a socket, a decoder and a sound card.
//!
//! This is the only part of the client that talks the protocol, and it talks nothing else -- no
//! AirPlay, no DLNA, no Bluetooth sink. Those all exist on this device too, but they live on the
//! *server*, which converts them into a Sendspin stream aimed here. That is the whole point: one
//! protocol on the device, one place where sync is solved.
//!
//! Protocol, clock filter, decoders and the timestamp-scheduled cpal output come from the `sendspin`
//! crate; what this module owns is the lifecycle -- how a stream's format change is applied, where
//! volume goes, what gets reported, and when to give up and reconnect.

use crate::alsa_volume::AlsaMixer;
use crate::devices;
use crate::hooks::VolumeHook;
use crate::models::{DesiredPlayer, VolumeControl};
use crate::status::{PlayerHandle, STATE_CONNECTED, STATE_CONNECTING, STATE_IDLE, STATE_STREAMING};
use anyhow::{anyhow, Context, Result};
use base64::prelude::*;
use sendspin::audio::decode::{Decoder, FlacDecoder, OpusDecoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec, SyncedPlayer, SyncedPlayerConfig};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientState, Message, PlayerCommand, PlayerCommandType, PlayerState,
    PlayerStateCommand, PlayerV1Support,
};
use sendspin::{ProtocolClientBuilder, WsSender};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Codecs this build can decode, best first. FLAC ahead of PCM so a busy wifi link carries lossless
/// audio instead of 2.3 Mbit/s of raw samples; Opus last because it is the only lossy one.
pub const SUPPORTED_CODECS: [&str; 3] = ["flac", "opus", "pcm"];

/// Rates advertised when the server pins none. Both are here so the server's bit-perfect path can
/// pass a 44.1 kHz album through untouched instead of resampling everything to 48.
const DEFAULT_RATES: [u32; 2] = [48_000, 44_100];
const DEFAULT_BIT_DEPTHS: [u8; 2] = [24, 16];
const DEFAULT_CHANNELS: u16 = 2;
const DEFAULT_BUFFER_MS: u32 = 500;
const DEFAULT_LEAD_MS: u32 = 500;
/// Room for a couple of seconds of 24-bit stereo, which is well past any lead the server asks for.
const BUFFER_CAPACITY_BYTES: usize = 8 * 1024 * 1024;
/// How often the clock lock is copied into the status report.
const CLOCK_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// What can change without dropping audio. Everything else lives in `PlayerParams` and a change
/// there means a reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSettings {
    pub volume: u8,
    pub muted: bool,
    pub static_delay_ms: u16,
}

#[derive(Debug, Clone)]
pub struct PlayerParams {
    pub url: String,
    pub client_id: String,
    pub name: String,
    /// cpal device id to open. `None` means the host default.
    pub output: Option<String>,
    pub codecs: Vec<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u16>,
    pub buffer_ms: Option<u32>,
    pub required_lead_time_ms: Option<u32>,
}

/// Where a player's volume ends up.
///
/// Decided once when the player starts, because it depends on what the card turns out to be, and a
/// speaker that moved its own volume yesterday should not be attenuating in software today.
pub enum VolumeSink {
    /// Gain in our own mixer. The fallback, not the preference: every dB taken here is resolution
    /// the card would not have taken.
    Software,
    /// A script the server named. Highest precedence -- somebody wired this deliberately.
    Hook(VolumeHook),
    /// The card's own mixer.
    Mixer(AlsaMixer),
}

impl VolumeSink {
    pub async fn resolve(player: &DesiredPlayer, fallback_hook: Option<String>) -> Self {
        let hook = player
            .volume_hook
            .clone()
            .or(fallback_hook)
            .map(|command| command.trim().to_string())
            .filter(|command| !command.is_empty());

        match player.volume_control() {
            VolumeControl::Software => VolumeSink::Software,
            VolumeControl::Hook => {
                match hook {
                    Some(command) => VolumeSink::Hook(VolumeHook::new(command)),
                    None => {
                        warn!("volume_control is hook but no volume_hook was given; using software gain");
                        VolumeSink::Software
                    }
                }
            }
            VolumeControl::Alsa => match AlsaMixer::discover(player).await {
                Some(mixer) => VolumeSink::Mixer(mixer),
                None => {
                    warn!("volume_control is alsa but this card has no mixer; using software gain");
                    VolumeSink::Software
                }
            },
            // A hook is a deliberate act, so it wins over a mixer we merely found.
            VolumeControl::Auto => match hook {
                Some(command) => VolumeSink::Hook(VolumeHook::new(command)),
                None => match AlsaMixer::discover(player).await {
                    Some(mixer) => VolumeSink::Mixer(mixer),
                    None => VolumeSink::Software,
                },
            },
        }
    }

    /// Whether the software mixer is the one doing the attenuating.
    ///
    /// When something else is, our own gain stays at unity: attenuating twice -- once here, once in
    /// the amplifier -- costs bits and makes the zone slider non-linear. Same lesson as the double
    /// taper in the BeoLab chain.
    pub fn is_software(&self) -> bool {
        matches!(self, VolumeSink::Software)
    }

    pub async fn apply(&self, volume: u8, muted: bool) {
        match self {
            VolumeSink::Software => {}
            VolumeSink::Hook(hook) => hook.apply(volume, muted).await,
            VolumeSink::Mixer(mixer) => mixer.apply(volume, muted).await,
        }
    }
}

/// Run one connection until it closes, then return so the caller can back off and retry.
///
/// `Ok(())` means the server hung up (or we were told to stop); `Err` means we never got going --
/// the card is missing, the socket refused. Both end the session, but only one is worth reporting as
/// a fault.
pub async fn run_session(
    params: &PlayerParams,
    settings_rx: &mut watch::Receiver<LiveSettings>,
    status: &PlayerHandle,
    volume: &VolumeSink,
) -> Result<()> {
    let device = match params.output.as_deref() {
        Some(id) if !id.trim().is_empty() => Some(
            devices::find_output_device(id)
                // Deliberately not "fall back to the default card": if the server was told to use
                // the DAC and the DAC is unplugged, playing out of HDMI instead is a worse answer
                // than saying the card is gone.
                .ok_or_else(|| anyhow!("output device '{}' not found", id))?,
        ),
        _ => None,
    };

    let mut settings = settings_rx.borrow_and_update().clone();
    status.set_state(STATE_CONNECTING);

    let client = ProtocolClientBuilder::builder()
        .client_id(params.client_id.clone())
        .name(params.name.clone())
        .player_v1_support(PlayerV1Support {
            supported_formats: advertised_formats(params),
            buffer_capacity: BUFFER_CAPACITY_BYTES.try_into().unwrap_or_default(),
            // Volume and mute are accepted so the server can drive this player from a zone slider;
            // whether that lands on software gain or a hardware hook is decided below.
            supported_commands: vec![
                "volume".to_string(),
                "mute".to_string(),
                "set_static_delay".to_string(),
            ],
        })
        .initial_player_state(PlayerState {
            volume: Some(settings.volume),
            muted: Some(settings.muted),
            static_delay_ms: Some(settings.static_delay_ms),
            required_lead_time_ms: Some(params.required_lead_time_ms.unwrap_or(DEFAULT_LEAD_MS)),
            min_buffer_ms: Some(params.buffer_ms.unwrap_or(DEFAULT_BUFFER_MS)),
            supported_commands: Some(vec![PlayerStateCommand::SetStaticDelay]),
        })
        .build()
        .connect(&params.url)
        .await
        .with_context(|| format!("connect to {}", params.url))?;

    info!(
        client_id = %params.client_id,
        url = %params.url,
        output = params.output.as_deref().unwrap_or("(default)"),
        "sendspin player connected"
    );
    status.set_state_ok(STATE_CONNECTED);

    let connection = client.split();
    let mut message_rx = connection.messages;
    let mut audio_rx = connection.audio;
    let clock_sync = connection.clock_sync;
    let sender = connection.sender;
    let _guard = connection.guard;

    let software_volume = volume.is_software();
    volume.apply(settings.volume, settings.muted).await;

    let mut player: Option<SyncedPlayer> = None;
    let mut format: Option<AudioFormat> = None;
    let mut decoder: Option<Box<dyn Decoder>> = None;
    let mut pcm_endian_locked = false;

    // A closure rather than a helper function: the clock handle's type is the crate's own, and
    // capturing it here means never having to name it.
    let open_output = |format: &AudioFormat, volume: u8, muted: bool| -> Result<SyncedPlayer> {
        SyncedPlayer::new(
            format.clone(),
            Arc::clone(&clock_sync),
            SyncedPlayerConfig {
                device: device.clone(),
                volume: if software_volume { volume } else { 100 },
                muted: software_volume && muted,
                buffer_size: None,
            },
        )
        .map_err(|err| anyhow!("open audio output: {}", err))
    };

    // Clock sync is the library's own business: it sends `client/time` and consumes `server/time`
    // without ever forwarding it here, so the only way to report on the lock is to look at the
    // filter. Once a second is plenty for a number that moves in milliseconds.
    let mut clock_report = tokio::time::interval(CLOCK_REPORT_INTERVAL);

    loop {
        tokio::select! {
            _ = clock_report.tick() => {
                let sync = clock_sync.lock();
                let rtt_ms = sync.rtt_micros().map(|rtt| rtt as f64 / 1000.0);
                let quality = format!("{:?}", sync.quality()).to_lowercase();
                drop(sync);
                status.set_clock(rtt_ms, Some(quality));
            }
            // Live settings before audio: a volume command should not wait behind a queue of chunks.
            changed = settings_rx.changed() => {
                if changed.is_err() {
                    // The supervisor dropped us; it is already tearing this task down.
                    return Ok(());
                }
                let next = settings_rx.borrow_and_update().clone();
                if next == settings {
                    continue;
                }
                settings = next;
                apply_settings(&settings, player.as_ref(), volume, software_volume, status, &sender).await;
            }
            message = message_rx.recv() => {
                // `None` is the socket closing: end the session so the supervisor can reconnect.
                // Treating a closed channel as "nothing happened" turns this select into a spin.
                let Some(message) = message else { break };
                match message {
                    Message::StreamStart(stream_start) => {
                        let Some(config) = stream_start.player.as_ref() else {
                            debug!("stream/start carried no player config");
                            continue;
                        };
                        let codec = match parse_codec(&config.codec) {
                            Some(codec) => codec,
                            None => {
                                // Nothing to do but say so: the server picked something outside what
                                // we advertised, and guessing a decoder would render noise.
                                status.set_error(format!("unsupported codec '{}'", config.codec));
                                decoder = None;
                                format = None;
                                continue;
                            }
                        };
                        let header = match config.codec_header.as_deref().map(decode_header) {
                            Some(Ok(header)) => Some(header),
                            Some(Err(err)) => {
                                status.set_error(format!("invalid codec header: {}", err));
                                continue;
                            }
                            None => None,
                        };
                        let next_format = AudioFormat {
                            codec,
                            sample_rate: config.sample_rate,
                            channels: config.channels,
                            bit_depth: config.bit_depth,
                            codec_header: header.clone(),
                        };

                        // A rate or depth change means a different cpal stream, so the open output
                        // is released here and reopened on the first chunk of the new format. The
                        // upstream example keeps its first player forever, which is fine for a demo
                        // and wrong for a server that switches between 44.1 and 48 per track.
                        if output_needs_reopen(format.as_ref(), &next_format) {
                            if let Some(open) = player.take() {
                                open.clear();
                            }
                        }

                        info!(
                            client_id = %params.client_id,
                            codec = %config.codec,
                            sample_rate = config.sample_rate,
                            bit_depth = config.bit_depth,
                            channels = config.channels,
                            "stream starting"
                        );
                        decoder = build_decoder(&next_format, header.as_deref());
                        // Only raw PCM has an endianness to settle, and it is settled on its first
                        // chunk; a framed codec is "locked" from the start.
                        pcm_endian_locked = codec != Codec::Pcm;
                        format = Some(next_format);
                        status.set_format(
                            &config.codec,
                            config.sample_rate,
                            config.bit_depth,
                            u16::from(config.channels),
                        );
                        status.set_state_ok(STATE_STREAMING);
                    }
                    Message::StreamEnd(_) | Message::StreamClear(_) => {
                        if let Some(open) = player.as_ref() {
                            open.clear();
                        }
                        decoder = None;
                        format = None;
                        pcm_endian_locked = false;
                        status.clear_format();
                        status.set_state_ok(STATE_CONNECTED);
                    }
                    Message::ServerCommand(command) => {
                        if let Some(player_command) = command.player {
                            settings = apply_command(settings.clone(), &player_command);
                            apply_settings(
                                &settings,
                                player.as_ref(),
                                volume,
                                software_volume,
                                status,
                                &sender,
                            )
                            .await;
                        }
                    }
                    other => debug!("unhandled sendspin message: {:?}", other),
                }
            }
            chunk = audio_rx.recv() => {
                let Some(chunk) = chunk else { break };
                let Some(active_format) = format.as_ref() else {
                    // Chunks before (or after) a stream/start have no format to decode against.
                    continue;
                };

                if active_format.codec == Codec::Pcm {
                    let bytes_per_sample = usize::from(active_format.bit_depth) / 8;
                    let frame = bytes_per_sample * usize::from(active_format.channels);
                    // Raw PCM has no framing of its own, so a partial frame is the first sign the
                    // stream and our idea of its format disagree. Decoding it would emit a click.
                    if frame == 0 || chunk.data.len() % frame != 0 {
                        warn!(
                            "dropping {} PCM bytes that do not fill {}-byte frames",
                            chunk.data.len(),
                            frame
                        );
                        continue;
                    }
                    if !pcm_endian_locked {
                        // Little-endian: what every host platform and our own server produce. The
                        // protocol has no field for it, so this is a convention, not a negotiation.
                        decoder = Some(Box::new(PcmDecoder::with_endian(
                            active_format.bit_depth,
                            PcmEndian::Little,
                        )));
                        pcm_endian_locked = true;
                    }
                }

                let Some(active_decoder) = decoder.as_ref() else {
                    continue;
                };
                let samples = match active_decoder.decode(&chunk.data) {
                    Ok(samples) => samples,
                    Err(err) => {
                        // One bad frame is not a dead stream; the next one usually decodes.
                        debug!("decode error: {}", err);
                        continue;
                    }
                };

                if player.is_none() {
                    match open_output(active_format, settings.volume, settings.muted) {
                        Ok(open) => {
                            open.set_static_delay(settings.static_delay_ms);
                            player = Some(open);
                        }
                        Err(err) => {
                            // The card is there but will not open at this format. Reporting and
                            // returning lets the supervisor retry with backoff instead of spinning
                            // on every chunk.
                            status.set_error(err.to_string());
                            return Err(err);
                        }
                    }
                }

                if let Some(open) = player.as_ref() {
                    open.enqueue(AudioBuffer {
                        timestamp: chunk.timestamp,
                        samples,
                        format: active_format.clone(),
                    });
                    if let Some(err) = open.take_error() {
                        // A cpal callback error (device removed, xrun storm) is fatal to this stream.
                        status.set_error(err.clone());
                        return Err(anyhow!("audio output failed: {}", err));
                    }
                }
            }
        }
    }

    info!(client_id = %params.client_id, "sendspin session ended");
    status.set_state(STATE_IDLE);
    status.clear_format();
    Ok(())
}

/// Fold a server player command into the live settings.
fn apply_command(mut settings: LiveSettings, command: &PlayerCommand) -> LiveSettings {
    match command.command {
        PlayerCommandType::Volume => {
            if let Some(volume) = command.volume {
                settings.volume = volume.min(100);
            }
        }
        PlayerCommandType::Mute => {
            if let Some(muted) = command.mute {
                settings.muted = muted;
            }
        }
        PlayerCommandType::SetStaticDelay => {
            if let Some(delay_ms) = command.static_delay_ms {
                settings.static_delay_ms = delay_ms;
            }
        }
        _ => {}
    }
    settings
}

async fn apply_settings(
    settings: &LiveSettings,
    player: Option<&SyncedPlayer>,
    volume: &VolumeSink,
    software_volume: bool,
    status: &PlayerHandle,
    sender: &WsSender,
) {
    if let Some(open) = player {
        open.set_static_delay(settings.static_delay_ms);
        if software_volume {
            open.set_volume(settings.volume);
            open.set_mute(settings.muted);
        }
    }
    volume.apply(settings.volume, settings.muted).await;
    status.set_volume(settings.volume, settings.muted);
    status.set_static_delay(settings.static_delay_ms);
    report_state(sender, settings).await;
}

/// Tell the server where this player ended up.
///
/// It matters most when the change did *not* come from the server: a B&O remote turning the volume up
/// is invisible otherwise, and the zone slider would sit at the old level until something else moved
/// it. Echoing a change the server itself asked for is harmless -- the reference client does the same.
async fn report_state(sender: &WsSender, settings: &LiveSettings) {
    let state = ClientState {
        // A volume report says nothing about whether this player is available, and claiming
        // either way on every volume change would be the client talking over itself.
        available: None,
        state: None,
        player: Some(PlayerState {
            volume: Some(settings.volume),
            muted: Some(settings.muted),
            static_delay_ms: Some(settings.static_delay_ms),
            required_lead_time_ms: None,
            min_buffer_ms: None,
            supported_commands: None,
        }),
        source: None,
    };
    if let Err(err) = sender.send_message(Message::ClientState(state)).await {
        // Not fatal: the socket is about to be torn down anyway, and the next connection re-announces
        // everything in its initial state.
        debug!("could not report player state: {}", err);
    }
}

/// Whether the open cpal stream can carry the next format or has to be reopened.
fn output_needs_reopen(current: Option<&AudioFormat>, next: &AudioFormat) -> bool {
    match current {
        None => false,
        Some(current) => {
            current.sample_rate != next.sample_rate
                || current.channels != next.channels
                || current.bit_depth != next.bit_depth
        }
    }
}

/// Build the decoder for a stream. Takes the whole format so codec, rate and channel count come
/// from one place -- an Opus decoder built at the wrong rate decodes to garbage.
fn build_decoder(format: &AudioFormat, header: Option<&[u8]>) -> Option<Box<dyn Decoder>> {
    match format.codec {
        // Built on the first chunk instead, once the endianness is settled.
        Codec::Pcm => None,
        Codec::Flac => Some(match header {
            // Without STREAMINFO the decoder has to infer the stream; our server always sends the
            // header, so a rejected one is worth a line in the log before falling back.
            Some(header) => match FlacDecoder::with_header(header) {
                Ok(decoder) => Box::new(decoder),
                Err(err) => {
                    warn!("FLAC header rejected, decoding without it: {}", err);
                    Box::new(FlacDecoder::new())
                }
            },
            None => Box::new(FlacDecoder::new()),
        }),
        Codec::Opus => match OpusDecoder::new(format.sample_rate, format.channels) {
            Ok(decoder) => Some(Box::new(decoder)),
            Err(err) => {
                warn!("Opus decoder unavailable: {}", err);
                None
            }
        },
        _ => None,
    }
}

fn parse_codec(codec: &str) -> Option<Codec> {
    match codec {
        "pcm" => Some(Codec::Pcm),
        "flac" => Some(Codec::Flac),
        "opus" => Some(Codec::Opus),
        _ => None,
    }
}

fn decode_header(encoded: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(encoded)
        .context("base64 codec header")
}

/// The format list in `client/hello`, best first.
///
/// The server picks from this and prefers the source's own rate when it appears here, so a pinned
/// rate is a real constraint -- worth doing for a DAC that only does 48 kHz, worth avoiding
/// otherwise.
fn advertised_formats(params: &PlayerParams) -> Vec<AudioFormatSpec> {
    let codecs = if params.codecs.is_empty() {
        SUPPORTED_CODECS.iter().map(|c| c.to_string()).collect()
    } else {
        params.codecs.clone()
    };
    let rates: Vec<u32> = match params.sample_rate {
        Some(rate) => vec![rate],
        None => DEFAULT_RATES.to_vec(),
    };
    let depths: Vec<u8> = match params.bit_depth {
        Some(depth) => vec![depth],
        None => DEFAULT_BIT_DEPTHS.to_vec(),
    };
    let channels = params.channels.unwrap_or(DEFAULT_CHANNELS);

    let mut formats = Vec::new();
    for codec in codecs {
        // Opus is defined at 16 bit; offering 24 would advertise something we cannot receive.
        let codec_depths: Vec<u8> = if codec == "opus" {
            vec![16]
        } else {
            depths.clone()
        };
        for rate in &rates {
            for depth in &codec_depths {
                formats.push(AudioFormatSpec {
                    codec: codec.clone(),
                    channels: channels.try_into().unwrap_or_default(),
                    sample_rate: *rate,
                    bit_depth: *depth,
                });
            }
        }
    }
    formats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> PlayerParams {
        PlayerParams {
            url: "ws://127.0.0.1:7090/sendspin".to_string(),
            client_id: "sonn-test".to_string(),
            name: "Test".to_string(),
            output: None,
            codecs: Vec::new(),
            sample_rate: None,
            bit_depth: None,
            channels: None,
            buffer_ms: None,
            required_lead_time_ms: None,
        }
    }

    #[test]
    fn both_rates_are_offered_when_none_is_pinned() {
        let formats = advertised_formats(&params());
        assert!(formats.iter().any(|f| f.sample_rate == 44_100));
        assert!(formats.iter().any(|f| f.sample_rate == 48_000));
    }

    #[test]
    fn a_pinned_format_is_the_only_one_offered() {
        let mut params = params();
        params.codecs = vec!["pcm".to_string()];
        params.sample_rate = Some(48_000);
        params.bit_depth = Some(16);
        let formats = advertised_formats(&params);
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].codec, "pcm");
        assert_eq!(formats[0].sample_rate, 48_000);
        assert_eq!(formats[0].bit_depth, 16);
    }

    #[test]
    fn opus_is_never_offered_at_24_bit() {
        let mut params = params();
        params.codecs = vec!["opus".to_string()];
        params.bit_depth = Some(24);
        let formats = advertised_formats(&params);
        assert!(formats.iter().all(|f| f.bit_depth == 16));
    }

    #[test]
    fn a_rate_change_reopens_the_output_but_a_codec_change_alone_does_not() {
        let base = AudioFormat {
            codec: Codec::Flac,
            sample_rate: 44_100,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        };
        let same_rate_other_codec = AudioFormat {
            codec: Codec::Pcm,
            ..base.clone()
        };
        let other_rate = AudioFormat {
            sample_rate: 48_000,
            ..base.clone()
        };
        assert!(!output_needs_reopen(Some(&base), &same_rate_other_codec));
        assert!(output_needs_reopen(Some(&base), &other_rate));
        assert!(!output_needs_reopen(None, &base));
    }

    #[test]
    fn volume_and_mute_commands_fold_into_live_settings() {
        let settings = LiveSettings {
            volume: 40,
            muted: false,
            static_delay_ms: 0,
        };
        let louder = apply_command(
            settings.clone(),
            &PlayerCommand {
                command: PlayerCommandType::Volume,
                volume: Some(70),
                mute: None,
                static_delay_ms: None,
            },
        );
        assert_eq!(louder.volume, 70);
        let muted = apply_command(
            settings,
            &PlayerCommand {
                command: PlayerCommandType::Mute,
                volume: None,
                mute: Some(true),
                static_delay_ms: None,
            },
        );
        assert!(muted.muted);
        assert_eq!(muted.volume, 40, "muting must not forget the level");
    }
}
