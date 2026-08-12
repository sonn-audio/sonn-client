//! Sonn Client -- a Sendspin-only audio endpoint.
//!
//! One protocol on the device and nothing else. AirPlay, DLNA, Cast, Spotify and Bluetooth all still
//! reach this speaker, but they are terminated on the server, which turns them into a Sendspin stream
//! aimed here. That is what makes a room a room: one clock, one buffer model, one place where sync is
//! solved. The price is that the device has to be told *what* to be, which is what the small
//! management API in `docs/PROTOCOL.md` is for -- the device reports its sound cards, the server picks
//! one, and no one has to SSH into a Pi to change a setting.

mod alsa_quiet;
mod beoremote;
mod components;
mod config;
mod devices;
mod discovery;
mod health;
mod hooks;
mod identity;
mod install;
mod models;
mod pairing;
mod player;
mod server_api;
mod source;
mod status;
mod supervisor;
mod update;

use anyhow::Result;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::discovery::DiscoveredServer;
use crate::models::{
    ClientCapabilities, ClientRegisterRequest, ClientStatusRequest, DesiredConfig, DeviceCommand,
    OutputDeviceInfo, SourceCommand,
};
use crate::server_api::ServerApi;
use crate::supervisor::SupervisorContext;

const DEFAULT_POLL_MS: u64 = 5_000;
const MIN_POLL_MS: u64 = 1_000;
const MAX_POLL_MS: u64 = 60_000;
/// How many failed status posts before we assume the server moved and start over from discovery.
const MAX_STATUS_FAILURES: u32 = 3;
const REGISTER_ATTEMPTS: u32 = 3;
/// Players one device can run at once, one per sound card. A build limit, not a licence.
const MAX_PLAYERS: u8 = 4;
/// How long a server that has no client API is left alone. Long enough that it stops filling the
/// journal, short enough that upgrading that server does not need a visit to the device.
const DECLINED_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
/// Pause when every audioserver on the network has declined, so the search does not become a poll.
const DISCOVERY_BACKOFF: Duration = Duration::from_secs(60);
/// Named extras this build ships, so the server can offer the matching configuration.
const FEATURES: [&str; 3] = ["source", "beoremote", "components"];

