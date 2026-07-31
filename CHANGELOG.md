## [1.0.3](https://github.com/sonn-audio/sonn-client/compare/v1.0.2...v1.0.3) (2026-07-31)


### Bug Fixes

* let the service say what it is doing ([399cbe0](https://github.com/sonn-audio/sonn-client/commit/399cbe0e345ec7de3878ac85eef930f1c10a1901))

## [1.0.2](https://github.com/sonn-audio/sonn-client/compare/v1.0.1...v1.0.2) (2026-07-31)


### Bug Fixes

* **source:** bound the chunk size the server asks for ([3bd5e91](https://github.com/sonn-audio/sonn-client/commit/3bd5e9132b95f1a5e419d36e835dc0c7c76b3ad6))

## [1.0.1](https://github.com/sonn-audio/sonn-client/compare/v1.0.0...v1.0.1) (2026-07-31)


### Bug Fixes

* build for 32-bit Raspberry Pi OS, and name sound cards the way cpal does ([6ff14b0](https://github.com/sonn-audio/sonn-client/commit/6ff14b00dce435f66b102e3814cb00f10ef51fd9))

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
