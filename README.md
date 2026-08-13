# sonn-client

A Raspberry Pi (or comparable box) turned into part of an audio system, installed with one command and
configured entirely from the audioserver.

It has four jobs, and every one of them is optional:

| | |
| --- | --- |
| **Output** | Plays a room. One player per sound card, so a Pi with two DACs serves two rooms. |
| **Input** | Listens to a turntable, a CD player, anything with a line out, and offers it to the server as a source. |
| **Bluetooth** | Takes a phone: pairs it, receives the audio, and hands it to a room as an ordinary input. |
| **Remote** | Serves the menus on a Beoremote One and sends its keys and picks to the server. |

They are independent. A Pi standing at the record player can do the input and the Bluetooth without
having a speaker attached at all, while the room those end up in is a different box across the house.
A box with nothing but a DAC does only the output. Which of the four a device does is decided on the
server, not here.

```bash
curl -fsSL https://raw.githubusercontent.com/sonn-audio/sonn-client/main/install.sh | sudo bash
```

That is the whole device-side setup. The client finds the audioserver over mDNS, registers, reports
its sound cards, and waits to be given a room. Which card to use, what the room is called, the output
delay, whether volume is done in software or by an amplifier — all of it is decided on the server and
pushed down. Nothing here needs editing, and nothing here needs to be edited again when it changes.

## Why playback is Sendspin and nothing else

AirPlay, DLNA, Chromecast and Spotify Connect all still reach a speaker driven by this client — they
are terminated on the **server**, which turns them into a Sendspin stream aimed here. For playback the
device runs one protocol and nothing else, which is what makes a room a room: one clock, one buffer
model, one place where synchronisation is solved. A second playback protocol on the device would mean
a second answer to "when should this sample be heard", and there is no good second answer.

Bluetooth is the exception that proves the rule, and it is an *input*. A phone's radio only reaches
the room it is in, so the thing that hears it has to be in that room too — but what leaves this client
is PCM announced as an ordinary source, exactly like the turntable next to it. Nothing about the
playback side changes: the room still hears one protocol, on one clock.

## What it does

- Discovers the audioserver (`_sonncore._tcp`), registers, and polls it for its desired state.
- Reports every sound card it can play through *and* capture from, so the server can offer them as a
  picker.
- Runs one Sendspin player per configured card. A Pi with two DACs serves two rooms.
- Runs one Sendspin **source** per configured input: a line-in, a turntable preamp, a CD player goes
  up to the server and comes back as a zone like anything else, with level and line-sense reporting so
  the server knows when someone started playing.
- Pairs a **phone over Bluetooth**, decodes what it sends, and offers it to the server as a source —
  with the phone's own now-playing, its volume slider, and the room's name on the phone's screen.
- Serves the menus on a **Beoremote One** and forwards its keys and picks to the server, on stock
  BlueZ: the client provides the remote's own service and reads its keys from the kernel.
- Updates itself, on the server's instruction and against a checksum.
- Applies volume, mute and output delay to a live player; reconnects only when it must (a different
  card, a different rate, a different server).
- Drives hardware volume through a hook when a speaker has real volume of its own, leaving the
  software mixer at unity so nothing is attenuated twice.

