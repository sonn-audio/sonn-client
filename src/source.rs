//! One Sendspin source: a capture device streamed to the server.
//!
//! A source is a player in reverse. The server decides when this input is being listened to, asks
//! for it to start, and does everything after capture -- resample, mix, distribute. What lives here
//! is capture, a level measurement, and the one thing only this end can know: whether there is
//! actually audio on the input. Nobody can start a turntable remotely, so a source that hears
//! something says so, and the server decides what that means.
//!
//! This is the same path the line-in bridge took over TCP, moved onto the protocol the rest of the
//! device already speaks: one connection per input, one clock, and the server-side ingest it already
//! has for Sendspin sources.

use crate::devices;
use crate::hooks::ControlHook;
use crate::status::{SourceHandle, STATE_CONNECTED, STATE_CONNECTING, STATE_IDLE, STATE_STREAMING};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sendspin::protocol::messages::{
    InputStreamSource, Message, SourceClientCommandType, SourceCommandType, SourceControl,
    SourceFeatures, SourceFormat, SourceSignal, SourceState, SourceStateType, SourceV1Support,
};
use sendspin::{Clock, ProtocolClientBuilder, WsSender};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// PCM only. The server transcodes anyway, and encoding on the device would spend a Pi's CPU to save
/// bandwidth on a LAN that has plenty.
pub const CODEC: &str = "pcm";
/// Capture defaults when the server names none. 48 kHz/16-bit stereo is what every USB interface and
/// every Pi HAT does without argument.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHANNELS: u16 = 2;
pub const DEFAULT_BIT_DEPTH: u8 = 16;
/// 20 ms per chunk: 50 sends a second, small enough that the server's ingest never waits on us.
pub const DEFAULT_FRAME_MS: u64 = 20;
/// Silence threshold when the server names none, matching the line-in bridge's default.
const DEFAULT_THRESHOLD_DB: f32 = -45.0;
const DEFAULT_HOLD_MS: u64 = 2_000;
/// Frames the capture callback may run ahead before we start dropping.
///
/// Capture is real-time and the socket is not. A backlog is latency we can never make up -- the
/// server has already scheduled around the timestamps we sent -- so the queue stays small and the
/// oldest frames are dropped rather than delivered late.
const CAPTURE_QUEUE_FRAMES: usize = 16;
const CLOCK_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct SourceParams {
    pub url: String,
    pub client_id: String,
    pub name: String,
    /// cpal capture device id. `None` means the host default.
    pub input: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u8,
    pub frame_ms: u64,
    /// Transport controls to advertise for whatever is wired to this input.
    pub controls: Vec<SourceControl>,
    /// Stream without waiting to be asked.
    pub always_on: bool,
}

impl SourceParams {
    fn format(&self) -> SourceFormat {
        SourceFormat {
            codec: CODEC.to_string(),
            channels: self.channels.try_into().unwrap_or(2),
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
        }
    }

    fn frames_per_chunk(&self) -> usize {
        let frames = self.sample_rate as u64 * self.frame_ms.max(1) / 1000;
        frames.max(1) as usize
    }

    fn chunk_duration(&self) -> Duration {
        Duration::from_micros(
            self.frames_per_chunk() as u64 * 1_000_000 / u64::from(self.sample_rate.max(1)),
        )
    }
}

/// Signal-detection settings. Pushed down by the server, either in the desired config or live over
/// Sendspin as VAD settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalSettings {
    pub threshold_db: f32,
    pub hold_ms: u64,
}

impl Default for SignalSettings {
    fn default() -> Self {
        Self {
            threshold_db: DEFAULT_THRESHOLD_DB,
            hold_ms: DEFAULT_HOLD_MS,
        }
    }
}

impl SignalSettings {
    /// Linear amplitude the dBFS threshold corresponds to.
    fn linear(&self) -> f32 {
        10f32.powf(self.threshold_db / 20.0)
    }
}

pub fn parse_control(value: &str) -> Option<SourceControl> {
    match value.trim().to_ascii_lowercase().as_str() {
        "play" => Some(SourceControl::Play),
        "pause" => Some(SourceControl::Pause),
        "next" => Some(SourceControl::Next),
        "previous" => Some(SourceControl::Previous),
        "activate" => Some(SourceControl::Activate),
        "deactivate" => Some(SourceControl::Deactivate),
        _ => None,
    }
}

