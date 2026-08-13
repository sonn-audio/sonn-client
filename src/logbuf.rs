//! The last few hundred log lines, kept in memory so the server can ask for them.
//!
//! A speaker is a box in someone's living room. Reading its log meant an SSH session and a
//! `journalctl`, which the person who owns it cannot do and the person who can should not have to
//! for a question as ordinary as "what did it say when it failed".
//!
//! Deliberately not `journalctl`: shelling out ties this to systemd and to whatever the unit is
//! called, and says nothing at all when the client was started by hand -- which is exactly how it
//! runs while something is being worked out. The lines are taken where they are written instead, so
//! what the server gets is what the client said, however it was started.

use std::collections::VecDeque;
use std::io;
use std::sync::{Mutex, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

/// How many lines are kept. A few hundred covers a startup, a failed pairing or a stretch of audio
/// jitter; keeping more would mean holding a device's whole session in memory for a question nobody
/// may ever ask.
const CAPACITY: usize = 500;

fn buffer() -> &'static Mutex<VecDeque<String>> {
    static BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// The most recent lines, oldest first. `limit` is capped at what is kept.
pub fn lines(limit: usize) -> Vec<String> {
    let Ok(buffer) = buffer().lock() else {
        return Vec::new();
    };
    let take = limit.min(buffer.len());
    buffer.iter().skip(buffer.len() - take).cloned().collect()
}

fn push(line: &str) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    let Ok(mut buffer) = buffer().lock() else {
        return;
    };
    if buffer.len() == CAPACITY {
        buffer.pop_front();
    }
    buffer.push_back(line.to_string());
}

/// Where the second `fmt` layer writes. One log event is one write, so a line is whole by the time
/// it arrives; a write carrying several is split rather than stored as one long line.
pub struct Writer;

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Lossy on purpose: a log line that is not valid UTF-8 is still worth reading, and refusing
        // the write would only lose it.
        for line in String::from_utf8_lossy(buf).split('\n') {
            push(line);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hands the layer a writer. Stateless -- everything lands in the one buffer above.
#[derive(Clone, Copy, Default)]
pub struct MakeLog;

impl<'a> MakeWriter<'a> for MakeLog {
    type Writer = Writer;

    fn make_writer(&'a self) -> Self::Writer {
        Writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_write_of_several_lines_is_stored_as_several() {
        let mut writer = Writer;
        writer.write_all(b"first\nsecond\n").expect("the write");
        let lines = lines(10);
        assert!(lines.contains(&"first".to_string()));
        assert!(lines.contains(&"second".to_string()));
    }

    #[test]
    fn asking_for_more_than_is_kept_gives_what_there_is() {
        let mut writer = Writer;
        writer.write_all(b"only\n").expect("the write");
        assert!(lines(usize::MAX).len() <= CAPACITY);
    }
}
