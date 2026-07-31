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
| `client_register` | `{api}/clients/register`             | register endpoint              |
| `client_status`   | `{api}/clients/{device_id}/status`   | status endpoint, `{device_id}` substituted |
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
  "outputs": [
    {
      "id": "hw:CARD=DAC,DEV=0",
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
  "capabilities": { "codecs": ["flac", "opus", "pcm"], "max_players": 4, "features": [] }
}
```

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
      "output": "hw:CARD=DAC,DEV=0",
      "codec": "flac",
      "sample_rate": 44100,
      "bit_depth": 16,
      "channels": 2,
      "volume": 42,
      "muted": false,
      "static_delay_ms": 0,
      "clock_rtt_ms": 1.34,
      "clock_quality": "Good",
      "last_error": null
    }
  ]
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
      "output": "hw:CARD=DAC,DEV=0",
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
      "volume_hook": null
    }
  ],
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
- **`volume_hook`** switches this player to hardware volume: the client runs `<command> <level>`
  (0 when muted) and leaves its software mixer at unity, so the level is not attenuated twice. Same
  contract as the reference client's `--hook-set-volume`.
- **`commands`** are one-shot, oldest first, and drained on the poll that returns them. The
  vocabulary is the server's; the client passes each one to its command hook untouched, so the server
  can add commands without a client release.

## What the server side needs (not yet built)

The client is complete against this contract; the server end is not. In the audioserver repo:

1. **`src/adapters/discovery/sonnCoreMdnsService.ts`** — add the two TXT keys next to the existing
   `linein_*` ones.
2. **A device-facing handler**, alongside `src/adapters/http/lineInApi/lineInApiHandler.ts`, matching
   `/api/clients`. Same shape as the line-in bridge registry: keep a `Map<deviceId, record>` of the
   last registration plus the last status, with a staleness window so a device that stops polling
   shows as offline. Route it in `httpService.ts` where `lineInApi.matches(pathname)` is handled.
3. **Config** — the desired state has to come from somewhere persistent: per device, a name and a
   list of players (client_id, output id, delay, volume mode). The natural home is a `sonnClients`
   section in the config, with `sendspin_url` derived from the server's own http host/port rather
   than stored.
4. **Admin API + UI** — `GET /admin/api/clients` returning registration + status + the configured
   players, and a view where the sound-card list from `outputs` becomes a picker. Zones need no
   change at all: a Sonn client is an ordinary Sendspin output, so once a `client_id` exists it can
   be assigned as a zone output or as a satellite exactly like any other.
5. **The zone-side link** — when a player is assigned to a zone, the zone's sendspin output config
   points at that `client_id`. Nothing else to wire: the client dials the server, so it appears in
   `sendspinCore.listClients()` and in `GET /transports/sendspin/clients` on its own.

## Roadmap on the device

Two things are designed for but not implemented, both of which the hooks above already carry the
plumbing for:

- **Beoremote bridge.** A Beoremote One pairs over Bluetooth HID and arrives as an evdev keyboard.
  Turning its keys into Sendspin group commands means adding the controller role to the client's
  connection and mapping keycodes to media commands — the device sends, rather than only receives.
  Volume keys are the one case worth special-handling: they map to the zone's volume, which is where
  `volume_hook` and the 0–90 B&O scale meet.
- **Managed binaries.** The custom Bluetooth binary needs to be installed and kept up to date from
  the server. That is a `features`/`managed_binaries` field in the register payload plus a download
  URL and version in the desired state, and a small verify-then-atomically-replace step on the
  device — the same shape `install.sh` already uses for the client itself.
