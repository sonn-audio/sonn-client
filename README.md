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
- Reports every sound card it can play through *and* capture from, so the server can offer them as a
  picker.
- Runs one Sendspin player per configured card. A Pi with two DACs serves two rooms.
- Runs one Sendspin **source** per configured input: a line-in, a turntable preamp, a CD player goes
  up to the server and comes back as a zone like anything else, with level and line-sense reporting so
  the server knows when someone started playing.
- Serves the menus on a **Beoremote One** and forwards its keys and picks to the server.
- Installs and updates the software those features need (B&O's patched BlueZ), on the server's
  instruction and with a checksum.
- Applies volume, mute and output delay to a live player; reconnects only when it must (a different
  card, a different rate, a different server).
- Drives hardware volume through a hook when a speaker has real volume of its own, leaving the
  software mixer at unity so nothing is attenuated twice.

The protocol, clock filter, decoders (FLAC/Opus/PCM) and the timestamp-scheduled output come from the
[`sendspin`](https://crates.io/crates/sendspin) crate — the upstream Rust implementation. What lives
in this repo is the device agent around it: discovery, registration, sound-card enumeration, the
supervisor that reconciles running players and sources against the server's wishes, the Beoremote
bridge, and the lifecycle glue the crate leaves to its caller.

The `source@v1` role is not upstream yet — it is in the spec and in the Python reference client, so it
is added in our fork ([`sonn-audio/sendspin-rs`](https://github.com/sonn-audio/sendspin-rs), branch
`feat/source-v1`) as one self-contained commit that can be sent upstream as a PR. `Cargo.toml` points
at the fork; there is a commented `[patch]` for building against a local checkout.

## Commands

```bash
sonn-client                       # run (what systemd does)
sudo sonn-client install          # write the systemd unit, enable and start it
sonn-client devices               # list the sound cards the server will be offered
sudo sonn-client pair-remote      # pair a Beoremote One (90s window: scan, pair, trust, connect)
sonn-client components            # what is installed of the managed software
sonn-client --log-level info run  # run in the foreground with logs
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

This repo is written against `sendspin` 0.3.x plus our `source@v1` patch, and against `cpal` 0.18 —
both pre-1.0. The first build is the one that confirms the shapes they expose. The places that touch
them:

- `src/player.rs` — `PlayerV1Support`, `PlayerState`, `PlayerCommand`, `AudioFormatSpec`, `SyncedPlayer`
- `src/source.rs` — `SourceV1Support`, `SourceState`, `InputStreamSource`, `build_input_stream`
- `src/devices.rs` — `DeviceTrait::id` / `description` / `supports_input`, and `SampleRate` (a plain
  `u32` alias in cpal 0.18, not a newtype)

Width-sensitive fields go through `try_into()` on purpose, so a minor bump does not break the build.
The whole thing has been written but not yet compiled on a machine with a Rust toolchain: expect the
first `cargo build` to be where any of the above needs a nudge.

## Line-in over Sendspin

An input on this device becomes a selectable source on the server: capture, a level measurement, and
the one thing only this end can know — whether there is actually audio on the wire. Nobody can start a
turntable remotely, so the device says "I hear something" and the server decides what that means.

A device that is not a network device gets switched on through `control_hook`: the server sends
`activate` when a zone selects the input, and the hook turns that into a MasterLink telegram, a relay,
an IR blast. Without it the chain deadlocks — the input makes no audio until it is on, and nothing
turns it on because nothing asked.

## Beoremote One

With B&O's patched BlueZ installed (see below), a Beoremote One shows your own sources, submenus and
playlists instead of three dots. The menu comes from the server, so adding a playlist is server-side
work with nothing to deploy here; keys go up as raw codes, because only the server knows whether
`next` should advance a queue or become a Beo4 command. Volume is the exception and stays local: it
arrives in bursts and has to survive the server being briefly away.

Pairing is one command (or one button in the server's UI, which queues it):

```bash
sudo sonn-client pair-remote        # then put the remote into pairing mode
```

## Managed components

`beoremote-bluetoothd` — B&O's patched BlueZ 5.45, which is what serves the remote's menus — is
installed by this client on the server's instruction, verified against a sha256, and reported back. It
is deliberately **not** part of this binary: it is GPLv2 (B&O publish their patches because BlueZ
leaves them no choice, and linking it in would relicense this client), and it is a whole `bluetoothd`
that owns the Bluetooth adapter, which most devices have no use for.

Build the artifact from [`beoremote-linux`](https://github.com/sonn-audio/beoremote-linux) and host it
per architecture; the server hands out the URL and checksum.

## Roadmap

- A pairing agent of our own, so an unattended re-pair needs no `bluetoothctl`.
- Optional Opus/FLAC on the way up from a source, for a wifi-only device on a long haul.

Both are sketched in [docs/PROTOCOL.md](docs/PROTOCOL.md#roadmap-on-the-device).
