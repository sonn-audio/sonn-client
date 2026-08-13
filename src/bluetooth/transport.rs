//! The audio itself: taking the socket bluez offers and reading what arrives on it.
//!
//! Once a phone starts playing, the transport goes to `active` and `Acquire()` hands back a socket
//! carrying RTP packets. Each packet is a small RTP header followed by an A2DP payload header and
//! then whole SBC frames -- several per packet, because a frame is only a few dozen bytes and one
//! packet is worth a few milliseconds.
//!
//! What this does *not* do is decode. That is the next piece, and it is deliberately a seam: the
//! frames come out of here whole, so a decoder can be dropped in, replaced, or moved out of the
//! process without anything above it noticing.

use anyhow::{anyhow, Context, Result};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// The RTP header every A2DP packet starts with, before the payload header.
const RTP_HEADER: usize = 12;
/// A2DP's own byte after it: how many SBC frames are in this packet.
const A2DP_HEADER: usize = 1;

/// How much of the stream has arrived, for whoever is watching.
#[derive(Debug, Default)]
pub struct StreamCounters {
    pub packets: AtomicU64,
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
}

impl StreamCounters {
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// One RTP packet's worth of SBC.
#[derive(Debug, PartialEq, Eq)]
pub struct Payload<'a> {
    /// How many SBC frames the sender says are in here.
    pub frames: u8,
    /// The frames themselves, back to back.
    pub data: &'a [u8],
}

/// Split an A2DP packet into the frames it carries.
///
/// Returns `None` for anything too short to be one, which on a socket that is also carrying
/// keep-alives is not an error worth logging every few milliseconds.
pub fn payload(packet: &[u8]) -> Option<Payload<'_>> {
    if packet.len() <= RTP_HEADER + A2DP_HEADER {
        return None;
    }
    // The RTP header can carry contributing-source identifiers; the count is in the first byte and
    // each one is four bytes. Skipping them by hand is the difference between decoding audio and
    // decoding the header of the next frame.
    let csrc_count = usize::from(packet[0] & 0x0F);
    let extension = packet[0] & 0x10 != 0;
    let mut offset = RTP_HEADER + csrc_count * 4;
    if extension {
        // A header extension is a two-byte profile field, a two-byte length in 32-bit words, then
        // that many words.
        if packet.len() < offset + 4 {
            return None;
        }
        let words = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
        offset += 4 + words * 4;
    }
    if packet.len() <= offset + A2DP_HEADER {
        return None;
    }
    let frames = packet[offset] & 0x0F;
    Some(Payload {
        frames,
        data: &packet[offset + A2DP_HEADER..],
    })
}

/// Read a transport socket until the phone stops or the transport goes away.
///
/// The socket is a sequenced-packet one, so every read is exactly one RTP packet -- there is no
/// framing to do and a short read is a packet, not a fragment.
pub fn read_stream(
    fd: OwnedFd,
    read_mtu: u16,
    counters: Arc<StreamCounters>,
    frames: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
) -> Result<()> {
    let mut buffer = vec![0u8; usize::from(read_mtu).max(1024)];
    let raw = fd.as_raw_fd();
    // bluez hands the socket over non-blocking, and this reader has a thread of its own precisely so
    // it can wait. Left as it comes, the first read returns EAGAIN before the phone has sent
    // anything and the stream looks like it ended a millisecond after it started.
    make_blocking(raw).context("make the transport socket blocking")?;
    info!("bluetooth: reading audio, up to {read_mtu} bytes per packet");

    loop {
        let read = unsafe { libc_recv(raw, buffer.as_mut_ptr(), buffer.len()) };
        match read {
            0 => {
                debug!("bluetooth: the phone closed the stream");
                return Ok(());
            }
            n if n < 0 => {
                let err = std::io::Error::last_os_error();
                // A signal, or a socket that says "not yet" despite being blocking: neither is the
                // end of the music.
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(anyhow!(err).context("read the bluetooth transport"));
            }
            n => {
                let packet = &buffer[..n as usize];
                counters.packets.fetch_add(1, Ordering::Relaxed);
                counters
                    .bytes
                    .fetch_add(packet.len() as u64, Ordering::Relaxed);
                match payload(packet) {
                    Some(payload) => {
                        counters
                            .frames
                            .fetch_add(u64::from(payload.frames), Ordering::Relaxed);
                        if let Some(frames) = frames.as_ref() {
                            // The decoder going away is the stream ending, not an error to shout
                            // about: it is dropped when the phone stops.
                            if frames.send(payload.data.to_vec()).is_err() {
                                debug!("bluetooth: nothing is decoding any more");
                                return Ok(());
                            }
                        }
                    }
                    None => warn!("bluetooth: a packet of {} bytes carried no audio", packet.len()),
                }
            }
        }
    }
}

