//! Sonn Client -- a Sendspin-only audio endpoint.
//!
//! One protocol on the device and nothing else. AirPlay, DLNA, Cast, Spotify and Bluetooth all still
//! reach this speaker, but they are terminated on the server, which turns them into a Sendspin stream
//! aimed here. That is what makes a room a room: one clock, one buffer model, one place where sync is
//! solved. The price is that the device has to be told *what* to be, which is what the small
//! management API in `docs/PROTOCOL.md` is for -- the device reports its sound cards, the server picks
//! one, and no one has to SSH into a Pi to change a setting.

mod config;
mod devices;
mod discovery;
mod health;
mod hooks;
mod identity;
mod install;
mod models;
mod player;
mod server_api;
mod status;
mod supervisor;

use anyhow::Result;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::discovery::DiscoveredServer;
use crate::models::{
    ClientCapabilities, ClientRegisterRequest, ClientStatusRequest, DesiredConfig, OutputDeviceInfo,
};
use crate::server_api::ServerApi;

const DEFAULT_POLL_MS: u64 = 5_000;
const MIN_POLL_MS: u64 = 1_000;
const MAX_POLL_MS: u64 = 60_000;
/// How many failed status posts before we assume the server moved and start over from discovery.
const MAX_STATUS_FAILURES: u32 = 3;
const REGISTER_ATTEMPTS: u32 = 3;
/// Players one device can run at once, one per sound card. A build limit, not a licence.
const MAX_PLAYERS: u8 = 4;

#[tokio::main]
async fn main() -> Result<()> {
    let (command, log_level) = parse_args()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            log_level.unwrap_or_else(|| "off".to_string()),
        ))
        .init();

    match command.as_deref() {
        Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("sonn-client {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("devices") => devices::print_output_devices(),
        Some("install") => install::run_install().await,
        Some("run") | None => run().await,
        _ => {
            print_usage();
            anyhow::bail!("unknown command");
        }
    }
}

async fn run() -> Result<()> {
    let (config, config_path) = config::load_or_create_config()?;
    info!(
        "sonn-client {} as {} (config {})",
        env!("CARGO_PKG_VERSION"),
        config.device_id,
        config_path.display()
    );
    let identity = identity::collect();
    let hooks = Arc::new(hooks::HookRunner::new(
        config.device_id.clone(),
        config.on_connect.clone(),
        config.on_command.clone(),
    ));
    let statuses = status::Registry::new();
    health::spawn(statuses.clone());
    spawn_shutdown_handler(Arc::clone(&hooks));

    // Outer loop: attach to a server, run until contact is lost, start over. A server that is
    // rebooted, renamed or moved to another address needs no help from anyone here.
    loop {
        let server = resolve_server(&config).await;
        let api = ServerApi::new(
            &server.base_url,
            &server.register_path,
            &server.status_path,
        )?;
        info!("attaching to {}", api.base_url());

        let outputs = devices::list_output_devices().unwrap_or_else(|err| {
            // Reported as no outputs rather than fatal: the device still registers, so the server can
            // show it with an empty card list instead of the user seeing nothing at all.
            warn!("could not enumerate audio outputs: {:#}", err);
            Vec::new()
        });
        let request = build_register_request(&config, &identity, &outputs);
        let Some(desired) = register(&api, &request).await else {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };

        hooks
            .connection_event("connected", api.base_url(), &server.instance_name)
            .await;

        let (desired_tx, desired_rx) = watch::channel(desired);
        let (stop_tx, stop_rx) = watch::channel(false);
        let poller = tokio::spawn(status_loop(
            api.clone(),
            config.device_id.clone(),
            statuses.clone(),
            Arc::clone(&hooks),
            desired_tx,
            stop_tx,
            outputs,
        ));

        // Returns when the poller gives up on this server (or its sender is dropped), having first
        // stopped every player it started.
        supervisor::run(
            desired_rx,
            statuses.clone(),
            config.volume_hook.clone(),
            stop_rx,
        )
        .await;

        poller.abort();
        let _ = poller.await;
        hooks
            .connection_event("disconnected", api.base_url(), &server.instance_name)
            .await;
        warn!("lost contact with {}; rediscovering", api.base_url());
    }
}

