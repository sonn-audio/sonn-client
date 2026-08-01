# Management protocol

Sonn Client speaks two things to the server, and they are deliberately separate.

**Audio** is plain Sendspin, unchanged. The client connects to the server's Sendspin endpoint as an
ordinary player, negotiates a format, and plays timestamped chunks against the shared clock. Nothing
in this document touches that: the audio path stays spec-conformant, which is what keeps the client
usable against any Sendspin server and keeps the server's existing output code working without a
special case for "our" clients.

**Management** is the small HTTP API described below. The Sendspin spec has no message for "use the
second sound card" or "you are the kitchen now", and inventing one would fork the protocol. So the
device reports what it has over HTTP and the server replies with what it should be.

The split has a practical consequence worth stating: management works before audio does. A device
with no zone assigned still registers, still lists its sound cards, and still shows up in the UI as
something waiting to be given a room.

## Discovery

The client browses for `_sonncore._tcp` — the service the audioserver already advertises — and reads
its endpoint paths from TXT records:

| TXT key           | Default                              | Meaning                        |
| ----------------- | ------------------------------------ | ------------------------------ |
| `api`             | `/api`                               | API root, already advertised   |
| `client_register` | `{api}/sonnclients/register`         | register endpoint              |
| `client_status`   | `{api}/sonnclients/{device_id}/status` | status endpoint, `{device_id}` substituted |
| `mac`             | —                                    | already advertised; used by `preferred_server_mac` |

Both paths have defaults, so a server that adds nothing to its TXT records still works. Adding them
is only needed if the endpoints ever move.

With several audioservers on one network, `preferred_server_name` (matched against the advertised
instance name) or `preferred_server_mac` in `config.toml` picks one. Preferences apply even when only
one server answers: attaching to the wrong server silently is worse than waiting.

## `POST {client_register}`

Sent once per attachment (and again after every reconnect). The device says what it is; the server
replies with the desired state.

```json
{
  "device_id": "sonn-kitchen-pi-9e2f41a7",
  "agent": "sonn-client",
  "version": "0.1.0",
  "hostname": "kitchen-pi",
  "ip": "192.168.1.42",
  "mac": "DC:A6:32:1B:44:90",
  "model": "Raspberry Pi 4 Model B Rev 1.4",
  "os": "Debian GNU/Linux 12 (bookworm) aarch64",
  "arch": "aarch64",
  "outputs": [
    {
      "id": "alsa:hw:CARD=DAC,DEV=0",
      "name": "Topping E30 Analogue Stereo",
      "channels": 2,
      "sample_rates": [44100, 192000],
      "is_default": false
    },
    {
      "id": "default",
      "name": "Default ALSA Device",
      "channels": 2,
      "sample_rates": [44100, 48000],
      "is_default": true
    }
  ],
  "inputs": [
    {
      "id": "alsa:hw:CARD=CODEC,DEV=0",
      "name": "USB Audio CODEC",
      "channels": 2,
      "sample_rates": [44100, 48000],
      "is_default": true
    }
  ],
  "capabilities": {
    "codecs": ["flac", "opus", "pcm"],
    "max_players": 4,
    "features": ["source", "beoremote", "components"]
  },
  "components": [{ "name": "beoremote-bluetoothd", "state": "absent" }]
}
```

`inputs` is the same shape as `outputs` — to the server a sound card is a sound card — and feeds the
source role below. `features` says what this build can be asked to do; `components` says what is
installed of the software that some of those features need.

`outputs[].id` is a cpal device id — on Linux the ALSA name. It is what the server sends back in
`players[].output`, so it has to be stored as opaque text and handed back unchanged. The list is
sorted with the host default first and then by id, so a picker built on it does not reshuffle between
polls.

`device_id` is generated once on the device and kept in `config.toml`. It is also the default
Sendspin `client_id`, which matters: a zone's output configuration points at a `client_id`, so this is
the value that must stay stable for as long as the user expects that room to keep working.

## `POST {client_status}`

Every poll (default 5 s, server-adjustable). Reports what the device is doing; the reply is the full
desired state again, so a change made in the UI lands one poll later without the server having to
reach back in.