The protocol, clock filter, decoders (FLAC/Opus/PCM) and the timestamp-scheduled output come from the
[`sendspin`](https://crates.io/crates/sendspin) crate — the upstream Rust implementation. What lives
in this repo is the device agent around it: discovery, registration, sound-card enumeration, the
supervisor that reconciles running players and sources against the server's wishes, the Beoremote
bridge, and the lifecycle glue the crate leaves to its caller.

The dependency is our fork ([`sonn-audio/sendspin-rs`](https://github.com/sonn-audio/sendspin-rs),
branch `sonn`). It carries the `source@v1` role, which is in the spec and in the Python reference
client but not in the published crate — and, just as much for playback, the fixes a *player* needs
against our own server: a paused group that would otherwise fail to parse and take the whole
`group/update` with it, pitch frames, the connection reasons this server sends, and the transmit
stamp that `required_lead_time_ms` is measured from.

Each of those is a single-purpose branch for upstream; `sonn` is where they are combined so this
client depends on one thing. `Cargo.toml` has a commented `[patch]` for building against a local
checkout.

## Commands

```bash
sonn-client                       # run (what systemd does)
sudo sonn-client install          # write the systemd unit, enable and start it
sonn-client devices               # list the sound cards the server will be offered
sudo sonn-client pair-remote      # pair a Beoremote One (90s window: scan, pair, trust, connect)
sudo sonn-client bluetooth        # be findable as a speaker, without a server (name, seconds)
sonn-client devices --log-level debug  # ...including the ones left out, and why
sonn-client --log-level info run       # run in the foreground with logs
```

Log levels: `error`, `warn`, `info`, `debug`, `trace`. The service logs at `info`; the one-shot
commands print their answer and stay quiet. `--log-level` wins over `RUST_LOG`, which wins over both
defaults.

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
a typo cannot stop a speaker from coming back after a reboot. A key this build does not recognise is
kept and named in the log rather than acted on -- a misspelled setting reads perfectly and does
nothing, which is the hardest kind of fault to see from the other end of a network.

`preferred_server_name` and `preferred_server_mac` select, they do not suggest: with a preference set,
no other audioserver is used. The effective choice is logged at startup, so "the setting did not take"
and "the server is not there" do not look alike.

## Hardware volume

A speaker with real volume of its own should use it. Volume therefore goes to the sound card's own
playback mixer whenever it has one, and the software mixer stays at unity — attenuating in both
places costs bits and makes the zone slider non-linear. Where a card has no mixer, gain is applied in
software as before.

The server decides per speaker with `players[].volume_control`:

| Value | Meaning |
| ---------- | -------------------------------------------------------------- |
| `auto` (default) | the card's mixer if it has one, software gain if not |
| `alsa` | always the card's mixer |
| `software` | never touch the mixer |
| `hook` | run `volume_hook` |

A `volume_hook` is a deliberate act, so it wins over a mixer that was merely found: point it at a
script and the client calls `<script> <level>` with the effective level 0–100, sending `0` for muted
— the same contract as the reference client's `--hook-set-volume`, so an existing script works
unchanged. The config file's `volume_hook` is the local default for players the server gave none.

### Which scale a mixer is on

`amixer -M` spreads a percentage perceptually across the raw register range. That is right for the
DAC HATs whose mixers are linear in register steps — without it, 50% lands halfway down the register
and sounds far quieter than expected. It is wrong for a mixer already calibrated in dB, where one
step is one dB: there the hardware does the perceptual mapping itself and `-M` lays a second curve on
top, so a percentage no longer corresponds to a known attenuation. On a B&O BeoLab over USB, which
reports `0-90` spanning `-90..0 dB`, 30% is -63 dB with `-M` and -30 dB without.

The client works this out by reading the mixer rather than asking: a card whose current level in dB
equals its distance from the top in steps is calibrated in dB and is addressed directly. At maximum
both readings are zero on any mixer, so nothing is concluded there. `players[].mixer_mapped` settles
it outright for a card that reads wrongly, and `players[].mixer_element` names the element when the
usual ones (`Digital`, `Master`, `PCM`) are not what this card calls it.

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

### Building against the fork

Until the fork is pushed, build against a checkout next door — `sendspin-rs` beside this repo, on
`sonn` — by uncommenting the `[patch]` block at the bottom of `Cargo.toml`.

Verified with cargo 1.97: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are
clean and `cargo test` passes (35). What that does *not* cover is a real device: no player, source,
remote bridge or component install has been run against hardware yet.

## Line-in over Sendspin

An input on this device becomes a selectable source on the server: capture, a level measurement, and
the one thing only this end can know — whether there is actually audio on the wire. Nobody can start a
turntable remotely, so the device says "I hear something" and the server decides what that means.

A device that is not a network device gets switched on through `control_hook`: the server sends
`activate` when a zone selects the input, and the hook turns that into a MasterLink telegram, a relay,
an IR blast. Without it the chain deadlocks — the input makes no audio until it is on, and nothing
turns it on because nothing asked.

## Beoremote One

A Beoremote One shows your own sources, submenus and playlists instead of three dots — on stock
BlueZ. B&O do it with a patched daemon of their own; this client instead serves the remote's service
itself and reads its keys from the kernel's input devices, so there is nothing to install on the
device beyond this binary. The menu comes from the server, so adding a playlist is server-side
work with nothing to deploy here; keys go up as raw codes, because only the server knows whether
`next` should advance a queue or become a Beo4 command. Volume is the exception and stays local: it
arrives in bursts and has to survive the server being briefly away.

Pairing is one command (or one button in the server's UI, which queues it):

```bash
sudo sonn-client pair-remote        # then put the remote into pairing mode
```

## Bluetooth

A phone pairs with the room rather than with a box: the adapter takes the zone's name, so what
someone sees in their phone's list is "Kitchen". The audio arrives as SBC, is decoded here, and goes
up as an ordinary source — which is why nothing downstream knows or cares that a room's music came
from a phone. What the phone says it is playing, and where its volume slider is, come along with it.

Two things are worth knowing before deploying it next to a remote. One radio serves both, and a
Beoremote One holds its connection open the whole time, which measurably starves the audio (83-94% of
real time arriving, against 99-100% with the remote gone) — so the remote stands aside while a phone
plays and is called back the moment it stops. And artwork does not travel: a phone offers cover art
over a separate OBEX channel that BlueZ has no client for, so the room shows title, artist and album
but no picture.

## Updates

The client updates itself on the server's instruction, verified against a sha256 and reported back.
It waits for the music to stop before replacing itself.

## Roadmap

- Transport keys from the room back to a phone over AVRCP: the client end is built, the server does
  not yet queue them.
- Cover art over Bluetooth, which needs an AVRCP controller and an OBEX client of our own.
- Optional Opus/FLAC on the way up from a source, for a wifi-only device on a long haul.

Both are sketched in [docs/PROTOCOL.md](docs/PROTOCOL.md#roadmap-on-the-device).
