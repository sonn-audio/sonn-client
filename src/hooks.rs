//! Scripts this device runs on the server's behalf.
//!
//! Three of them, and the contracts are deliberately the reference client's, so a script written for
//! `sendspin --hook-set-volume` keeps working here unchanged:
//!
//! * volume -- `<command> <level>`, with 0 sent for muted. This is how a speaker with real hardware
//!   volume (a BeoLab over MasterLink, an amp behind a GPIO) is driven instead of applying gain in
//!   software and throwing away bits.
//! * connect -- a shell command with `SONN_EVENT` and friends in the environment, for a device that
//!   has to switch something on when it joins a server.
//! * command -- `<script> <command> [args...]` for whatever the server queues. The vocabulary is the
//!   server's; we pass it through untouched so it can add to it without a client release.
//!
//! A failing hook is logged and swallowed. None of them may take audio down with them.

use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Device-level hooks, shared by everything on the box.
pub struct HookRunner {
    on_connect: Option<String>,
    on_command: Option<String>,
    device_id: String,
}

impl HookRunner {
    pub fn new(device_id: String, on_connect: Option<String>, on_command: Option<String>) -> Self {
        Self {
            on_connect,
            on_command,
            device_id,
        }
    }

    /// Fire the connect hook. `event` is `connected` or `disconnected`.
    pub async fn connection_event(&self, event: &str, server_url: &str, server_name: &str) {
        let Some(command) = self.on_connect.as_deref() else {
            return;
        };
        debug!("running connect hook for {} event", event);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .env("SONN_EVENT", event)
            .env("SONN_SERVER_URL", server_url)
            .env("SONN_SERVER_NAME", server_name)
            .env("SONN_CLIENT_ID", &self.device_id);
        run(cmd, "connect").await;
    }

    /// Forward a server-queued command to the command hook.
    pub async fn command(&self, command: &str, args: &[String]) {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(script) = self.on_command.as_deref() else {
            debug!("command {} dropped; no on_command hook configured", trimmed);
            return;
        };
        info!("running command hook: {} {}", trimmed, args.join(" "));
        run(shell_argv(script, trimmed, args), "command").await;
    }
}

/// Spawn via a shell so a configured hook can carry its own arguments, not just be a bare path.
///
/// The command text is left exactly as configured and the hook's arguments arrive as the script's
/// positional parameters -- appending `"$@"` to the text itself would double the arguments for any
/// script that already refers to them.
/// Run one input's control hook as `<script> <command> [args...]`.
///
/// The command name goes through untouched: what `play` or `disc 3` means is the business of the
/// script that speaks to the hardware, not of this client.
pub async fn run_control_hook(script: &str, command: &str, args: &[String]) {
    run(shell_argv(script, command, args), "control").await;
}

fn shell_argv(script: &str, first: &str, rest: &[String]) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("exec {} \"$@\"", script))
        // Placeholder for $0, which a `sh -c` script does not otherwise get.
        .arg("sonn-client")
        .arg(first);
    for arg in rest {
        cmd.arg(arg);
    }
    cmd
}

async fn run(mut cmd: Command, label: &str) {
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => debug!("{} hook finished", label),
        Ok(output) => warn!(
            "{} hook exited with {}: {}",
            label,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => warn!("{} hook could not be started: {}", label, err),
    }
}