```json
{
  "state": "streaming",
  "version": "0.1.0",
  "uptime_s": 8123,
  "players": [
    {
      "client_id": "sonn-kitchen-pi-9e2f41a7",
      "state": "streaming",
      "output": "alsa:hw:CARD=DAC,DEV=0",
      "codec": "flac",
      "sample_rate": 44100,
      "bit_depth": 16,
      "channels": 2,
      "volume": 42,
      "muted": false,
      "static_delay_ms": 0,
      "clock_rtt_ms": 1.34,
      "clock_quality": "good",
      "last_error": null
    }
  ],
  "sources": [
    {
      "client_id": "sonn-kitchen-pi-9e2f41a7-linein",
      "state": "streaming",
      "input": "alsa:hw:CARD=CODEC,DEV=0",
      "codec": "pcm",
      "sample_rate": 48000,
      "bit_depth": 16,
      "channels": 2,
      "level": 0.31,
      "signal": "present",
      "clock_rtt_ms": 1.21
    }
  ],
  "components": [
    { "name": "beoremote-bluetoothd", "version": "5.45-bo1", "state": "running" }
  ],
  "beoremote": {
    "state": "connected",
    "zone_id": 28,
    "menu_revision": "5787de5a",
    "hid_connected": true
  }
}
```

`outputs` is only present when the set of sound cards changed — a USB DAC plugged in after boot has
to appear in the picker, but repeating an unchanged list on every poll is noise. A server must
therefore keep the last list it was given rather than treating "absent" as "none".

Device `state` is a roll-up: `streaming` beats `connected` beats `connecting`, and `error` is only
reported when *every* player is in error. A device with one dead card and one playing room is not
broken.

## Desired state (the reply to both calls)

```json
{
  "device_name": "Kitchen",
  "sendspin_url": "ws://192.168.1.209:7090/sendspin",
  "poll_interval_ms": 5000,
  "players": [
    {
      "client_id": "sonn-kitchen-pi-9e2f41a7",
      "name": "Kitchen",
      "output": "alsa:hw:CARD=DAC,DEV=0",
      "enabled": true,
      "codecs": ["flac", "pcm"],
      "sample_rate": null,
      "bit_depth": null,
      "channels": 2,
      "static_delay_ms": 0,
      "volume": 100,
      "muted": false,
      "buffer_ms": 500,
      "required_lead_time_ms": 500,
      "volume_hook": null,
      "volume_control": "auto",
      "mixer_element": null,
      "mixer_mapped": null
    }
  ],
  "sources": [
    {
      "client_id": "sonn-kitchen-pi-9e2f41a7-linein",
      "name": "BeoSound 9000",
      "input": "alsa:hw:CARD=CODEC,DEV=0",
      "enabled": true,
      "sample_rate": 48000,
      "bit_depth": 16,
      "channels": 2,
      "frame_ms": 20,
      "threshold_db": -45.0,
      "hold_ms": 2000,
      "controls": ["activate", "deactivate", "play", "pause", "next", "previous"],
      "control_hook": "/usr/local/bin/ml-cmd",
      "always_on": false
    }
  ],
  "beoremote": {
    "enabled": true,
    "zone_id": 28,
    "menu_poll_ms": 10000,
    "volume_player": "sonn-kitchen-pi-9e2f41a7",
    "volume_step": 4
  },
  "components": [
    {
      "name": "beoremote-bluetoothd",
      "version": "5.45-bo1",
      "url": "https://.../beoremote-bluetoothd-5.45-bo1-aarch64.tar.gz",
      "sha256": "…",
      "enabled": true
    }
  ],
  // one artifact, already chosen for this device's `arch`
  "commands": []
}
```

Semantics the server should count on:

- **`sendspin_url` absent or empty** — the device stays registered and keeps polling, playing
  nothing. This is the normal state of a freshly installed device, not an error.
- **`players` is the whole list.** A player that disappears is stopped; one that appears is started.
  Several players on one device is supported (one per sound card), which is how a Pi with two DACs
  serves two rooms.
- **`enabled: false`** parks a player without removing it. The room stays configured, just silent.
- **Reconnect vs. live.** `output`, `name`, `codecs`, `sample_rate`, `bit_depth`, `channels`,
  `buffer_ms`, `required_lead_time_ms` and `sendspin_url` can only change by reconnecting that
  player, and changing one does exactly that. `volume`, `muted` and `static_delay_ms` are applied to
  the running player.
- **`volume` is a seed, not a setpoint.** It is applied when a player starts and whenever the *value
  in this config changes*. It is not re-applied on every poll, because live volume arrives over
  Sendspin (`server/command` → `volume`/`mute`) from the same server: re-asserting a stale seed on
  every poll would fight the zone slider.