#[tokio::main]
async fn main() -> Result<()> {
    let (command, argument, log_level) = parse_args()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(default_log_filter(
            command.as_deref(),
            log_level,
        )))
        .init();
    alsa_quiet::install();

    match command.as_deref() {
        Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("sonn-client {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("devices") => devices::print_devices(),
        Some("install") => install::run_install().await,
        Some("pair-remote") => run_pair_remote(argument).await,
        Some("components") => {
            let status = components::inspect_bluetoothd();
            println!(
                "{}: {} ({})",
                status.name,
                status.state,
                status.version.as_deref().unwrap_or("no version recorded")
            );
            Ok(())
        }
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
    // Said out loud at startup, because the alternative is reading a log full of the wrong server and
    // having no way to tell whether the config was even picked up.
    info!("{}", describe_server_preference(&config));
    let unrecognised = config.unrecognised_keys();
    if !unrecognised.is_empty() {
        warn!(
            "{} contains {} this build does not know, which will be ignored: {}",
            config_path.display(),
            if unrecognised.len() == 1 {
                "a setting"
            } else {
                "settings"
            },
            unrecognised.join(", ")
        );
    }

    // Servers that answered "no such endpoint", and when. Cleared by a restart, which is the same
    // moment someone would have upgraded the server they were expecting to use.
    let mut declined: HashMap<String, Instant> = HashMap::new();

    // Outer loop: attach to a server, run until contact is lost, start over. A server that is
    // rebooted, renamed or moved to another address needs no help from anyone here.
    loop {
        let server = resolve_server(&config, &declined).await;
        let api = ServerApi::new(&server.base_url, &server.register_path, &server.status_path)?;
        info!("attaching to {}", api.base_url());

        let outputs = devices::list_output_devices().unwrap_or_else(|err| {
            // Reported as no outputs rather than fatal: the device still registers, so the server can
            // show it with an empty card list instead of the user seeing nothing at all.
            warn!("could not enumerate audio outputs: {:#}", err);
            Vec::new()
        });
        let inputs = devices::list_input_devices().unwrap_or_else(|err| {
            warn!("could not enumerate audio inputs: {:#}", err);
            Vec::new()
        });
        // Reported at registration so the server knows up front whether the B&O features can be
        // offered on this device, without asking for an install to find out.
        statuses.set_components(vec![components::inspect_bluetoothd()]);
        let request = build_register_request(&config, &identity, &outputs, &inputs, &statuses);
        let desired = match register(&api, &request).await {
            Registration::Accepted(desired) => *desired,
            Registration::NotSupported => {
                declined.insert(api.base_url().to_string(), Instant::now());
                // A pinned server is not skipped and discovery is not run, so nothing else would
                // slow this loop down. Wait before asking the same server the same question.
                if is_pinned(&config) {
                    tokio::time::sleep(DISCOVERY_BACKOFF).await;
                }
                continue;
            }
            Registration::Unreachable => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
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
            inputs,
        ));

        // Returns when the poller gives up on this server (or its sender is dropped), having first
        // stopped every player, source and bridge it started.
        supervisor::run(
            desired_rx,
            SupervisorContext {
                statuses: statuses.clone(),
                fallback_volume_hook: config.volume_hook.clone(),
                server_base_url: api.base_url().to_string(),
            },
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
///
/// `declined` holds the servers that answered "no such endpoint". They are skipped while that answer
/// is still fresh, so a network with two audioservers -- one upgraded, one not -- settles on the one
/// that can actually use this device instead of retrying the other every few seconds forever.
async fn resolve_server(
    config: &config::Config,
    declined: &HashMap<String, Instant>,
) -> DiscoveredServer {
    if let Some(url) = config
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        // A pinned server is never skipped: there is nothing else to fall back to, and the operator
        // gets to see it keep trying rather than have the device quietly give up on their choice.
        return DiscoveredServer::from_base_url(url);
    }

    loop {
        let preferred_name = config.preferred_server_name.clone();
        let preferred_mac = config.preferred_server_mac.clone();
        // Discovery blocks for up to 8 seconds waiting for mDNS answers, so it does not belong on an
        // async worker.
        let discovered = tokio::task::spawn_blocking(move || {
            discovery::discover_servers(preferred_name.as_deref(), preferred_mac.as_deref())
        })
        .await;
        match discovered {
            Ok(Ok(servers)) => {
                let fresh: Vec<DiscoveredServer> = servers
                    .iter()
                    .filter(|server| match declined.get(&server.base_url) {
                        Some(when) => when.elapsed() >= DECLINED_RETRY_AFTER,
                        None => true,
                    })
                    .cloned()
                    .collect();
                if let Some(server) = fresh.into_iter().next() {
                    info!("discovered {} at {}", server.instance_name, server.base_url);
                    return server;
                }
                if !servers.is_empty() {
                    warn!(
                        "the {} audioserver(s) found do not support Sonn clients; waiting {} minutes before asking again",
                        servers.len(),
                        DECLINED_RETRY_AFTER.as_secs() / 60
                    );
                    tokio::time::sleep(DISCOVERY_BACKOFF).await;
                    continue;
                }
            }
            Ok(Err(err)) => warn!("mDNS discovery found nothing: {:#}", err),
            Err(err) => warn!("discovery task failed: {}", err),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// What a server had to say when this device introduced itself.
enum Registration {
    Accepted(Box<DesiredConfig>),
    /// The server answered, but has no client API. Retrying changes nothing.
    NotSupported,
    /// Nobody answered, or answered badly. Worth trying again.
    Unreachable,
}

async fn register(api: &ServerApi, request: &ClientRegisterRequest) -> Registration {
    for attempt in 1..=REGISTER_ATTEMPTS {
        match api.register(request).await {
            Ok(desired) => {
                info!(
                    "registered {} with {} output(s); {} player(s) configured",
                    request.device_id,
                    request.outputs.len(),
                    desired.players.len()
                );
                // The first honest moment to call an update successful: this build started *and*
                // reached a server. Until now the previous binary was still standing by.
                update::confirm_started();
                return Registration::Accepted(Box::new(desired));
            }
            Err(err) => {
                let detail = format!("{:#}", err);
                if detail.contains("404") {
                    // A settled answer, not a hiccup: this server is running a build without the
                    // client API. Asking it four more times only fills the journal.
                    warn!(
                        "{} does not support Sonn clients (404); looking for another audioserver",
                        api.base_url()
                    );
                    return Registration::NotSupported;
                }
                warn!(
                    "registration attempt {}/{} failed: {}",
                    attempt, REGISTER_ATTEMPTS, detail
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Registration::Unreachable
}

/// Report what we are doing, receive what we should be doing. Every reply is the full desired state,
/// so a change made in the UI takes effect one poll later without the server reaching back in.
#[allow(clippy::too_many_arguments)]
async fn status_loop(
    api: ServerApi,
    device_id: String,
    statuses: status::Registry,
    hooks: Arc<hooks::HookRunner>,
    desired_tx: watch::Sender<DesiredConfig>,
    stop_tx: watch::Sender<bool>,
    initial_outputs: Vec<OutputDeviceInfo>,
    initial_inputs: Vec<OutputDeviceInfo>,
) {
    let mut reported_outputs = hash_outputs(&initial_outputs);
    let mut reported_inputs = hash_outputs(&initial_inputs);
    let mut failures = 0u32;
    let mut interval = Duration::from_millis(DEFAULT_POLL_MS);

    loop {
        tokio::time::sleep(interval).await;

        let outputs = devices::list_output_devices().unwrap_or_default();
        let outputs_hash = hash_outputs(&outputs);
        let inputs = devices::list_input_devices().unwrap_or_default();
        let inputs_hash = hash_outputs(&inputs);
        let request = ClientStatusRequest {
            state: statuses.device_state(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: statuses.uptime().as_secs(),
            players: statuses.reports(),
            sources: statuses.source_reports(),
            // Only when the set changed: a USB DAC plugged in after boot has to appear in the
            // picker, and repeating an unchanged list on every poll is noise.
            outputs: (outputs_hash != reported_outputs).then(|| outputs.clone()),
            inputs: (inputs_hash != reported_inputs).then(|| inputs.clone()),
            components: statuses.components(),
            pairing: statuses.pairing(),
            beoremote: statuses.beoremote(),
        };

        match api.post_status(&device_id, &request).await {
            Ok(desired) => {
                failures = 0;
                reported_outputs = outputs_hash;
                reported_inputs = inputs_hash;
                interval = poll_interval(&desired);
                for command in &desired.commands {
                    // Built-ins first: a command this client can carry out itself should not need a
                    // script on the device to be useful.
                    if handle_builtin_command(command, &statuses).await {
                        continue;
                    }
                    hooks.command(&command.command, &command.args).await;
                }
                // Transport for the gear on an input goes to that input's own hook, so the script
                // that speaks to a BeoSound is configured with the input rather than with the box.
                for command in &desired.source_commands {
                    run_source_command(&desired, command).await;
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
    inputs: &[OutputDeviceInfo],
    statuses: &status::Registry,
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
        arch: identity.arch.clone(),
        outputs: outputs.to_vec(),
        inputs: inputs.to_vec(),
        capabilities: ClientCapabilities {
            codecs: player::supported_codecs(),
            max_players: MAX_PLAYERS,
            features: FEATURES.iter().map(|entry| entry.to_string()).collect(),
        },
        components: statuses.components(),
    }
}

/// Commands this client carries out itself, rather than handing to a script.
///
/// Returns true when it was one of ours. Pairing is the case that matters: it is the one thing a user
/// would otherwise need a terminal for, and the server can offer it as a button instead.
async fn handle_builtin_command(command: &DeviceCommand, statuses: &status::Registry) -> bool {
    match command.command.as_str() {
        "pair_remote" => {
            let address = command.args.first().cloned();
            let statuses = statuses.clone();
            // Spawned: the pairing window is up to 90 seconds and the status loop has to keep
            // reporting while it is open -- that report is how the UI shows progress.
            tokio::spawn(async move {
                if let Err(err) = pairing::pair_remote(&statuses, address, None).await {
                    warn!("pairing failed to start: {:#}", err);
                }
            });
            true
        }
        _ => false,
    }
}

/// Hand one transport command to the hook of the source it names.
///
/// Silence here is a command that reaches nothing, so a source without a hook says so once rather
/// than leaving someone pressing a button that is quietly discarded.
async fn run_source_command(desired: &DesiredConfig, command: &SourceCommand) {
    let Some(source) = desired
        .sources
        .iter()
        .find(|source| source.client_id == command.client_id)
    else {
        warn!(
            "command {} is for input {}, which this device does not have",
            command.command, command.client_id
        );
        return;
    };
    let Some(hook) = source
        .control_hook
        .as_deref()
        .map(str::trim)
        .filter(|hook| !hook.is_empty())
    else {
        warn!(
            "input {} has no control hook, so {} goes nowhere",
            command.client_id, command.command
        );
        return;
    };
    info!(
        "input {}: {} {}",
        command.client_id,
        command.command,
        command.args.join(" ")
    );
    hooks::run_control_hook(hook, &command.command, &command.args).await;
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
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
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

/// `sonn-client pair-remote [address]`, for pairing a Beoremote One by hand. The same flow the server
/// triggers with a `pair_remote` command, so the button in the UI and the command line cannot drift.
async fn run_pair_remote(address: Option<String>) -> Result<()> {
    let statuses = status::Registry::new();
    println!("Put the remote into pairing mode now.");
    pairing::pair_remote(&statuses, address, None).await?;
    match statuses.pairing() {
        Some(report) => {
            println!(
                "{}{}{}",
                report.state,
                report
                    .address
                    .map(|address| format!(" {}", address))
                    .unwrap_or_default(),
                report
                    .message
                    .map(|message| format!(" -- {}", message))
                    .unwrap_or_default()
            );
        }
        None => println!("nothing to report"),
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  sonn-client [--log-level <level>] [run]");
    eprintln!("  sonn-client install");
    eprintln!("  sonn-client devices");
    eprintln!("  sonn-client pair-remote [address]");
    eprintln!("  sonn-client components");
    eprintln!("  sonn-client --help");
    eprintln!("  sonn-client --version");
    eprintln!();
    eprintln!("Log levels: error, warn, info, debug, trace. The service logs at info unless told");
    eprintln!("otherwise (--log-level, or RUST_LOG); the one-shot commands stay quiet.");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  sudo sonn-client install         # write the systemd unit and start the service");
    eprintln!(
        "  sonn-client devices              # list the sound cards the server will be offered"
    );
    eprintln!("  sonn-client --log-level info run # run in the foreground with logs");
    eprintln!("  sudo sonn-client pair-remote     # pair a Beoremote One without a terminal dance");
}

fn is_pinned(config: &config::Config) -> bool {
    config
        .server_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|url| !url.is_empty())
}

/// Which server this device will attach to, in the words of the config that decided it.
fn describe_server_preference(config: &config::Config) -> String {
    let set = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    if let Some(url) = set(&config.server_url) {
        return format!("pinned to {} (server_url); mDNS discovery is skipped", url);
    }
    if let Some(mac) = set(&config.preferred_server_mac) {
        return format!("looking for the audioserver with mac {} and no other", mac);
    }
    if let Some(name) = set(&config.preferred_server_name) {
        return format!("looking for the audioserver named {:?} and no other", name);
    }
    "no server pinned; attaching to whichever audioserver mDNS finds first".to_string()
}

/// What to log when nobody said.
///
/// The service is the whole point of this program and it runs unattended, so it logs at `info` by
/// default: a device that fails to find its server has to say so in `journalctl`, and silence is the
/// one thing that cannot be diagnosed remotely. The one-shot commands print their own output and stay
/// quiet. `--log-level` wins over everything; `RUST_LOG` is honoured in between so the usual Rust
/// habit works on a device with no way to edit the unit file.
fn default_log_filter(command: Option<&str>, requested: Option<String>) -> String {
    if let Some(level) = requested {
        return level;
    }
    if let Ok(env) = std::env::var("RUST_LOG") {
        if !env.trim().is_empty() {
            return env;
        }
    }
    match command {
        // Crate-scoped: at plain `info` the HTTP and mDNS crates fill the journal with traffic
        // nobody asked about.
        Some("run") | Some("install") | None => "sonn_client=info".to_string(),
        _ => "off".to_string(),
    }
}

fn parse_args() -> Result<(Option<String>, Option<String>, Option<String>)> {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut argument = None;
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
        } else if argument.is_none() {
            argument = Some(arg);
        }
    }

    Ok((command, argument, log_level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_server_is_stated_in_plain_words() {
        let mut config = config::Config {
            device_id: "test".to_string(),
            preferred_server_name: None,
            preferred_server_mac: None,
            server_url: None,
            on_connect: None,
            on_command: None,
            volume_hook: None,
            unrecognised: Default::default(),
        };
        assert!(describe_server_preference(&config).contains("no server pinned"));

        // Whitespace-only counts as unset, the same way the resolver treats it -- otherwise the log
        // would claim a pin that nothing honours.
        config.preferred_server_name = Some("   ".to_string());
        assert!(describe_server_preference(&config).contains("no server pinned"));

        config.preferred_server_name = Some("Test Audioserver".to_string());
        assert!(describe_server_preference(&config).contains("Test Audioserver"));

        config.server_url = Some("http://192.168.1.209:7090".to_string());
        let stated = describe_server_preference(&config);
        assert!(stated.contains("192.168.1.209"), "the url wins: {stated}");
        assert!(is_pinned(&config));
    }

    #[test]
    fn the_service_logs_and_the_one_shot_commands_do_not() {
        // A device nobody is watching has to leave a trail; `devices` and `components` print their
        // answer and would only be cluttered by one.
        assert_eq!(default_log_filter(Some("run"), None), "sonn_client=info");
        assert_eq!(default_log_filter(None, None), "sonn_client=info");
        assert_eq!(default_log_filter(Some("devices"), None), "off");

        assert_eq!(
            default_log_filter(Some("run"), Some("debug".to_string())),
            "debug",
            "an explicit --log-level wins"
        );
        assert_eq!(
            default_log_filter(Some("devices"), Some("trace".to_string())),
            "trace"
        );
    }
}
