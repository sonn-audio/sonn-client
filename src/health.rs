//! A JSON snapshot on disk, for looking at a device that is not where you are.
//!
//! The server already gets everything in this file through the status poll. This exists for the case
//! where the poll is the thing that is broken -- no route to the server, wrong VLAN -- and all you
//! have is an SSH session.

use crate::status::Registry;
use serde::Serialize;
use std::fs;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;

const DEFAULT_HEALTH_PATH: &str = "/tmp/sonn-client.status.json";
const WRITE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize)]
struct HealthSnapshot {
    ts: String,
    version: String,
    state: String,
    uptime_s: u64,
    players: Vec<crate::models::PlayerStatusReport>,
}

pub fn spawn(statuses: Registry) {
    let path = std::env::var("SONN_CLIENT_HEALTH_PATH")
        .unwrap_or_else(|_| DEFAULT_HEALTH_PATH.to_string());
    tokio::spawn(async move {
        let mut last_write_ok = true;
        loop {
            let snapshot = HealthSnapshot {
                ts: time::OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .unwrap_or_default(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                state: statuses.device_state(),
                uptime_s: statuses.uptime().as_secs(),
                players: statuses.reports(),
            };
            match serde_json::to_string_pretty(&snapshot) {
                Ok(payload) => {
                    if let Err(err) = fs::write(&path, payload) {
                        // Logged once per failure streak: a read-only /tmp should not fill the log.
                        if last_write_ok {
                            tracing::warn!("health snapshot write failed: {}", err);
                            last_write_ok = false;
                        }
                    } else {
                        last_write_ok = true;
                    }
                }
                Err(err) => {
                    if last_write_ok {
                        tracing::warn!("health snapshot serialize failed: {}", err);
                        last_write_ok = false;
                    }
                }
            }
            tokio::time::sleep(WRITE_INTERVAL).await;
        }
    });
}