- **`sample_rate`/`bit_depth` null** means "advertise everything this build supports" — 44.1 and 48
  kHz at 16 and 24 bit. That is what lets the server's bit-perfect path pass a 44.1 kHz album through
  without a resample. Pin them only for hardware that genuinely accepts one rate.
- **`volume_control`** says where volume is applied: `auto` (the default) uses the card's own mixer
  when it has one and software gain when it does not, `alsa` and `software` insist, and `hook` runs
  the script. Whatever applies it, the software mixer stays at unity so the level is not attenuated
  twice. `mixer_element` names the mixer control if the usual ones are not what this card calls it,
  and `mixer_mapped` overrides the client's reading of the mixer's scale — see the README for what
  that reading is and why it matters on a mixer calibrated in dB.
- **`volume_hook`** is the script `hook` runs, and wins over a mixer when `volume_control` is `auto`:
  the client runs `<command> <level>` (0 when muted). Same contract as the reference client's
  `--hook-set-volume`.
- **`commands`** are one-shot, oldest first, and drained on the poll that returns them. The
  vocabulary is the server's; the client passes each one to its command hook untouched, so the server
  can add commands without a client release.

## Sources (line-in over Sendspin)

A source is a player in reverse: the device captures a local input and streams it *to* the server,
which resamples, mixes and distributes it like any other audio. The zone side is unchanged — the
server already maps a Sendspin source client to a line-in input by `client_id`.

The client implements `source@v1` as written in the spec and in the Python reference client:

- `client/hello` carries `source@v1_support` with the capture format, the transport controls this
  input will act on, and `level` / `line_sense` reporting.
- `server/command` drives it: `start` / `stop`, signal thresholds (`vad`), and transport controls for
  the device on the other end of the cable.
- `input_stream/start` announces each stream's format before its first frame; `input_stream/end` ends
  it; `input_stream/request-format` is answered by re-announcing the capture format, since the client
  produces exactly one.
- Audio goes up as binary type 12 frames, timestamped in the *server's* clock.
- `client/state` reports capture state, level and signal presence; `client/command` reports
  `started` / `stopped` when the level crosses the threshold and stays across it.

Level and signal are reported **whether or not the source is streaming**. That is the whole point of
line sensing: nobody can start a turntable remotely, so the device says "I hear something" and the
server decides whether that means the zone should switch to it.

`control_hook` is what makes a non-network device usable: the server sends `activate` when a zone
selects this input, and the hook turns that into whatever the hardware understands — a MasterLink
telegram for a BeoSound 9000, a relay, an IR blast. Without it the chain deadlocks: the input produces
no audio until it is switched on, and nothing switches it on because nothing asked.

Support for this role is not in the upstream Rust library yet. It lives in our fork
(`sonn-audio/sendspin-rs`, branch `feat/source-v1`) as one self-contained commit so it can go upstream
as a PR; `Cargo.toml` points at it.

## Beoremote One

A Beoremote One paired to a stock Linux box is a keyboard: press MUSIC and the display shows three
dots, because the list has to come from the host. B&O's own BlueZ plugin serves that list and exposes
two unix sockets for whoever fills it in. This client is that "whoever", replacing the Python bridge
that used to sit next to the player.

```text
/var/run/beoremote_one_socket   menus, volume, selections   (plugin listens, we connect)
/tmp/streamsdk_hog              raw 2-byte HID key reports  (we listen, hog connects)
```

- The **menu is the server's**. `GET /api/beoremote/zones/{zone}/menu` returns sources, the one
  submenu and a revision; picks go back to `POST …/select` carrying that revision, so a list that
  changed since it was rendered cannot start the wrong thing. A new playlist appears on the remote
  with nothing deployed on the device.
- **Keys go up as raw codes** to `POST …/key`. Only the server knows what the zone is playing — a
  source picked in the app never passes through the device — so it decides whether `next` advances a
  queue or becomes a Beo4 command on a MasterLink bus.
- **Volume stays local.** It arrives in bursts (six presses in a row is normal) and has to keep
  working while the server is briefly away, so it is applied to the player directly and reported back
  upstream in `client/state`. That reporting is new: with the old bridge the zone slider did not
  follow the remote.

`hid_connected` in the status report is worth watching: while that socket has no peer, bluetoothd
falls back to uHID and the keys arrive as evdev events that nothing reads. The listener is therefore
created before anything else, because the fallback is decided per connection and is sticky.

## Managed components

