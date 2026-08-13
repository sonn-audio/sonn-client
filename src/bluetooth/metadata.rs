//! What the phone is playing, and the keys back to it.
//!
//! AVRCP, which BlueZ presents as `org.bluez.MediaPlayer1` on the connected device: a `Track`
//! dictionary with the tags the phone chose to send, a `Status` that says playing or paused, and
//! methods for the transport keys. Nothing here is Bluetooth-specific to the rest of the system --
//! the metadata goes up as the source's now-playing and the keys arrive as ordinary transport
//! commands, so a phone behaves like any other source in the room.
//!
//! What a phone sends is entirely up to it. Title and artist are near-universal, album usually,
//! duration often, artwork almost never over AVRCP -- so the player shows what there is and does not
//! pretend the rest is missing data.

use anyhow::{Context, Result};
use tracing::debug;
use zbus::zvariant::OwnedObjectPath;
use zbus::{proxy, Connection};

use super::{interface, managed_objects, string_property, Properties};

#[proxy(interface = "org.bluez.MediaPlayer1", default_service = "org.bluez")]
trait MediaPlayer {
    fn play(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn track(&self) -> zbus::Result<Properties>;
}

/// What the phone says is playing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Track length in milliseconds, when the phone sends one.
    pub duration_ms: Option<u32>,
    /// `playing`, `paused` or `stopped`, as AVRCP spells it.
    pub status: Option<String>,
}

impl NowPlaying {
    /// Whether there is anything worth showing. A phone that sends an empty track is common between
    /// songs, and an empty row on the player looks like a fault rather than a gap.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none()
    }
}

/// The transport keys a room can send back to the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerControl {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

impl PlayerControl {
    /// The names the server already uses for a line-in's transport commands.
    pub fn parse(command: &str) -> Option<Self> {
        match command.trim().to_ascii_lowercase().as_str() {
            "play" | "resume" => Some(Self::Play),
            "pause" => Some(Self::Pause),
            "stop" => Some(Self::Stop),
            "next" | "skip" => Some(Self::Next),
            "previous" | "prev" | "back" => Some(Self::Previous),
            _ => None,
        }
    }
}

/// Read what the connected phone is playing.
pub async fn now_playing(connection: &Connection) -> Option<NowPlaying> {
    let path = player_path(connection).await?;
    let player = MediaPlayerProxy::builder(connection)
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;

    let mut playing = NowPlaying {
        status: player.status().await.ok(),
        ..Default::default()
    };
    if let Ok(track) = player.track().await {
        playing.title = string_property(&track, "Title");
        playing.artist = string_property(&track, "Artist");
        playing.album = string_property(&track, "Album");
        playing.duration_ms = track
            .get("Duration")
            .and_then(|value| u32::try_from(value.clone()).ok());
    }
    if playing.is_empty() && playing.status.is_none() {
        return None;
    }
    Some(playing)
}

/// Press a key on the phone.
pub async fn control(connection: &Connection, command: PlayerControl) -> Result<()> {
    let path = player_path(connection)
        .await
        .context("no phone is connected to control")?;
    let player = MediaPlayerProxy::builder(connection)
        .path(path)?
        .build()
        .await
        .context("talk to the phone's player")?;
    match command {
        PlayerControl::Play => player.play().await,
        PlayerControl::Pause => player.pause().await,
        PlayerControl::Stop => player.stop().await,
        PlayerControl::Next => player.next().await,
        PlayerControl::Previous => player.previous().await,
    }
    .with_context(|| format!("send {command:?} to the phone"))?;
    debug!("bluetooth: sent {command:?} to the phone");
    Ok(())
}

/// The connected phone's player object, if there is one.
async fn player_path(connection: &Connection) -> Option<OwnedObjectPath> {
    let objects = managed_objects(connection).await.ok()?;
    objects
        .into_iter()
        .find(|(_, interfaces)| interface(interfaces, "org.bluez.MediaPlayer1").is_some())
        .map(|(path, _)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_are_the_names_the_rest_of_the_system_uses() {
        assert_eq!(PlayerControl::parse("play"), Some(PlayerControl::Play));
        assert_eq!(PlayerControl::parse("PAUSE"), Some(PlayerControl::Pause));
        assert_eq!(PlayerControl::parse(" next "), Some(PlayerControl::Next));
        assert_eq!(PlayerControl::parse("prev"), Some(PlayerControl::Previous));
        // A line-in's transport uses "resume" for play; a phone should answer to the same word.
        assert_eq!(PlayerControl::parse("resume"), Some(PlayerControl::Play));
        assert_eq!(PlayerControl::parse("eject"), None);
    }

    #[test]
    fn a_track_with_nothing_in_it_is_not_worth_showing() {
        let empty = NowPlaying {
            status: Some("playing".to_string()),
            ..Default::default()
        };
        assert!(empty.is_empty());

        let real = NowPlaying {
            title: Some("Teardrop".to_string()),
            artist: Some("Massive Attack".to_string()),
            ..Default::default()
        };
        assert!(!real.is_empty());
    }
}
