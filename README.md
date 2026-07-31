# sonn-client

A Sendspin-only audio endpoint for a Raspberry Pi or comparable device, installed with one command and
configured entirely from the audioserver.

```bash
curl -fsSL https://raw.githubusercontent.com/sonn-audio/sonn-client/main/install.sh | sudo bash
```

That is the whole device-side setup. The client finds the audioserver over mDNS, registers, reports
its sound cards, and waits to be given a room. Which card to use, what the room is called, the output
delay, whether volume is done in software or by an amplifier — all of it is decided on the server and
pushed down. Nothing here needs editing, and nothing here needs to be edited again when it changes.

## Why only Sendspin

AirPlay, DLNA, Chromecast, Spotify Connect and Bluetooth all still reach a speaker driven by this
client — they are terminated on the **server**, which turns them into a Sendspin stream aimed here.
The device runs one protocol and nothing else, which is what makes a room a room: one clock, one
buffer model, one place where synchronisation is solved. Adding a second protocol to the device would
mean a second answer to "when should this sample be heard", and there is no good second answer.

## What it does

- Discovers the audioserver (`_sonncore._tcp`), registers, and polls it for its desired state.
- Reports every sound card it can play through, so the server can offer them as a picker.
- Runs one Sendspin player per configured card. A Pi with two DACs serves two rooms.
- Applies volume, mute and output delay to a live player; reconnects only when it must (a different
  card, a different rate, a different server).
- Drives hardware volume through a hook when a speaker has real volume of its own, leaving the
  software mixer at unity so nothing is attenuated twice.

The protocol, clock filter, decoders (FLAC/Opus/PCM) and the timestamp-scheduled output come from the
[`sendspin`](https://crates.io/crates/sendspin) crate — the upstream Rust implementation. What lives
in this repo is the device agent around it: discovery, registration, sound-card enumeration, the
supervisor that reconciles running players against the server's wishes, and the lifecycle glue the
crate leaves to its caller.

## Commands

```bash
sonn-client                      # run (what systemd does)
sudo sonn-client install         # write the systemd unit, enable and start it
sonn-client devices              # list the sound cards the server will be offered
sonn-client --log-level info run # run in the foreground with logs
```

Log levels: `off` (default), `error`, `warn`, `info`, `debug`, `trace`.

```bash
journalctl -u sonn-client -f     # logs
cat /tmp/sonn-client.status.json # last state snapshot, written every 5s
```

## Configuration

`/etc/sonn-client/config.toml` (falling back to `~/.config/sonn-client/config.toml`), written on
first run. See `examples/config.toml`. Fields:

| Field                                        | Purpose                                               |
| -------------------------------------------- | ----------------------------------------------------- |
| `device_id`                                  | stable identity; also the default Sendspin `client_id` |
| `preferred_server_name` / `preferred_server_mac` | which audioserver to attach to, when there are several |
| `server_url`                                 | skip mDNS and pin a server                            |
| `on_connect`                                 | script run on join/leave (`SONN_EVENT=connected\|disconnected`) |
| `on_command`                                 | script run for server-queued device commands           |
| `volume_hook`                                | local default hardware-volume hook                     |

A config file that cannot be parsed is moved aside with a timestamp and replaced with a fresh one, so
a typo cannot stop a speaker from coming back after a reboot.

## Hardware volume

A speaker with real volume of its own should use it. Point `volume_hook` at a script and the client
calls it as `<script> <level>` with the effective level 0–100, sending `0` for muted — the same
contract as the reference client's `--hook-set-volume`, so an existing script works unchanged. While a
hook is in use the software mixer stays at unity: attenuating in both places costs bits and makes the
zone slider non-linear.

Normally the server pushes this per player (`players[].volume_hook`); the config field is the local
default.

## Management protocol

The device reports, the server decides. Audio is plain Sendspin and stays that way; the management
channel is a small HTTP API on the server. Both payloads, their semantics, and the server-side work
still to do are written up in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Build

```bash
sudo apt-get install -y libasound2-dev pkg-config
cargo build --release
sudo install -m 0755 target/release/sonn-client /usr/local/bin/
sudo sonn-client install
```

Release builds for all four Linux targets are cross-compiled in CI (`cross build --release --target
…`); `Cross.toml` carries the per-architecture ALSA setup. Targets:

| Target                          | Devices                       |
| ------------------------------- | ----------------------------- |
| `x86_64-unknown-linux-gnu`      | PC, NUC, VM                   |
| `aarch64-unknown-linux-gnu`     | Pi 5 / 4 / 3 on a 64-bit OS   |
| `armv7-unknown-linux-gnueabihf` | Pi 3 / 2 on a 32-bit OS       |
| `arm-unknown-linux-gnueabihf`   | Pi 1 / Zero                   |

### First build

This repo was written against `sendspin` 0.3.x, whose API is documented but pre-1.0. The first build
is the one that confirms the integer widths and struct shapes it uses; the places that touch them are
`src/player.rs` (`PlayerV1Support`, `PlayerState`, `PlayerCommand`, `AudioFormatSpec`, `SyncedPlayer`)
and its tests. Width-sensitive fields go through `try_into()` on purpose, so a minor bump does not
break the build.

## Roadmap

- **Beoremote bridge** — Beoremote One pairs as a Bluetooth HID keyboard; mapping its keys to Sendspin
  group commands means adding the controller role to the connection.
- **Managed binaries** — install and update the custom Bluetooth binary from the server, the way
  `install.sh` already updates the client itself.

Both are sketched in [docs/PROTOCOL.md](docs/PROTOCOL.md#roadmap-on-the-device).