fn control_name(control: SourceControl) -> &'static str {
    match control {
        SourceControl::Play => "play",
        SourceControl::Pause => "pause",
        SourceControl::Next => "next",
        SourceControl::Previous => "previous",
        SourceControl::Activate => "activate",
        SourceControl::Deactivate => "deactivate",
        SourceControl::Unknown => "unknown",
    }
}

fn signal_name(signal: SourceSignal) -> &'static str {
    match signal {
        SourceSignal::Unknown => "unknown",
        SourceSignal::Present => "present",
        SourceSignal::Absent => "absent",
    }
}

/// One captured chunk on its way out: interleaved little-endian PCM plus the moment its first sample
/// was taken, in the library's clock.
struct CapturedChunk {
    capture_us: i64,
    pcm: Vec<u8>,
}

/// Run one source connection until it closes.
pub async fn run_session(
    params: &SourceParams,
    signal_rx: &mut watch::Receiver<SignalSettings>,
    status: &SourceHandle,
    control_hook: Option<&ControlHook>,
) -> Result<()> {
    let device = match params.input.as_deref() {
        Some(id) if !id.trim().is_empty() => devices::find_input_device(id)
            .ok_or_else(|| anyhow!("capture device '{}' not found", id))?,
        // Unlike an output, a default capture device is a reasonable answer: a Pi with one USB
        // interface has exactly one, and naming it adds nothing.
        _ => cpal::default_host()
            .default_input_device()
            .ok_or_else(|| anyhow!("no capture device available"))?,
    };

    let mut settings = *signal_rx.borrow_and_update();
    status.set_state(STATE_CONNECTING);
    status.set_format(CODEC, params.sample_rate, params.bit_depth, params.channels);

    let client = ProtocolClientBuilder::builder()
        .client_id(params.client_id.clone())
        .name(params.name.clone())
        .source_v1_support(SourceV1Support {
            supported_formats: vec![params.format()],
            controls: (!params.controls.is_empty()).then(|| params.controls.clone()),
            features: Some(SourceFeatures {
                level: Some(true),
                line_sense: Some(true),
            }),
        })
        .initial_source_state(SourceState {
            state: SourceStateType::Idle,
            level: Some(0.0),
            signal: Some(SourceSignal::Unknown),
        })
        .build()
        .connect(&params.url)
        .await
        .with_context(|| format!("connect to {}", params.url))?;

    info!(
        client_id = %params.client_id,
        url = %params.url,
        input = params.input.as_deref().unwrap_or("(default)"),
        "sendspin source connected"
    );
    status.set_state_ok(STATE_CONNECTED);

    let connection = client.split();
    let mut message_rx = connection.messages;
    let clock_sync = connection.clock_sync;
    let sender = connection.sender;
    let _guard = connection.guard;
    // Capture timestamps have to be in the timebase the filter runs on. Clock sync is the library's
    // own business -- `server/time` never reaches this loop -- so the clock is taken from it.
    let clock = clock_sync.lock().clock();

    let (chunk_tx, mut chunk_rx) = mpsc::channel::<CapturedChunk>(CAPTURE_QUEUE_FRAMES);
    let stream = start_capture(&device, params, clock.clone(), chunk_tx)?;
    stream.play().context("start capture stream")?;

    let mut reporter = SignalReporter::new(settings);
    let mut streaming = params.always_on;
    if streaming {
        announce_stream(&sender, params).await?;
        status.set_state_ok(STATE_STREAMING);
    }
    let mut clock_report = tokio::time::interval(CLOCK_REPORT_INTERVAL);

    loop {
        tokio::select! {
            _ = clock_report.tick() => {
                let sync = clock_sync.lock();
                let rtt_ms = sync.rtt_micros().map(|rtt| rtt as f64 / 1000.0);
                drop(sync);
                status.set_clock(rtt_ms);
            }
            changed = signal_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let next = *signal_rx.borrow_and_update();
                if next != settings {
                    settings = next;
                    reporter.update_settings(settings);
                    info!(
                        client_id = %params.client_id,
                        threshold_db = settings.threshold_db,
                        hold_ms = settings.hold_ms,
                        "signal detection settings updated"
                    );
                }
            }
            message = message_rx.recv() => {
                let Some(message) = message else { break };
                match message {
                    Message::ServerCommand(command) => {
                        let Some(source) = command.source else { continue };
                        if let Some(vad) = source.vad {
                            // The live path for the same two numbers. Whichever arrived last wins;
                            // the server owns both and does not contradict itself in practice.
                            let mut next = settings;
                            if let Some(threshold) = vad.threshold_db {
                                if threshold.is_finite() {
                                    next.threshold_db = threshold;
                                }
                            }
                            if let Some(hold) = vad.hold_ms {
                                next.hold_ms = hold;
                            }
                            if next != settings {
                                settings = next;
                                reporter.update_settings(settings);
                            }
                        }
                        if let Some(control) = source.control {
                            // Straight to the hook: what `activate` means is a property of the
                            // hardware on the other end of the cable, not of this client.
                            let name = control_name(control);
                            debug!(client_id = %params.client_id, control = name, "source control");
                            if let Some(hook) = control_hook {
                                hook.run(name).await;
                            }
                        }
                        match source.command {
                            Some(SourceCommandType::Start) => {
                                if !streaming {
                                    info!(client_id = %params.client_id, "server asked for capture");
                                    announce_stream(&sender, params).await?;
                                    reporter.reset();
                                    streaming = true;
                                    status.set_state_ok(STATE_STREAMING);
                                }
                            }
                            Some(SourceCommandType::Stop) => {
                                if streaming && !params.always_on {
                                    info!(client_id = %params.client_id, "server asked us to stop");
                                    streaming = false;
                                    sender.send_input_stream_end().await?;
                                    reporter.reset();
                                    send_state(&sender, SourceStateType::Idle, 0.0, SourceSignal::Absent).await?;
                                    status.set_state_ok(STATE_CONNECTED);
                                    status.set_signal(0.0, signal_name(SourceSignal::Absent));
                                }
                            }
                            _ => {}
                        }
                    }
                    Message::InputStreamRequestFormat(request) => {
                        // One format, taken from the capture device we opened. Re-announcing is the
                        // honest answer: the server learns nothing changed instead of waiting for a
                        // stream that never comes in the format it asked for.
                        warn!(
                            client_id = %params.client_id,
                            "server requested {:?}; keeping the capture format",
                            request.source
                        );
                        if streaming {
                            announce_stream(&sender, params).await?;
                        }
                    }
                    other => debug!("unhandled sendspin message: {:?}", other),
                }
            }
            chunk = chunk_rx.recv() => {
                let Some(chunk) = chunk else {
                    // The capture callback is gone, which means the card is.
                    return Err(anyhow!("capture stream ended"));
                };
                let level = rms_level(&chunk.pcm, params.bit_depth);
                let state = if streaming {
                    SourceStateType::Streaming
                } else {
                    SourceStateType::Idle
                };

                if streaming {
                    // While the filter has not settled there is no conversion to server time, and a
                    // frame stamped in local time would play at the wrong moment forever. Dropping
                    // the first few frames of a fresh connection is the cheaper mistake.
                    //
                    // The conversion is in its own statement so the clock lock is released before the
                    // send: a guard held across an await is a deadlock waiting for a reason.
                    let server_us = clock_sync.lock().client_to_server_micros(chunk.capture_us);
                    if let Some(server_us) = server_us {
                        sender.send_source_audio(server_us, &chunk.pcm).await?;
                    }
                }

                // Level and signal are reported whether or not we are streaming: line sensing is how
                // the server finds out a turntable started, and it cannot ask.
                if let Some(update) = reporter.observe(level, state) {
                    if let Some(event) = update.event {
                        sender.send_source_event(event).await?;
                    }
                    send_state(&sender, state, level, update.signal).await?;
                    status.set_signal(level, signal_name(update.signal));
                } else {
                    status.set_level(level);
                }
            }
        }
    }

    info!(client_id = %params.client_id, "sendspin source session ended");
    status.set_state(STATE_IDLE);
    Ok(())
}

