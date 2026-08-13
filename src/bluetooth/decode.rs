//! Turning what a phone sends into audio, with ffmpeg.
//!
//! The frames arrive already encoded -- SBC today, and whatever else the endpoint agrees to
//! tomorrow -- so nothing here decodes anything itself. ffmpeg does, and it is the right tool for
//! three reasons: it is on every one of these boards already, its decoders are the ones everybody
//! else's audio goes through, and it costs this binary no dependency at all. A Rust crate would
//! have meant trusting one author's subband maths; a C library would have meant carrying it into
//! four cross-compiled targets.
//!
//! What comes out is PCM, and PCM is what a sendspin source may announce. That matters more than it
//! sounds: the protocol names three codecs -- pcm, flac, opus -- and forwarding SBC over it would
//! have been a private extension that no other server could read.

use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// How much PCM to hand on at a time.
///
/// About twenty milliseconds at 48 kHz stereo, and deliberately small. This is a live stream with
/// no clock of its own: the server holds a quarter to a third of a second of it and plays from the
/// far end of that. Handing over quarter-second lumps -- which is what a comfortable read size
/// works out to -- makes the arrival jitter as large as the whole buffer, and one lump that is
/// slightly late empties it. Measured on a phone: 290 ms of jitter against a 250 ms floor, and the
/// lead walking down to it over twenty seconds.
///
/// Smaller reads cost a few more syscalls and buy a stream that arrives the way it was played.
const CHUNK_BYTES: usize = 48_000 * 2 * 2 / 50;

/// A running decoder: frames in, PCM out.
pub struct Decoder {
    child: Child,
    /// Where encoded frames are handed in.
    pub frames: mpsc::UnboundedSender<Vec<u8>>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // The music stopped; so should the decoder. Killing it closes both pipes, which is what
        // ends the two tasks feeding and draining it.
        let _ = self.child.start_kill();
    }
}

/// Start ffmpeg for one stream, and hand back where to put frames and where PCM comes out.
///
/// `codec` is the name ffmpeg knows the incoming format by (`sbc` today). The output format is the
/// one the source announces, so what the server receives needs no further conversion.
pub fn spawn(
    codec: &str,
    sample_rate: u32,
    channels: u8,
) -> Result<(Decoder, mpsc::UnboundedReceiver<Vec<u8>>)> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            // Raw frames, not a container: there is no file here, only what came off the air.
            "-f",
            codec,
            "-i",
            "pipe:0",
            "-f",
            "s16le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start ffmpeg (is it installed?)")?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ffmpeg gave no stdin"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg gave no stdout"))?;
    let stderr = child.stderr.take();

    let (frames_tx, mut frames_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (pcm_tx, pcm_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // With `SONN_BT_DUMP=/some/prefix`, both sides of the decoder are written to disk: `.sbc` is
    // exactly what came off the air, `.raw` exactly what went to the server. When a room says the
    // sound is bad, this is the difference between guessing and knowing which half is at fault.
    let dump = std::env::var("SONN_BT_DUMP").ok().filter(|p| !p.is_empty());
    let mut dump_in = dump
        .as_ref()
        .and_then(|prefix| std::fs::File::create(format!("{prefix}.sbc")).ok());
    let mut dump_out = dump
        .as_ref()
        .and_then(|prefix| std::fs::File::create(format!("{prefix}.raw")).ok());
    if let Some(prefix) = dump.as_deref() {
        info!("bluetooth: writing both sides of the decoder to {prefix}.sbc/.raw");
    }

    // Frames in.
    tokio::spawn(async move {
        while let Some(frame) = frames_rx.recv().await {
            if let Some(file) = dump_in.as_mut() {
                use std::io::Write;
                let _ = file.write_all(&frame);
            }
            if stdin.write_all(&frame).await.is_err() {
                break;
            }
        }
        // Closing stdin is how ffmpeg is told the music ended; it then flushes and exits.
        let _ = stdin.shutdown().await;
    });

    // PCM out, through a buffer that hands it on at a steady rate.
    //
    // Bluetooth does not arrive evenly. A phone sends in bursts of a few dozen milliseconds, a
    // retransmission or a busy moment on this board pushes a burst late, and the server -- which
    // plays from a lead of a quarter to a third of a second -- has nothing to absorb that with.
    // Measured against a phone: bursts averaging 39 ms with spikes past 350 ms, the lead walking
    // into its floor, and the room hearing it.
    //
    // So the audio is collected here and released on a clock. What goes out is smooth by
    // construction; what the radio does stays behind this buffer.
    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut buffer = vec![0u8; CHUNK_BYTES];
        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Some(file) = dump_out.as_mut() {
                        use std::io::Write;
                        let _ = file.write_all(&buffer[..read]);
                    }
                    if raw_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        debug!("bluetooth: the decoder's output ended");
    });

    let bytes_per_second =
        usize::try_from(sample_rate).unwrap_or(48_000) * usize::from(channels) * 2;
    tokio::spawn(pace(raw_rx, pcm_tx, bytes_per_second));

    // ffmpeg says why it stopped on stderr, and a decoder that quietly does nothing is the worst
    // kind of silence.
    if let Some(mut stderr) = stderr {
        tokio::spawn(async move {
            let mut text = String::new();
            if stderr.read_to_string(&mut text).await.is_ok() && !text.trim().is_empty() {
                warn!("bluetooth: ffmpeg said: {}", text.trim());
            }
        });
    }

    info!("bluetooth: decoding {codec} to {sample_rate} Hz {channels}ch PCM");
    Ok((
        Decoder {
            child,
            frames: frames_tx,
        },
        pcm_rx,
    ))
}

