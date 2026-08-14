//! Local config. Deliberately almost empty.
//!
//! Everything about *what this device plays* lives on the server: the sound card, the room name,
//! the delay, the volume mode. What is left here is identity (who am I) and, when a site has more
//! than one audioserver, which one to attach to. A fresh install writes this file itself, so the
//! device needs nothing but the binary.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

const CONFIG_DIR_SYSTEM: &str = "/etc/sonn-client";
const CONFIG_DIR_FALLBACK: &str = ".config/sonn-client";
const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// Stable device identity. Generated on first run; changing it makes the server treat this as a
    /// new device, so every zone pointing at the old id stops playing.
    pub device_id: String,
    /// Match the advertised mDNS instance name, e.g. `Test Audioserver`. Optional; only needed with
    /// several audioservers on one network.
    #[serde(default)]
    pub preferred_server_name: Option<String>,
    /// Match the `mac` TXT record of the server to attach to, any separator or case.
    #[serde(default)]
    pub preferred_server_mac: Option<String>,
    /// Skip discovery and use this base URL, e.g. `http://192.168.1.209:7090`. For networks where
    /// mDNS does not cross a VLAN.
    #[serde(default)]
    pub server_url: Option<String>,
    /// Run when this device connects to / disconnects from a server, as `<command>` with
    /// `SONN_EVENT` (and `SONN_SERVER_*`, `SONN_CLIENT_*`) in the environment. Same shape as the
    /// reference client's `--hook-start` / `--hook-stop`.
    #[serde(default)]
    pub on_connect: Option<String>,
    /// Run for every command the server queues for this device, as `<script> <command> [args...]`.
    /// Reserved for hardware that has to be switched on or handed a key press.
    #[serde(default)]
    pub on_command: Option<String>,
    /// Fallback hardware-volume hook for players the server has not given one, called as
    /// `<command> <level 0-100>`. Muted is sent as 0.
    #[serde(default)]
    pub volume_hook: Option<String>,
    /// The server that last claimed this device, remembered across restarts.
    ///
    /// Written by the client, not by hand: it is state, like `device_id`, not a preference. Without
    /// it a device that reboots while its own server is down is a device with no memory of whose it
    /// is, and the first server to answer takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_server: Option<String>,
    /// Anything in the file this build does not know.
    ///
    /// Kept rather than rejected, because a config written by a newer client should survive a
    /// downgrade untouched. Kept rather than *silently* dropped, because the other thing that lands
    /// here is a misspelled setting, and a setting that does nothing while looking right is the
    /// hardest kind of failure to see from the far end of a network.
    #[serde(flatten)]
    pub unrecognised: BTreeMap<String, toml::Value>,
}

impl Config {
    /// Say what in this file will be ignored. Empty when everything was understood.
    pub fn unrecognised_keys(&self) -> Vec<&str> {
        self.unrecognised.keys().map(String::as_str).collect()
    }
}

pub fn preferred_config_path() -> PathBuf {
    PathBuf::from(CONFIG_DIR_SYSTEM).join(CONFIG_FILE)
}

pub fn fallback_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(CONFIG_DIR_FALLBACK)
        .join(CONFIG_FILE))
}

pub fn write_config(config: &Config) -> Result<PathBuf> {
    let contents = toml::to_string_pretty(config).context("serialize config")?;
    let preferred = preferred_config_path();
    if try_write(&preferred, &contents).is_ok() {
        return Ok(preferred);
    }

    let fallback = fallback_config_path()?;
    try_write(&fallback, &contents).context("write fallback config")?;
    Ok(fallback)
}

pub fn load_or_create_config() -> Result<(Config, PathBuf)> {
    // Salvaged from a file we could not parse, so an unrelated typo does not re-identify the device.
    let mut rescued_device_id: Option<String> = None;

    let preferred = preferred_config_path();
    if preferred.exists() {
        match load_config_file(&preferred) {
            Ok(config) => return Ok((config, preferred)),
            Err(err) => {
                rescued_device_id = rescue_device_id(&preferred);
                backup_invalid_config(&preferred, &err)?;
            }
        }
    }

    let fallback = fallback_config_path()?;
    if fallback.exists() {
        match load_config_file(&fallback) {
            Ok(config) => return Ok((config, fallback)),
            Err(err) => {
                rescued_device_id = rescued_device_id.or_else(|| rescue_device_id(&fallback));
                backup_invalid_config(&fallback, &err)?;
            }
        }
    }

    let config = Config {
        device_id: rescued_device_id.unwrap_or_else(default_device_id),
        preferred_server_name: None,
        preferred_server_mac: None,
        server_url: None,
        on_connect: None,
        on_command: None,
        volume_hook: None,
        attached_server: None,
        unrecognised: BTreeMap::new(),
    };
    let path = write_config(&config)?;
    Ok((config, path))
}