extern "C" {
    #[link_name = "recv"]
    fn libc_recv_raw(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    #[link_name = "fcntl"]
    fn libc_fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
}

/// `F_GETFL` / `F_SETFL` with `O_NONBLOCK` cleared.
fn make_blocking(fd: i32) -> Result<()> {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: i32 = 0o4000;
    let flags = unsafe { libc_fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return Err(anyhow!(std::io::Error::last_os_error()));
    }
    if flags & O_NONBLOCK == 0 {
        return Ok(());
    }
    if unsafe { libc_fcntl(fd, F_SETFL, flags & !O_NONBLOCK) } < 0 {
        return Err(anyhow!(std::io::Error::last_os_error()));
    }
    Ok(())
}

unsafe fn libc_recv(fd: i32, buf: *mut u8, len: usize) -> isize {
    libc_recv_raw(fd, buf, len, 0)
}

/// Acquire the socket for a transport that has gone active.
pub async fn acquire(
    connection: &zbus::Connection,
    path: &zbus::zvariant::OwnedObjectPath,
) -> Result<(OwnedFd, u16)> {
    let transport = super::endpoint::MediaTransportProxy::builder(connection)
        .path(path.clone())?
        .build()
        .await
        .context("talk to the transport")?;
    let (fd, read_mtu, _write_mtu) = transport
        .acquire()
        .await
        .context("acquire the bluetooth transport")?;
    Ok((OwnedFd::from(fd), read_mtu))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain packet: version 2, no CSRCs, no extension, then one SBC frame's worth of bytes.
    fn packet(first_byte: u8, after_header: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; RTP_HEADER];
        out[0] = first_byte;
        out.extend_from_slice(after_header);
        out
    }

    #[test]
    fn the_frames_come_out_whole() {
        // The A2DP header says five frames; everything after it is audio.
        let raw = packet(0x80, &[0x05, 1, 2, 3, 4]);
        let payload = payload(&raw).expect("a payload");
        assert_eq!(payload.frames, 5);
        assert_eq!(payload.data, &[1, 2, 3, 4]);
    }

    #[test]
    fn contributing_sources_are_skipped() {
        // Two CSRCs: eight bytes between the header and the audio. Reading them as audio would hand
        // the decoder someone else's identifiers and lose sync on every packet.
        let mut raw = packet(0x82, &[0; 8]);
        raw.extend_from_slice(&[0x03, 9, 9]);
        let payload = payload(&raw).expect("a payload");
        assert_eq!(payload.frames, 3);
        assert_eq!(payload.data, &[9, 9]);
    }

    #[test]
    fn a_header_extension_is_skipped_by_its_own_length() {
        // Extension bit set, one 32-bit word of extension.
        let mut raw = packet(0x90, &[0xBE, 0xDE, 0x00, 0x01, 7, 7, 7, 7]);
        raw.extend_from_slice(&[0x02, 5, 5]);
        let payload = payload(&raw).expect("a payload");
        assert_eq!(payload.frames, 2);
        assert_eq!(payload.data, &[5, 5]);
    }

    #[test]
    fn anything_too_short_to_be_audio_is_not_audio() {
        assert!(payload(&[]).is_none());
        assert!(payload(&vec![0u8; RTP_HEADER]).is_none());
        // A header and a frame count, but nothing behind it.
        assert!(payload(&packet(0x80, &[0x01])).is_none());
        // An extension that claims more than the packet holds.
        assert!(payload(&packet(0x90, &[0xBE, 0xDE, 0xFF, 0xFF, 1])).is_none());
    }
}
