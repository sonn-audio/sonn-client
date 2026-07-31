# 1.0.0 (2026-07-31)


### Bug Fixes

* depend on the fork's integration branch, not the source one ([754c261](https://github.com/sonn-audio/sonn-client/commit/754c261a27cbace1ce535cba840925d81e34f200))
* make it compile, and fix what compiling found ([93c26a3](https://github.com/sonn-audio/sonn-client/commit/93c26a3cbf526621644882a30ac8cb377771e426))
* match the management endpoints the server actually serves ([dbecc84](https://github.com/sonn-audio/sonn-client/commit/dbecc84a3707e92c218b29295e4f856e6d09c4cd))


### Features

* line-in as a sendspin source, the Beoremote One bridge, and the BlueZ it needs ([e1c434c](https://github.com/sonn-audio/sonn-client/commit/e1c434ca6f80a6bae2e487799abe2bf26193ff09))
* sendspin-only client that is installed once and configured from the server ([a70af85](https://github.com/sonn-audio/sonn-client/commit/a70af85a4dd43d450f192a6dd5f24646faf8ab55))

# Changelog

All notable changes to this project are documented here. Releases are cut by semantic-release from
conventional commits; this file is maintained by it from the next release onwards.

## 0.1.0 (unreleased)

First cut. A Sendspin-only endpoint that is installed with one command and configured from the server.

- mDNS discovery of `_sonncore._tcp`, with `preferred_server_name` / `preferred_server_mac` for sites
  running more than one audioserver, and `server_url` to skip discovery entirely.
- Registration and status polling against the management API in `docs/PROTOCOL.md`; every reply is the
  full desired state, so a change in the server's UI lands one poll later.
- Sound-card enumeration reported to the server for selection, re-reported when a card is plugged in
  or removed.
- One Sendspin player per configured card, on the `sendspin` crate: FLAC/Opus/PCM, clock sync, and
  timestamp-scheduled output through cpal.
- Live volume, mute and static delay; reconnect only for changes that need one (card, rate, server).
- Hardware volume through a hook (`<script> <level>`, 0 for muted), with the software mixer left at
  unity so nothing is attenuated twice.
- Sendspin **sources**: a capture device streamed to the server as a selectable line-in, with level
  and line-sense reporting, server-driven start/stop and signal thresholds, and a control hook so a
  non-network device (a BeoSound 9000 on MasterLink) gets switched on when a zone selects it. Uses the
  `source@v1` role from our `sendspin-rs` fork.
- **Beoremote One** support: menus filled from the server, picks and keys forwarded to it, volume
  applied locally to the player and reported back upstream. Replaces the Python bridge.
- **Managed components**: fetch, verify and install B&O's patched BlueZ (`beoremote-bluetoothd`) on the
  server's instruction, including the storage-prefix detection that the reference install script
  learned the hard way. Kept out of this binary for licence and size reasons.
- `pair-remote`: a pairing window (scan, pair, trust, connect) driven by a device command or by hand,
  so adding a remote needs no terminal.
- `install` (systemd unit), `devices` (list sound cards) and a status snapshot at
  `/tmp/sonn-client.status.json`.