/// `sonn-<hostname>-<random>`: readable in a client list, still unique when three Pis are all
/// called `raspberrypi`.
fn default_device_id() -> String {
    let host = hostname::get()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let slug: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    if slug.is_empty() {
        format!("sonn-{}", suffix)
    } else {
        format!("sonn-{}-{}", slug, suffix)
    }
}

fn load_config_file(path: &Path) -> Result<Config> {
    let data = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&data).with_context(|| format!("parse {}", path.display()))
}

/// Recover the device id from a file that will not parse.
///
/// Identity is the one line that must survive a mistake anywhere else in the file: the server knows
/// this device by it, and every zone pointing here goes quiet the moment it changes. A hand-written
/// `server_url` without quotes should not silently turn a configured speaker into a stranger.
///
/// Deliberately not a TOML parse -- the file is already known not to be one. It looks for the
/// simplest possible spelling of the line and gives up otherwise.
fn rescue_device_id(path: &Path) -> Option<String> {
    let data = fs::read_to_string(path).ok()?;
    for line in data.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("device_id") else {
            continue;
        };
        let value = value.trim_start().strip_prefix('=')?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())?;
        if !value.trim().is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// A config we cannot parse is moved aside rather than overwritten: whatever the operator typed is
/// still there to look at, and the device comes up on a fresh one instead of refusing to start.
///
/// Loudly, though. Silently replacing the file is how a mistyped setting becomes "my speaker
/// disappeared and I have no idea why".
fn backup_invalid_config(path: &Path, err: &anyhow::Error) -> Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_extension(format!("invalid.{}", timestamp));
    fs::rename(path, &backup)
        .or_else(|_| {
            fs::copy(path, &backup)
                .map(|_| ())
                .and_then(|_| fs::remove_file(path))
        })
        .with_context(|| format!("backup invalid config {}: {}", path.display(), err))?;
    warn!(
        "{} could not be read as TOML ({:#}); it has been moved to {} and replaced with a fresh one. \
         Any setting in it is no longer in effect.",
        path.display(),
        err,
        backup.display()
    );
    Ok(())
}

fn try_write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

/// Remember which server runs this device, so a restart does not forget.
///
/// Best effort on purpose: a device that cannot write its config still works, it only loses the
/// memory of whose it is when it restarts -- which is what every device did before this.
pub fn remember_server(config: &mut Config, base_url: Option<&str>) {
    let next = base_url.map(str::to_string);
    if config.attached_server == next {
        return;
    }
    config.attached_server = next;
    match write_config(config) {
        Ok(path) => tracing::debug!("noted the attached server in {}", path.display()),
        Err(err) => tracing::debug!("could not note the attached server: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_a_file_that_will_not_parse() {
        let dir = std::env::temp_dir().join(format!(
            "sonn-cfg-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");

        // What an operator actually types: a value without quotes. TOML rejects the whole file, and
        // before this the device came back under a new id -- every zone pointing at the old one gone
        // quiet, with nothing in the log to connect the two.
        fs::write(
            &path,
            "device_id = \"sonn-beosound9000-a01ec20a\"\nserver_url = http://192.168.1.209:7090\n",
        )
        .expect("write");
        assert!(toml::from_str::<Config>(&fs::read_to_string(&path).unwrap()).is_err());
        assert_eq!(
            rescue_device_id(&path).as_deref(),
            Some("sonn-beosound9000-a01ec20a")
        );

        fs::write(&path, "nothing to see here\n").expect("write");
        assert_eq!(rescue_device_id(&path), None, "invented ids are worse");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_setting_this_build_does_not_know_is_kept_and_named() {
        let config: Config = toml::from_str(
            r#"
device_id = "sonn-test"
preferred_server_name = "Test Audioserver"
prefered_server_name = "typo, one r short"
"#,
        )
        .expect("an unknown key is not a parse error");

        assert_eq!(
            config.preferred_server_name.as_deref(),
            Some("Test Audioserver")
        );
        // The failure this catches: a setting that looks right in the file, does nothing, and says
        // nothing about it.
        assert_eq!(config.unrecognised_keys(), vec!["prefered_server_name"]);

        // And it survives a rewrite, so a config written by a newer client is not quietly stripped.
        let written = toml::to_string_pretty(&config).expect("serialize");
        assert!(written.contains("prefered_server_name"), "{written}");
    }
}
