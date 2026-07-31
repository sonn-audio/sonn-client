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
- `install` (systemd unit), `devices` (list sound cards) and a status snapshot at
  `/tmp/sonn-client.status.json`.
