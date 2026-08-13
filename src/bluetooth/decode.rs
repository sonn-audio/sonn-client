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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// How much PCM to hand on at a time.
///
/// A quarter of a second at 48 kHz stereo: large enough that the pipe is not the bottleneck, small
/// enough that stopping the music stops the sound.
const CHUNK_BYTES: usize = 48_000 * 2 * 2 / 4;

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

    // Frames in.
    tokio::spawn(async move {
        while let Some(frame) = frames_rx.recv().await {
            if stdin.write_all(&frame).await.is_err() {
                break;
            }
        }
        // Closing stdin is how ffmpeg is told the music ended; it then flushes and exits.
        let _ = stdin.shutdown().await;
    });

    // PCM out.
    tokio::spawn(async move {
        let mut buffer = vec![0u8; CHUNK_BYTES];
        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if pcm_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
        debug!("bluetooth: the decoder's output ended");
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_is_a_quarter_second_of_cd_like_audio() {
        // 48 kHz, stereo, 16-bit: 192 000 bytes a second, so a quarter of that.
        assert_eq!(CHUNK_BYTES, 48_000);
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
