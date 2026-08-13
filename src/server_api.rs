//! The two calls this device makes: register once, then post status forever.
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
    client: Client,
}

impl ServerApi {
    pub fn new(base_url: &str, register_path: &str, status_path: &str) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            register_path: register_path.to_string(),
            status_path: status_path.to_string(),
            // Bounded so a server that accepts the connection and then stalls cannot wedge the
            // status loop: a missed poll is retried, a hung one never is.
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("build http client")?,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
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
