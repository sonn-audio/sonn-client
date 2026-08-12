//! Turning a configured input into a Sendspin source.
//!
//! The session is the crate's. `sendspin::source::Source` opens the card, stamps every chunk in the
//! server's clock, encodes it and answers the server's start and stop. What stays here is the one
//! thing the protocol deliberately leaves to the application: deciding whether there is actually
//! audio on the input.
//!
//! That decision is a policy, not a fact. The crate measures the level and refuses to pick a
//! threshold, because the reference implementation does not pick one either -- and a threshold
//! chosen in a library would be an invention wearing the protocol's name. This device has somewhere
//! to get one from: the server configures it per input, along with how long a change has to last.

use crate::models::DesiredSource;
use crate::status::{SourceHandle, STATE_CONNECTED, STATE_CONNECTING, STATE_IDLE, STATE_STREAMING};
use anyhow::{anyhow, Result};
use sendspin::audio::devices::find_input_device;
use sendspin::protocol::messages::SourceSignal;
use sendspin::source::{ConnectionState, Source, SourceConfig, SourceStatus};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// Capture defaults when the server names none. 48 kHz/16-bit stereo is what every USB interface and
/// every Pi HAT does without argument.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHANNELS: u8 = 2;
pub const DEFAULT_BIT_DEPTH: u8 = 16;
/// PCM by default. The server transcodes centrally anyway, and encoding on the device would spend a
/// Pi's CPU to save bandwidth on a LAN that has plenty.
pub const DEFAULT_CODEC: &str = "pcm";
/// Silence threshold when the server names none, matching the line-in bridge's default.
const DEFAULT_THRESHOLD_DB: f32 = -45.0;
const DEFAULT_HOLD_MS: u64 = 2_000;
/// How long to wait before dialling again. The crate doubles this up to a minute.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Signal-detection settings, as the server configures them per input.
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
    pub fn from_desired(source: &DesiredSource) -> Self {
        Self {
            threshold_db: source.threshold_db.unwrap_or(DEFAULT_THRESHOLD_DB),
            hold_ms: source.hold_ms.unwrap_or(DEFAULT_HOLD_MS),
        }
    }

    /// Linear amplitude the dBFS threshold corresponds to.
    fn linear(&self) -> f32 {
        10f32.powf(self.threshold_db / 20.0)
    }
}

/// Build a source for one desired input.
pub fn build(desired: &DesiredSource, name: String) -> Result<Source> {
    let mut config = SourceConfig::new(desired.client_id.clone(), name);

    if let Some(id) = desired
        .input
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        config.device =
            Some(find_input_device(id).map_err(|err| anyhow!("capture device {}: {}", id, err))?);
    }

    config.codec = desired
        .codec
        .clone()
        .unwrap_or_else(|| DEFAULT_CODEC.to_string());
    config.sample_rate = desired.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE);
    config.bit_depth = desired.bit_depth.unwrap_or(DEFAULT_BIT_DEPTH);
    config.channels = desired
        .channels
        .and_then(|channels| u8::try_from(channels).ok())
        .unwrap_or(DEFAULT_CHANNELS);
    // Only claimed because this client does watch the level. A source that advertises the feature
    // and then never reports leaves the server waiting on a promise.
    config.line_sense = true;

    Ok(Source::new(config))
}