/// How much audio to gather before letting any of it out.
///
/// Enough to ride out the bursts that were measured, and no more: every millisecond here is a
/// millisecond between pressing play on a phone and hearing it in the room.
const PREROLL_MS: usize = 150;
/// The most that may pile up before the oldest is dropped.
///
/// A buffer that only ever grows is latency that never comes back. A phone that runs slightly fast
/// would otherwise push the room further behind for as long as the music lasts.
const CEILING_MS: usize = 400;
/// How often audio is handed on.
const TICK_MS: usize = 20;

/// Hand audio on at the rate it is meant to be played, whatever rate it arrives at.
async fn pace(
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    bytes_per_second: usize,
) {
    let per_tick = (bytes_per_second * TICK_MS / 1000).max(4);
    let preroll = bytes_per_second * PREROLL_MS / 1000;
    let ceiling = bytes_per_second * CEILING_MS / 1000;
    let mut held: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut started = false;
    let mut filled = 0usize;
    let mut ticker = tokio::time::interval(Duration::from_millis(TICK_MS as u64));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    loop {
        tokio::select! {
            chunk = frames.recv() => {
                let Some(chunk) = chunk else { break };
                held.extend(chunk);
                if held.len() > ceiling {
                    // Late is worse than short: drop what is oldest and keep the room close to the
                    // phone rather than carrying a delay that never comes back.
                    let excess = held.len() - ceiling;
                    held.drain(..excess);
                    debug!("bluetooth: dropped {excess} bytes to stay close to the phone");
                }
                if !started && held.len() >= preroll {
                    started = true;
                }
            }
            _ = ticker.tick(), if started => {
                if held.len() < per_tick {
                    // Nothing to send: the phone is behind. Filling the gap with silence keeps the
                    // timeline whole, which is what the server places frames against.
                    filled += per_tick;
                    if out.send(vec![0u8; per_tick]).is_err() {
                        return;
                    }
                    continue;
                }
                let chunk: Vec<u8> = held.drain(..per_tick).collect();
                if out.send(chunk).is_err() {
                    return;
                }
            }
        }
    }

    // Whatever is left is real audio; the tail belongs to the room too.
    while held.len() >= per_tick {
        let chunk: Vec<u8> = held.drain(..per_tick).collect();
        if out.send(chunk).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(TICK_MS as u64)).await;
    }
    if filled > 0 {
        info!(
            "bluetooth: filled {} ms of gaps while the phone was behind",
            filled * 1000 / bytes_per_second.max(1)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_is_about_twenty_milliseconds() {
        // 48 kHz, stereo, 16-bit: 192 000 bytes a second. Small enough that the room's buffer is
        // never asked to absorb a lump of its own size.
        assert_eq!(CHUNK_BYTES, 3_840);
        assert_eq!(CHUNK_BYTES * 50, 48_000 * 2 * 2);
    }

    #[tokio::test]
    async fn bursts_go_in_and_a_steady_stream_comes_out() {
        // One second of audio handed over in a single lump, as Bluetooth does on a bad moment.
        let (tx, rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let rate = 48_000 * 2 * 2;
        tokio::spawn(pace(rx, out_tx, rate));
        tx.send(vec![7u8; rate]).expect("the pacer is listening");

        // Nothing comes out before its time: a lump in is not a lump out.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut got = 0;
        while let Ok(chunk) = out_rx.try_recv() {
            assert_eq!(chunk.len(), rate * TICK_MS / 1000);
            got += chunk.len();
        }
        // About half a second of audio: the point is that a one-second lump did not come straight
        // back out. Real time here, so the margin is generous on purpose.
        let half = rate / 2;
        let tick = rate * TICK_MS / 1000;
        assert!(got > tick * 5, "hardly anything came out: {got}");
        assert!(got < half + tick * 10, "the lump came straight back out: {got}");
    }

    #[tokio::test]
    async fn a_gap_is_filled_rather_than_left() {
        // A phone that falls behind must not leave a hole in the timeline: the server places frames
        // by their timestamps, so a missing twenty milliseconds shifts everything after it.
        let (tx, rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let rate = 48_000 * 2 * 2;
        tokio::spawn(pace(rx, out_tx, rate));
        tx.send(vec![7u8; rate * PREROLL_MS / 1000]).expect("the pacer is listening");
        tokio::time::sleep(Duration::from_millis(400)).await;

        let mut silence = 0;
        while let Ok(chunk) = out_rx.try_recv() {
            if chunk.iter().all(|byte| *byte == 0) {
                silence += chunk.len();
            }
        }
        assert!(silence > 0, "the gap was left open");
    }

    #[tokio::test]
    async fn a_missing_ffmpeg_is_an_error_with_a_reason() {
        // Nothing here decodes without it, and "no sound" is not a diagnosis.
        let result = spawn("definitely-not-a-codec", 48_000, 2);
        if let Ok((decoder, _)) = result {
            drop(decoder);
            return; // ffmpeg is installed; the codec is rejected by ffmpeg itself, not by us.
        }
        let message = format!("{:#}", result.err().expect("an error"));
        assert!(message.contains("ffmpeg"), "{message}");
    }
}