`beoremote-bluetoothd` — B&O's patched BlueZ 5.45 — is fetched, verified and installed by the client
on request, rather than being part of the binary. Two reasons, both decisive:

- It is **GPLv2**. B&O publish their BlueZ patches because the licence leaves them no choice; linking
  that daemon into this client would relicense the client.
- It is a whole `bluetoothd` that takes over the Bluetooth adapter, which most devices running this
  client have no use for.

So the server names a version, a URL and a **sha256** (required — this installs a daemon that owns the
adapter), and the client does the rest: verify, unpack, install, write the unit, disable the stock
`bluetooth.service` (both claim `org.bluez` and the same adapter), start it, and report the version
back. `enabled: false` removes it again, leaving `/var/lib/bluetooth` alone so a remote does not have
to be re-paired.

One detail the artifact has to respect: **the install prefix is baked into the binary** by
`./configure --prefix`, and it is not where the binary ends up. The client reads it back out of the
ELF and creates the storage symlink and `main.conf` under *that* path. Guessing it wrong is silent —
the daemon starts, reads no config, and stores pairings where nobody looks, which shows up as "the
remote pairs but is gone after a reboot".

Build the artifact from `beoremote-linux` (`./build.sh`, then tar up `bluetoothd` and
`etc/bluetooth/main.conf`), one per architecture.

## Pairing a remote

`pair_remote` as a device command (or `sonn-client pair-remote [address]` by hand) opens a 90-second
window: scan, pair, trust, connect. Progress comes back in the status report's `pairing` block, so the
server can show it as a button with a result instead of an SSH session. `trust` is the step that is
easy to forget and annoying to debug — without it every reconnect needs re-authorising, so the remote
works once and then looks dead.

It is driven through `bluetoothctl` rather than D-Bus on purpose: bluetoothd refuses to pair without
an *agent* registered to answer its questions, and `bluetoothctl` brings one.

## The server side

Built, in the audioserver repo:

- **`POST /api/sonnclients/register`** and **`POST /api/sonnclients/{device_id}/status`** in
  `src/adapters/http/sonnClientApi/sonnClientApiHandler.ts`. Ungated, like `/api/linein`: a speaker
  has no admin session. A device that registers is written into the config on first sight — identity
  only — so it appears in the admin UI as something waiting to be given a room.
- **Desired state** assembled from `config.sonnClients.devices[]` on every reply. `sendspin_url` is
  built from the request's own Host header, because that is by definition an address the device just
  reached the server on; a reconstructed `host:port` is a guess, and on a multi-homed machine usually
  the wrong one.
- **`client_register` / `client_status`** TXT records on `_sonncore._tcp`, so the paths can move
  without a release on every speaker.
- **Admin API** under `/admin/api/sonnclients`: list, read one, `PUT` a device's configuration,
  `DELETE` to forget it, and `POST …/commands` to queue `pair_remote`. A `DELETE` is refused with 409
  while any zone output, satellite or line-in input still points at one of the device's client ids —
  forgetting it would leave that room silent with nothing to explain why.
- **Components** are resolved server-side: the catalogue in `config.sonnClients.components[]` holds a
  URL and sha256 per architecture, and the device is handed the one matching the `arch` it reported.
  An entry with no artifact for that architecture is left out rather than sent without a URL.

Still to do: a screen in the admin UI. The API is shaped for one — the card lists come back in the
registration, so the picker is a `<select>` over `outputs[]` and `inputs[]`, and everything else is a
form over the device record.

Zones need no changes at all: a Sonn client's player is an ordinary Sendspin output, so a room is
assigned on the Zones screen against the `client_id` this screen created. Sources likewise —
`SendspinLineInService` already maps a line-in input whose `source.type` is `sendspin` to a client id
and consumes its type-12 frames.

## Roadmap on the device

- **MasterLink** is deliberately out of scope here. A BeoSound 9000 reaches the server as a *source*
  and takes its commands through `control_hook`, which is where the existing `ml-cmd` script fits. The
  bus itself stays where it already works.
- **A pairing agent of our own.** `bluetoothctl` supplies the agent during a pairing window, which
  covers pairing but not an unattended re-pair. A small `org.bluez.Agent1` implementation would remove
  the last external dependency; it needs a D-Bus client, which nothing else here does.
- **Opus or FLAC on the way up.** Sources send PCM: on a LAN it costs 1.5 Mbit/s and saves a Pi's CPU.
  A wifi-only device with a long haul might prefer to encode, which is a decoder the *server* already
  has.
