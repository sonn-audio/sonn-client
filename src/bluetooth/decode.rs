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

    // PCM out, at the rate it arrives.
    //
    // There is deliberately no pacing here, and that is worth saying because the obvious thing to
    // reach for -- collect the bursts, release them on a timer -- was tried and was wrong. The
    // phone's crystal is the clock for this audio. A twenty millisecond tick of our own runs a
    // fraction of a percent faster or slower than that, and a fraction of a percent is a buffer
    // emptied in half a minute: measured as twenty seconds of perfect sound followed by permanent
    // starvation, with a byte rate that looked right the whole time.
    //
    // What smooths the bursts instead is the size of the reads above and the lead the server
    // already plays behind. Neither invents a second clock.
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
    tokio::spawn(forward(raw_rx, pcm_tx, bytes_per_second));

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

/// How often the flow is said out loud.
const REPORT_SECONDS: u64 = 10;

/// Hand the audio on as it comes, and say how much of it there was.
///
/// The report is the point of this being a function at all: a stream that is a few percent short of
/// real time sounds broken while every average looks right, so the milliseconds are counted against
/// the wall clock where anyone can see them.
async fn forward(
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    bytes_per_second: usize,
) {
    let mut sent = 0usize;
    let mut last_report = tokio::time::Instant::now();
    while let Some(chunk) = frames.recv().await {
        sent += chunk.len();
        if out.send(chunk).is_err() {
            return;
        }
        let elapsed = last_report.elapsed();
        if elapsed >= Duration::from_secs(REPORT_SECONDS) {
            let carried = sent * 1000 / bytes_per_second.max(1);
            let real = elapsed.as_millis().max(1) as usize;
            info!(
                "bluetooth: {carried} ms of audio in {real} ms ({}%)",
                carried * 100 / real
            );
            sent = 0;
            last_report = tokio::time::Instant::now();
        }
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
    async fn audio_is_handed_on_untouched_and_in_order() {
        // Nothing is held back, nothing is invented, nothing is reordered: the phone's clock is the
        // only clock this stream has.
        let (tx, rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(forward(rx, out_tx, 48_000 * 2 * 2));
        tx.send(vec![1u8; 8]).expect("the forwarder is listening");
        tx.send(vec![2u8; 8]).expect("the forwarder is listening");
        drop(tx);

        let mut out = Vec::new();
        while let Some(chunk) = out_rx.recv().await {
            out.extend(chunk);
        }
        assert_eq!(out, [vec![1u8; 8], vec![2u8; 8]].concat());
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