/// A server pinned in config.toml, or whatever mDNS turns up. Never gives up: a device that boots
/// before the network is a normal event.
async fn resolve_server(config: &config::Config) -> DiscoveredServer {
    if let Some(url) = config
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return DiscoveredServer::from_base_url(url);
    }

    loop {
        let preferred_name = config.preferred_server_name.clone();
        let preferred_mac = config.preferred_server_mac.clone();
        // Discovery blocks for up to 8 seconds waiting for mDNS answers, so it does not belong on an
        // async worker.
        let discovered = tokio::task::spawn_blocking(move || {
            discovery::discover_server(preferred_name.as_deref(), preferred_mac.as_deref())
        })
        .await;
        match discovered {
            Ok(Ok(server)) => {
                info!("discovered {} at {}", server.instance_name, server.base_url);
                return server;
            }
            Ok(Err(err)) => warn!("mDNS discovery found nothing: {:#}", err),
            Err(err) => warn!("discovery task failed: {}", err),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn register(api: &ServerApi, request: &ClientRegisterRequest) -> Option<DesiredConfig> {
    for attempt in 1..=REGISTER_ATTEMPTS {
        match api.register(request).await {
            Ok(desired) => {
                info!(
                    "registered {} with {} output(s); {} player(s) configured",
                    request.device_id,
                    request.outputs.len(),
                    desired.players.len()
                );
                return Some(desired);
            }
            Err(err) => {
                let detail = format!("{:#}", err);
                if detail.contains("404") {
                    // Worth calling out: everything else about the server looks fine, it just does
                    // not have the client API yet.
                    warn!(
                        "registration rejected with 404 -- this audioserver may not support Sonn clients yet: {}",
                        detail
                    );
                } else {
                    warn!(
                        "registration attempt {}/{} failed: {}",
                        attempt, REGISTER_ATTEMPTS, detail
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    None
}

/// Report what we are doing, receive what we should be doing. Every reply is the full desired state,
/// so a change made in the UI takes effect one poll later without the server reaching back in.
async fn status_loop(
    api: ServerApi,
    device_id: String,
    statuses: status::Registry,
    hooks: Arc<hooks::HookRunner>,
    desired_tx: watch::Sender<DesiredConfig>,
    stop_tx: watch::Sender<bool>,
    initial_outputs: Vec<OutputDeviceInfo>,
) {
    let mut reported_outputs = hash_outputs(&initial_outputs);
    let mut failures = 0u32;
    let mut interval = Duration::from_millis(DEFAULT_POLL_MS);

    loop {
        tokio::time::sleep(interval).await;

        let outputs = devices::list_output_devices().unwrap_or_default();
        let outputs_hash = hash_outputs(&outputs);
        let request = ClientStatusRequest {
            state: statuses.device_state(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: statuses.uptime().as_secs(),
            players: statuses.reports(),
            // Only when the set changed: a USB DAC plugged in after boot has to appear in the
            // picker, and repeating an unchanged list on every poll is noise.
            outputs: (outputs_hash != reported_outputs).then(|| outputs.clone()),
        };

        match api.post_status(&device_id, &request).await {
            Ok(desired) => {
                failures = 0;
                reported_outputs = outputs_hash;
                interval = poll_interval(&desired);
                for command in &desired.commands {
                    hooks.command(&command.command, &command.args).await;
                }
                if desired_tx.send(desired).is_err() {
                    // Nobody left to act on it.
                    return;
                }
            }
            Err(err) => {
                failures += 1;
                warn!(
                    "status post failed ({}/{}): {:#}",
                    failures, MAX_STATUS_FAILURES, err
                );
                if failures >= MAX_STATUS_FAILURES {
                    let _ = stop_tx.send(true);
                    return;
                }
            }
        }
    }
}

fn poll_interval(desired: &DesiredConfig) -> Duration {
    let ms = desired
        .poll_interval_ms
        .unwrap_or(DEFAULT_POLL_MS)
        .clamp(MIN_POLL_MS, MAX_POLL_MS);
    Duration::from_millis(ms)
}

fn build_register_request(
    config: &config::Config,
    identity: &identity::DeviceIdentity,
    outputs: &[OutputDeviceInfo],
) -> ClientRegisterRequest {
    ClientRegisterRequest {
        device_id: config.device_id.clone(),
        agent: "sonn-client".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        hostname: identity.hostname.clone(),
        ip: identity.ip.clone(),
        mac: identity.mac.clone(),
        model: identity.model.clone(),
        os: identity.os.clone(),
        outputs: outputs.to_vec(),
        capabilities: ClientCapabilities {
            codecs: player::SUPPORTED_CODECS
                .iter()
                .map(|codec| codec.to_string())
                .collect(),
            max_players: MAX_PLAYERS,
            features: Vec::new(),
        },
    }
}

fn hash_outputs(outputs: &[OutputDeviceInfo]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    outputs.hash(&mut hasher);
    hasher.finish()
}

/// systemd stopping us is the normal way this process ends. Fire the disconnect hook first, so a
/// device that switched an amplifier on when it joined switches it off again when it leaves.
fn spawn_shutdown_handler(hooks: Arc<hooks::HookRunner>) {
    tokio::spawn(async move {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            Ok(signal) => signal,
            Err(err) => {
                warn!("cannot listen for SIGTERM: {}", err);
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => info!("SIGTERM received"),
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    warn!("cannot listen for SIGINT: {}", err);
                    return;
                }
                info!("SIGINT received");
            }
        }
        // The server fields are empty here: which server we were attached to is not knowable from a
        // signal handler, and the hook's job at this point is to shut hardware down.
        hooks.connection_event("disconnected", "", "").await;
        std::process::exit(0);
    });
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  sonn-client [--log-level <level>] [run]");
    eprintln!("  sonn-client install");
    eprintln!("  sonn-client devices");
    eprintln!("  sonn-client --help");
    eprintln!("  sonn-client --version");
    eprintln!();
    eprintln!("Log levels: off (default), error, warn, info, debug, trace");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  sudo sonn-client install         # write the systemd unit and start the service");
    eprintln!("  sonn-client devices              # list the sound cards the server will be offered");
    eprintln!("  sonn-client --log-level info run # run in the foreground with logs");
}

fn parse_args() -> Result<(Option<String>, Option<String>)> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut log_level = None;

    while let Some(arg) = args.next() {
        if arg == "--log-level" {
            let level = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--log-level requires a value"))?;
            log_level = Some(level);
            continue;
        }
        if let Some(level) = arg.strip_prefix("--log-level=") {
            log_level = Some(level.to_string());
            continue;
        }
        if command.is_none() {
            command = Some(arg);
        }
    }

    Ok((command, log_level))
}