async fn announce_stream(sender: &WsSender, params: &SourceParams) -> Result<()> {
    let format = params.format();
    sender
        .send_input_stream_start(InputStreamSource {
            codec: format.codec,
            channels: format.channels,
            sample_rate: format.sample_rate,
            bit_depth: format.bit_depth,
            codec_header: None,
        })
        .await
        .context("send input_stream/start")?;
    send_state(
        sender,
        SourceStateType::Streaming,
        0.0,
        SourceSignal::Unknown,
    )
    .await
}

async fn send_state(
    sender: &WsSender,
    state: SourceStateType,
    level: f32,
    signal: SourceSignal,
) -> Result<()> {
    sender
        .send_source_state(SourceState {
            state,
            level: Some(level),
            signal: Some(signal),
        })
        .await
        .context("send source state")
}

/// Open the capture device and hand finished chunks to the async side.
///
/// The returned stream must be kept alive; dropping it closes the device.
fn start_capture(
    device: &cpal::Device,
    params: &SourceParams,
    clock: std::sync::Arc<dyn Clock>,
    tx: mpsc::Sender<CapturedChunk>,
) -> Result<cpal::Stream> {
    let config = cpal::StreamConfig {
        channels: params.channels.try_into().unwrap_or(2),
        sample_rate: params.sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let bytes_per_sample = usize::from(params.bit_depth) / 8;
    let chunk_bytes = params.frames_per_chunk() * usize::from(params.channels) * bytes_per_sample;
    let chunk_duration = params.chunk_duration();

    let mut buffer: Vec<u8> = Vec::with_capacity(chunk_bytes * 2);
    let mut dropped: u64 = 0;
    let stream = device
        .build_input_stream(
            config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                for sample in data {
                    buffer.extend_from_slice(&sample.to_le_bytes());
                }
                while buffer.len() >= chunk_bytes {
                    let pcm: Vec<u8> = buffer.drain(..chunk_bytes).collect();
                    // The callback runs after its samples were taken, so the first sample of this
                    // chunk is one chunk-duration in the past. Stamping "now" would drift the whole
                    // stream late by exactly one frame.
                    let capture_us = clock
                        .now_micros()
                        .saturating_sub(chunk_duration.as_micros() as i64);
                    if tx.try_send(CapturedChunk { capture_us, pcm }).is_err() {
                        dropped = dropped.saturating_add(1);
                        // Logged from the audio callback only occasionally: this path runs hundreds
                        // of times a second and a log line per drop would make the xrun worse.
                        if dropped % 100 == 1 {
                            warn!("capture queue full; dropped {} chunk(s)", dropped);
                        }
                    }
                }
            },
            |err| warn!("capture stream error: {}", err),
            None,
        )
        .with_context(|| {
            format!(
                "open capture at {} Hz, {} channels, {}-bit",
                params.sample_rate, params.channels, params.bit_depth
            )
        })?;
    Ok(stream)
}

