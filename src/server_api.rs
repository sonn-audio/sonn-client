//! The calls this device makes: register once, post status forever, and hand over its log when
//! someone asks for it.
//!
//! Both return the same thing -- the server's desired state for this device -- so a config change
//! made in the UI lands on the next poll without the server having to reach back in.

use crate::models::{ClientRegisterRequest, ClientStatusRequest, DesiredConfig};
use anyhow::{Context, Result};

/// What a status post came back with.
pub enum StatusOutcome {
    /// The state this device should be in.
    Desired(Box<DesiredConfig>),
    /// The server does not know this device and wants it to register again.
    Unknown,
}
use reqwest::Client;
use std::time::Duration;

#[derive(Clone)]
pub struct ServerApi {
    base_url: String,
    register_path: String,
    status_path: String,
    logs_path: String,
    client: Client,
}

impl ServerApi {
    pub fn new(base_url: &str, register_path: &str, status_path: &str) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            register_path: register_path.to_string(),
            status_path: status_path.to_string(),
            logs_path: logs_path_for(status_path),
            client: Client::builder()
                // Bounded so a server that accepts the connection and then stalls cannot wedge the
                // status loop: a missed poll is retried, a hung one never is.
                .timeout(Duration::from_secs(10))
                // Shorter than any sensible server's keep-alive, so we never hand a request to a
                // connection the other side is closing at that moment. Node's default is five
                // seconds and this client posts its status every five: the two lined up exactly,
                // and every few minutes a speaker logged a connection reset for a request that
                // succeeded on the retry.
                .pool_idle_timeout(Duration::from_secs(3))
                .build()
                .context("build http client")?,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Hand over what this device has been saying.
    ///
    /// Only when asked: the lines are worth kilobytes and are read perhaps once a week, so putting
    /// them in every status post would be paying for them constantly. Failure is the caller's to
    /// log and forget -- a log that did not arrive is not worth a retry that competes with the
    /// status loop.
    pub async fn post_logs(&self, device_id: &str, lines: &[String]) -> Result<()> {
        let url = format!(
            "{}{}",
            self.base_url,
            self.logs_path.replace("{device_id}", device_id)
        );
        self.client
            .post(url)
            .json(&serde_json::json!({ "lines": lines }))
            .send()
            .await
            .context("post logs")?
            .error_for_status()
            .context("logs response status")?;
        Ok(())
    }

    pub async fn register(&self, request: &ClientRegisterRequest) -> Result<DesiredConfig> {
        let url = format!("{}{}", self.base_url, self.register_path);
        let response = self
            .client
            .post(url)
            .json(request)
            .send()
            .await
            .context("register client")?
            .error_for_status()
            .context("register response status")?;
        response
            .json::<DesiredConfig>()
            .await
            .context("parse register response")
    }

    /// Report what this device is doing, and read back what it should be.
    ///
    /// [`StatusOutcome::Unknown`] is not a failure: the server keeps what hardware a device has in
    /// memory, so a server that has restarted is one this device has to introduce itself to again.
    /// It says so rather than answering with a desired state built on nothing.
    pub async fn post_status(
        &self,
        device_id: &str,
        status: &ClientStatusRequest,
    ) -> Result<StatusOutcome> {
        let url = format!(
            "{}{}",
            self.base_url,
            self.status_path.replace("{device_id}", device_id)
        );
        let response = self
            .client
            .post(url)
            .json(status)
            .send()
            .await
            .context("post status")?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            return Ok(StatusOutcome::Unknown);
        }
        let response = response
            .error_for_status()
            .context("status response status")?;
        response
            .json::<DesiredConfig>()
            .await
            .map(|desired| StatusOutcome::Desired(Box::new(desired)))
            .context("parse status response")
    }
}

/// Where the logs go, given where the status posts go.
///
/// Derived rather than discovered: the route sits beside status on the same server, and a device
/// that has found a server old enough not to have it gets a 404 it can log. A status path in an
/// unexpected shape falls back to the standard route rather than inventing one.
fn logs_path_for(status_path: &str) -> String {
    match status_path.strip_suffix("/status") {
        Some(base) => format!("{base}/logs"),
        None => "/api/sonnclients/{device_id}/logs".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::logs_path_for;

    #[test]
    fn logs_sit_beside_status() {
        assert_eq!(
            logs_path_for("/api/sonnclients/{device_id}/status"),
            "/api/sonnclients/{device_id}/logs"
        );
        assert_eq!(
            logs_path_for("/custom/prefix/sonnclients/{device_id}/status"),
            "/custom/prefix/sonnclients/{device_id}/logs"
        );
    }

    #[test]
    fn an_unexpected_status_path_falls_back_to_the_standard_route() {
        assert_eq!(
            logs_path_for("/api/sonnclients/{device_id}/report"),
            "/api/sonnclients/{device_id}/logs"
        );
    }
}
