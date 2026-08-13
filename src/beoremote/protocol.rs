//! What the BeoRemote One's attributes hold.
//!
//! Reverse-engineered from Bang & Olufsen's own GPL-released BlueZ plugin and measured against a
//! real remote. The host is the GATT server here -- the opposite of what a BLE remote normally is --
//! and [`super::gatt`] serves the attributes below as characteristics.
//!
//! Two traps are worth naming, because both fail silently:
//!
//! * The attribute enum is **not** the characteristic UUID word. The UUIDs skip 0x09-0x0C, so
//!   MUSIC_SOURCES is UUID 0x19 but enum 21; `gatt::attribute_uuid_number` is what bridges the two.
//! * Source lists are newline-separated with a comma between name and submenu flag. B&O's own debug
//!   logging replaces newlines with commas before printing, so the log shows a flat comma list --
//!   and sending it that way makes the remote render every 0 and 1 as a menu entry.

/// Attribute enum from `plugins/beoremote_one_types.h`, sequential from 1.
const ATTRIBUTE_ORDER: [&str; 44] = [
    "VERSION",
    "FEATURES",
    "FEATURES_CHANGED",
    "INJECT_PRESS",
    "INJECT_RELEASE",
    "DISC_TRACK",
    "STAND_POSITIONS",
    "ACTIVE_STAND_POSITION",
    "SPEAKER_GROUPS",
    "ACTIVE_SPEAKER_GROUP",
    "SOUND_MODES",
    "ACTIVE_SOUND_MODE",
    "PICTURE_FORMATS",
    "ACTIVE_PICTURE_FORMAT",
    "PICTURE_MODES",
    "ACTIVE_PICTURE_MODE",
    "PICTURE_MUTE",
    "2D_3D_MODES",
    "ACTIVE_2D_3D_MODE",
    "TV_SOURCES",
    "MUSIC_SOURCES",
    "ACTIVE_SOURCE",
    "CUSTOM_COMMANDS",
    "ACTIVE_CUSTOM_COMMAND",
    "HOME_CONTROL_SCENES",
    "ACTIVE_HOME_CONTROL_SCENE",
    "CINEMA_MODE",
    "EXPERIENCES",
    "ACTIVE_EXPERIENCE",
    "CONTROL_1",
    "CONTROL_2",
    "SOURCE_CONTENT_1",
    "SOURCE_CONTENT_2",
    "SOURCE_CONTENT_3",
    "SOURCE_CONTENT_4",
    "SOURCE_CONTENT_5",
    "SOURCE_CONTENT_6",
    "SOURCE_CONTENT_7",
    "SOURCE_CONTENT_8",
    "SOURCE_CONTENT_9",
    "SOURCE_CONTENT_10",
    "ACTIVE_SOURCE_CONTENT",
    "MY_BUTTONS",
    "VOLUME",
];

/// Feature bitmap as a real BeoSound Shape reports it.
pub const FEATURES: [u8; 16] = [0x10, 0xC0, 0x80, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
/// Written to make the remote re-read the lists. Same bitmap with the second byte cleared.
pub const FEATURES_CHANGED: [u8; 16] = [0x10, 0x00, 0x80, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// `ACTIVE_SOURCE` reports 20 + the position in the published list.
pub const ACTIVE_SOURCE_BASE: u8 = 20;

/// Attribute number for a name, or None if this build does not know it.
pub fn attribute(name: &str) -> Option<u8> {
    ATTRIBUTE_ORDER
        .iter()
        .position(|entry| *entry == name)
        .map(|index| (index + 1) as u8)
}

/// `MUSIC_SOURCES` / `TV_SOURCES`: one entry per line, `name,flag`.
///
/// Only one source may carry the submenu flag. The remote only ever reads `SOURCE_CONTENT_1` and
/// never says which submenu it opened, so a second flagged source shows the first one's contents --
/// measured repeatedly, including with every FEATURES bit set.
pub fn encode_sources(entries: &[(String, bool)]) -> Vec<u8> {
    entries
        .iter()
        .map(|(name, submenu)| format!("{},{}", sanitize(name), u8::from(*submenu)))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// `SOURCE_CONTENT_n`: plain newline-separated names, no flags.
pub fn encode_content(names: &[String]) -> Vec<u8> {
    names
        .iter()
        .map(|name| sanitize(name))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// Strip the two characters that are structure in this encoding. A playlist called "Rock, Paper"
/// would otherwise split into two menu entries -- one of them called " Paper,0".
fn sanitize(name: &str) -> String {
    name.replace(['\n', ','], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_sources_is_the_attribute_after_tv_sources() {
        // The one that bites: UUID 0x19 is MUSIC_SOURCES, but the enum is 21.
        assert_eq!(attribute("MUSIC_SOURCES"), Some(21));
        assert_eq!(attribute("TV_SOURCES"), Some(20));
        assert_eq!(attribute("ACTIVE_SOURCE"), Some(22));
        assert_eq!(attribute("SOURCE_CONTENT_1"), Some(32));
        assert_eq!(attribute("VOLUME"), Some(44));
        assert_eq!(attribute("NOT_AN_ATTRIBUTE"), None);
    }

    #[test]
    fn entries_are_newline_separated_and_flags_comma_separated() {
        let encoded = encode_sources(&[
            ("B&O Radio".to_string(), true),
            ("Jazz Mix".to_string(), false),
        ]);
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            "B&O Radio,1\nJazz Mix,0"
        );
    }

    #[test]
    fn a_comma_in_a_name_does_not_become_a_menu_entry() {
        let encoded = encode_sources(&[("Rock, Paper".to_string(), false)]);
        assert_eq!(String::from_utf8(encoded).unwrap(), "Rock  Paper,0");
    }
}