/// Normalised RMS of a 16-bit little-endian PCM buffer.
fn rms_level(pcm: &[u8], bit_depth: u8) -> f32 {
    if bit_depth != 16 || pcm.len() < 2 {
        return 0.0;
    }
    let mut acc = 0f64;
    let mut count = 0u32;
    for frame in pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([frame[0], frame[1]]) as f64;
        acc += sample * sample;
        count += 1;
    }
    if count == 0 {
        return 0.0;
    }
    let rms = (acc / f64::from(count)).sqrt();
    ((rms / f64::from(i16::MAX)) as f32).min(1.0)
}

struct SignalUpdate {
    signal: SourceSignal,
    /// Set when the transition is worth telling the server about as an event.
    event: Option<SourceClientCommandType>,
}

/// Turns a stream of levels into signal presence, and presence changes into events.
///
/// The hold is what makes this usable on real audio: a quiet passage is not the end of a record, and
/// a click is not the start of one. A change has to persist before it counts.
struct SignalReporter {
    settings: SignalSettings,
    threshold: f32,
    last_signal: Option<SourceSignal>,
    last_state: Option<SourceStateType>,
    candidate: Option<SourceSignal>,
    candidate_since: Option<std::time::Instant>,
}

impl SignalReporter {
    fn new(settings: SignalSettings) -> Self {
        Self {
            settings,
            threshold: settings.linear(),
            last_signal: None,
            last_state: None,
            candidate: None,
            candidate_since: None,
        }
    }

    fn update_settings(&mut self, settings: SignalSettings) {
        self.settings = settings;
        self.threshold = settings.linear();
        // Drop any pending candidate: it was being timed against the old hold.
        self.candidate = None;
        self.candidate_since = None;
    }

    fn reset(&mut self) {
        self.last_signal = None;
        self.last_state = None;
        self.candidate = None;
        self.candidate_since = None;
    }

