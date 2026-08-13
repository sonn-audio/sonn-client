//! Prove the Bluetooth audio path without a phone.
//!
//! Everything downstream of the radio -- the decoder's output, the source that carries it, the
//! server recognising the room's Bluetooth by its client id -- can be exercised with any audio at
//! all. This sends a few seconds of it and stops, which is exactly what a phone does.
//!
//! `cargo run --example bluetooth_selftest -- ws://server:7090/sendspin <client-id> [seconds] [hz]`
//!
//! With no tone frequency it sends digital silence, so the proof can be run in a room where someone
//! is asleep.

use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| {
        eprintln!("usage: bluetooth_selftest <ws-url> <client-id> [seconds] [tone-hz]");
        std::process::exit(2);
    });
    let client_id = args.next().unwrap_or_else(|| "selftest-bt".to_string());
    let seconds: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let tone: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    // The rate matters as much as the audio: a phone sends 44.1 kHz and the server has to carry that
    // to a 48 kHz output for as long as the music lasts. Sending 48 kHz here would test everything
    // except the conversion.
    let sample_rate: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48_000);
    let channels = 2usize;
    let (frames_tx, frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // A tenth of a second at a time, paced against a fixed clock rather than by sleeping. A sleep is
    // always a little longer than asked, and a producer that runs one percent slow starves the room
    // a few seconds in -- which sounds exactly like a stream that is broken, and is not.
    let chunk_frames = (sample_rate / 10) as usize;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    tokio::spawn(async move {
        let mut phase = 0f32;
        for _ in 0..(seconds * 10) {
            let mut chunk = Vec::with_capacity(chunk_frames * channels * 2);
            for _ in 0..chunk_frames {
                let sample = if tone > 0.0 {
                    phase += std::f32::consts::TAU * tone / sample_rate as f32;
                    (phase.sin() * 8000.0) as i16
                } else {
                    0
                };
                for _ in 0..channels {
                    chunk.extend_from_slice(&sample.to_le_bytes());
                }
            }
            if frames_tx.send(chunk).is_err() {
                return;
            }
            tick.tick().await;
        }
        println!("selftest: audio finished; closing the source");
    });

    let mut config = sendspin::source::SourceConfig::new(client_id, "Bluetooth selftest".to_string());
    config.codec = "pcm".to_string();
    config.sample_rate = sample_rate;
    config.channels = channels as u8;
    config.bit_depth = 16;
    let source = sendspin::source::Source::with_frames(config, frames_rx);

    println!("selftest: sending {seconds}s of {sample_rate} Hz to {url}");
    match source.run_outbound(&url, None).await {
        Ok(()) => println!("selftest: the session ended"),
        Err(err) => println!("selftest: the session failed: {err}"),
    }
}