/// Run one source until it is told to stop, deciding signal presence as the levels come in.
pub async fn run(
    source: Source,
    url: String,
    status: SourceHandle,
    mut settings_rx: watch::Receiver<SignalSettings>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut policy = SignalPolicy::new(*settings_rx.borrow_and_update());
    let reporter = source.signal_reporter();
    let mut levels = source.levels();
    let mut source_status = source.status();
    report(&source_status.borrow(), &status);

    let session = source.run_outbound(&url, Some(RECONNECT_DELAY));
    tokio::pin!(session);

    loop {
        tokio::select! {
            outcome = &mut session => {
                if let Err(err) = outcome {
                    warn!("source session ended: {}", err);
                    status.set_error(err.to_string());
                }
                return;
            }
            Ok(()) = levels.changed() => {
                let level = *levels.borrow_and_update();
                if let Some(signal) = policy.observe(level) {
                    info!("input signal {}", signal_name(signal));
                    reporter.report(signal);
                }
                status.set_signal(level, signal_name(policy.signal()));
            }
            Ok(()) = source_status.changed() => {
                report(&source_status.borrow(), &status);
            }
            Ok(()) = settings_rx.changed() => {
                policy.update(*settings_rx.borrow_and_update());
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

/// Turns a stream of levels into signal presence.
///
/// The hold is what makes this usable on real audio: a quiet passage is not the end of a record, and
/// a click is not the start of one. A change has to persist before it counts.
struct SignalPolicy {
    settings: SignalSettings,
    threshold: f32,
    signal: SourceSignal,
    settled: bool,
    candidate: Option<(SourceSignal, Instant)>,
}

impl SignalPolicy {
    fn new(settings: SignalSettings) -> Self {
        Self {
            settings,
            threshold: settings.linear(),
            signal: SourceSignal::Absent,
            settled: false,
            candidate: None,
        }
    }

    fn update(&mut self, settings: SignalSettings) {
        self.settings = settings;
        self.threshold = settings.linear();
        // Drop any pending candidate: it was being timed against the old hold.
        self.candidate = None;
    }

    fn signal(&self) -> SourceSignal {
        self.signal
    }

    /// Feed one measurement. Returns a signal only when the server needs to hear about it.
    fn observe(&mut self, level: f32) -> Option<SourceSignal> {
        let raw = if level >= self.threshold {
            SourceSignal::Present
        } else {
            SourceSignal::Absent
        };

        // The first measurement is not a transition. It settles the baseline and is reported once,
        // so a server that connects to a turntable already playing is not told the input is silent.
        if !self.settled {
            self.settled = true;
            self.signal = raw;
            return Some(raw);
        }

        if raw == self.signal {
            self.candidate = None;
            return None;
        }

        let hold = Duration::from_millis(self.settings.hold_ms);
        let now = Instant::now();
        match self.candidate {
            Some((candidate, since)) if candidate == raw && now.duration_since(since) >= hold => {
                self.signal = raw;
                self.candidate = None;
                Some(raw)
            }
            Some((candidate, _)) if candidate == raw => None,
            _ => {
                self.candidate = Some((raw, now));
                None
            }
        }
    }
}

/// Copy what the source says about itself into what this device reports upstream.
fn report(from: &SourceStatus, to: &SourceHandle) {
    match &from.last_error {
        Some(error) => to.set_error(error.clone()),
        None => to.set_state_ok(state_name(from.connection)),
    }
    // Only ever set, never cleared: the format a source announced is what it is capturing, and a
    // reconnect that has not announced one yet has not stopped capturing in that format.
    if let Some(format) = &from.format {
        to.set_format(
            &format.codec,
            format.sample_rate,
            format.bit_depth,
            u16::from(format.channels),
        );
    }
}

fn state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => STATE_IDLE,
        ConnectionState::Connecting => STATE_CONNECTING,
        ConnectionState::Connected => STATE_CONNECTED,
        ConnectionState::Streaming => STATE_STREAMING,
    }
}

fn signal_name(signal: SourceSignal) -> &'static str {
    match signal {
        SourceSignal::Present => "present",
        SourceSignal::Absent => "absent",
        SourceSignal::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(hold_ms: u64) -> SignalPolicy {
        SignalPolicy::new(SignalSettings {
            threshold_db: -40.0,
            hold_ms,
        })
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
    fn the_first_measurement_settles_the_baseline_and_is_reported() {
        // Audio that was already playing when this connected still has to reach the server, or a
        // turntable mid-record shows as a silent input until the next time it changes.
        let mut policy = policy(0);
        assert_eq!(policy.observe(0.9), Some(SourceSignal::Present));
        assert_eq!(policy.observe(0.9), None, "nothing changed");
    }

    #[test]
    fn a_change_must_outlast_the_hold_before_it_counts() {
        let mut policy = policy(60_000);
        assert_eq!(policy.observe(0.0), Some(SourceSignal::Absent));
        // Loud now, but nowhere near a minute of it: the candidate is still being timed.
        for _ in 0..5 {
            assert_eq!(policy.observe(0.9), None);
        }
        assert_eq!(policy.signal(), SourceSignal::Absent);
    }

    #[test]
    fn a_sustained_change_is_reported_once() {
        let mut policy = policy(0);
        assert_eq!(policy.observe(0.0), Some(SourceSignal::Absent));
        // With no hold, the observation after the one that saw the change confirms it.
        assert_eq!(policy.observe(0.9), None);
        assert_eq!(policy.observe(0.9), Some(SourceSignal::Present));
        assert_eq!(policy.observe(0.9), None, "and not again");
    }

    #[test]
    fn a_settings_change_drops_a_candidate_timed_against_the_old_hold() {
        let mut policy = policy(60_000);
        assert_eq!(policy.observe(0.0), Some(SourceSignal::Absent));
        assert_eq!(policy.observe(0.9), None);
        policy.update(SignalSettings {
            threshold_db: -40.0,
            hold_ms: 0,
        });
        // The candidate started again under the new hold rather than inheriting the old timing.
        assert_eq!(policy.observe(0.9), None);
        assert_eq!(policy.observe(0.9), Some(SourceSignal::Present));
    }
}