    /// Feed one measurement. Returns something only when the server needs to hear about it.
    fn observe(&mut self, level: f32, state: SourceStateType) -> Option<SignalUpdate> {
        let raw = if level >= self.threshold {
            SourceSignal::Present
        } else {
            SourceSignal::Absent
        };
        let previous = self.last_signal;
        let mut event = None;

        if Some(raw) != previous {
            let hold = Duration::from_millis(self.settings.hold_ms);
            let now = std::time::Instant::now();
            match (self.candidate, self.candidate_since) {
                (Some(candidate), Some(since)) if candidate == raw => {
                    if now.duration_since(since) >= hold {
                        // A first observation is not a transition: nothing has changed yet, so it
                        // sets the baseline instead of announcing a start that never happened.
                        if previous.is_some() {
                            event = Some(match raw {
                                SourceSignal::Present => SourceClientCommandType::Started,
                                _ => SourceClientCommandType::Stopped,
                            });
                        }
                        self.last_signal = Some(raw);
                        self.candidate = None;
                        self.candidate_since = None;
                    }
                }
                _ => {
                    self.candidate = Some(raw);
                    self.candidate_since = Some(now);
                }
            }
        } else {
            self.candidate = None;
            self.candidate_since = None;
        }

        if self.last_signal.is_none() {
            self.last_signal = Some(raw);
        }
        let signal = self.last_signal.unwrap_or(raw);
        let state_changed = self.last_state != Some(state);
        let signal_changed = previous != Some(signal);
        self.last_state = Some(state);
        (state_changed || signal_changed || event.is_some()).then_some(SignalUpdate {
            signal,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm16(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn silence_measures_zero_and_full_scale_measures_one() {
        assert_eq!(rms_level(&pcm16(&[0, 0, 0, 0]), 16), 0.0);
        let full = rms_level(&pcm16(&[i16::MAX, i16::MAX, i16::MAX]), 16);
        assert!((full - 1.0).abs() < 0.001, "got {}", full);
    }

    #[test]
    fn a_threshold_in_db_becomes_a_linear_amplitude() {
        let settings = SignalSettings {
            threshold_db: -20.0,
            hold_ms: 0,
        };
        assert!((settings.linear() - 0.1).abs() < 0.0001);
    }

    #[test]
    fn the_first_observation_sets_a_baseline_without_claiming_a_start() {
        let mut reporter = SignalReporter::new(SignalSettings {
            threshold_db: -40.0,
            hold_ms: 0,
        });
        let update = reporter
            .observe(0.5, SourceStateType::Streaming)
            .expect("first observation reports state");
        assert!(
            update.event.is_none(),
            "audio that was already playing did not just start"
        );
        assert_eq!(update.signal, SourceSignal::Present);
    }

    #[test]
    fn a_change_must_outlast_the_hold_before_it_counts() {
        let mut reporter = SignalReporter::new(SignalSettings {
            threshold_db: -40.0,
            hold_ms: 60_000,
        });
        let _ = reporter.observe(0.0, SourceStateType::Idle);
        // Loud now, but nowhere near a minute of it: the candidate is still being timed.
        for _ in 0..5 {
            let update = reporter.observe(0.9, SourceStateType::Idle);
            assert!(update.is_none_or(|u| u.event.is_none()));
        }
        assert_eq!(reporter.last_signal, Some(SourceSignal::Absent));
    }

    #[test]
    fn a_sustained_change_produces_one_event() {
        let mut reporter = SignalReporter::new(SignalSettings {
            threshold_db: -40.0,
            hold_ms: 0,
        });
        let _ = reporter.observe(0.0, SourceStateType::Idle);
        let update = reporter
            .observe(0.9, SourceStateType::Idle)
            .expect("a transition is reported");
        assert!(matches!(
            update.event,
            Some(SourceClientCommandType::Started)
        ));
        // Still loud: nothing new to say.
        assert!(reporter
            .observe(0.9, SourceStateType::Idle)
            .is_none_or(|u| u.event.is_none()));
    }

    #[test]
    fn known_controls_parse_and_unknown_ones_do_not() {
        assert_eq!(parse_control("Activate"), Some(SourceControl::Activate));
        assert_eq!(parse_control(" next "), Some(SourceControl::Next));
        assert_eq!(parse_control("teleport"), None);
    }
}
