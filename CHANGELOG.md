# [1.5.0](https://github.com/sonn-audio/sonn-client/compare/v1.4.2...v1.5.0) (2026-08-13)


### Bug Fixes

* **beoremote:** a database bluez lays out the way we wrote it ([69d6e57](https://github.com/sonn-audio/sonn-client/commit/69d6e579ddf5896d14402e6adfe254a9ce4090fb))
* **beoremote:** an empty favorite is an answer, not a failure ([b71ae89](https://github.com/sonn-audio/sonn-client/commit/b71ae890e30d25b1e429a407567fa2195364d26b))
* **beoremote:** rebuild a link that was up before the bridge ([12e6191](https://github.com/sonn-audio/sonn-client/commit/12e619157ae6816a823d8802d6eb8cc376737a74))
* **beoremote:** reopen the remote's input devices, and say what a key did ([12b1d56](https://github.com/sonn-audio/sonn-client/commit/12b1d56ef6214528f965beaa124a51f03ed1759d))
* **beoremote:** take the writes the remote actually sends, and keep our handles ([4125c31](https://github.com/sonn-audio/sonn-client/commit/4125c31e4667178e51860e75f032202814916a1c))
* **beoremote:** the same handles on every start ([56264a0](https://github.com/sonn-audio/sonn-client/commit/56264a048080fe4d9589a24cc168eb227d27c103))
* **bluetooth:** a remote is not a phone ([cb3e6ee](https://github.com/sonn-audio/sonn-client/commit/cb3e6ee1117ed850ddb31c61aecaae2d4dbc23ca))
* **bluetooth:** hand the audio on the way it was played ([b939ac9](https://github.com/sonn-audio/sonn-client/commit/b939ac9a09aad7d05cdedb98719a3c73c61da2ea))
* **bluetooth:** keep the phone's audio, once it starts ([8a6a623](https://github.com/sonn-audio/sonn-client/commit/8a6a623aec5a8f0e7270ea769e7493e35fe5d4f7))
* **bluetooth:** stop tugging at a phone that is simply not playing ([e6911b0](https://github.com/sonn-audio/sonn-client/commit/e6911b0e6c07902e144dff853a1984878ca8febe))
* **bluetooth:** the phone's clock is the only clock ([f257dc1](https://github.com/sonn-audio/sonn-client/commit/f257dc1de4304bc0f655413fc0f36d843ae25e04))
* **client:** introduce yourself again to a server that has forgotten you ([ba30668](https://github.com/sonn-audio/sonn-client/commit/ba3066851bbf14a62e30cc01a86296c7dfe1cb3f))
* **pairing:** hold an agent open for the whole window ([f175066](https://github.com/sonn-audio/sonn-client/commit/f175066f146fce53c75a5886e1a49d170808301b))
* **pairing:** scan LE-only, retry once, and let the remote finish ([1bf71b4](https://github.com/sonn-audio/sonn-client/commit/1bf71b445fd0b8d51199672a4845cc08c8893258))


### Features

* **beoremote:** serve the remote's service ourselves ([c2d624f](https://github.com/sonn-audio/sonn-client/commit/c2d624fef3739a2dcb251f1c0ee6133583ed0333))
* **bluetooth:** a phone pairs with the client and plays to a zone ([06ea951](https://github.com/sonn-audio/sonn-client/commit/06ea95126b9098d2b0dc64732116a75f152190d6))
* **bluetooth:** send what a phone plays to the server ([171c7ed](https://github.com/sonn-audio/sonn-client/commit/171c7ede5907e8621a5427677d4a16a63570ddf5))
* **bluetooth:** the phone's slider is the room's volume ([f52d63b](https://github.com/sonn-audio/sonn-client/commit/f52d63bafbd8a8094c2af2c30f22aef5f0e50055))
* **bluetooth:** the radio carries the room's name, for everything that reads it ([d9cea28](https://github.com/sonn-audio/sonn-client/commit/d9cea28a024ed26edade3f89b36988634f319c3e))
* **bluetooth:** the remote stands aside while a phone plays ([7dcc8e5](https://github.com/sonn-audio/sonn-client/commit/7dcc8e5d85452a1c4b1a2081e59c24a18594a3e4))
* **pairing:** pair-remote takes a window, and never restarts bluetoothd ([7327715](https://github.com/sonn-audio/sonn-client/commit/7327715fab9159c5179d5898ed26b0c4c8a87196))
* **source:** announce what the hardware can do, not what a form said ([81ab641](https://github.com/sonn-audio/sonn-client/commit/81ab641140f95b394898f00cdf845c62f288cfda))

## [1.4.2](https://github.com/sonn-audio/sonn-client/compare/v1.4.1...v1.4.2) (2026-08-12)


### Bug Fixes

* **pairing:** start over when someone presses pair ([d640c11](https://github.com/sonn-audio/sonn-client/commit/d640c11fa2e10e4f36d754f6e2dbdf96dd713a66))

## [1.4.1](https://github.com/sonn-audio/sonn-client/compare/v1.4.0...v1.4.1) (2026-08-12)


### Bug Fixes

* **pairing:** actually scan, and wait for each step to answer ([a4ec7f3](https://github.com/sonn-audio/sonn-client/commit/a4ec7f344d531470b545afc229f2fa4b23dec15f))

# [1.4.0](https://github.com/sonn-audio/sonn-client/compare/v1.3.0...v1.4.0) (2026-08-12)


### Features

* **source:** run an input's transport commands again ([0b4c785](https://github.com/sonn-audio/sonn-client/commit/0b4c78531be243e763c9a8ed7fba9dbf5d81e0d1))

# [1.3.0](https://github.com/sonn-audio/sonn-client/compare/v1.2.2...v1.3.0) (2026-08-12)


### Features

* **update:** let a speaker install its own new version ([0b33e31](https://github.com/sonn-audio/sonn-client/commit/0b33e31eddef353823d5ce39817862e84ca37c16))

## [1.2.2](https://github.com/sonn-audio/sonn-client/compare/v1.2.1...v1.2.2) (2026-08-12)


### Bug Fixes

* apply a change to where the volume goes without a restart ([33a5699](https://github.com/sonn-audio/sonn-client/commit/33a5699da4da4a557a3368a4c9e32de557e19f6a))

## [1.2.1](https://github.com/sonn-audio/sonn-client/compare/v1.2.0...v1.2.1) (2026-08-12)


### Bug Fixes

* follow the volume when the speaker's own control moves it ([9e4ac0b](https://github.com/sonn-audio/sonn-client/commit/9e4ac0bb7f9a2bcf7f813a90d14f1fabc3224a2f))

# [1.2.0](https://github.com/sonn-audio/sonn-client/compare/v1.1.0...v1.2.0) (2026-08-12)


### Bug Fixes

* **ci:** stop a release that was never cut from replacing the last one ([8f97d85](https://github.com/sonn-audio/sonn-client/commit/8f97d85c6be285af842a931aecd768240083eb52))

# [1.1.0](https://github.com/sonn-audio/sonn-client/compare/v1.0.8...v1.1.0) (2026-08-01)


### Features

* **volume:** use the sound card's own volume, on the scale it is calibrated in ([8613819](https://github.com/sonn-audio/sonn-client/commit/86138197a1fa1e52f0cd6ffdd5bcaf798cc35d81))

## [1.0.8](https://github.com/sonn-audio/sonn-client/compare/v1.0.7...v1.0.8) (2026-08-01)


### Bug Fixes

* play at the rate the stream is in ([e362e56](https://github.com/sonn-audio/sonn-client/commit/e362e5683d80b39e2528465e17e702d3a0044686))

## [1.0.7](https://github.com/sonn-audio/sonn-client/compare/v1.0.6...v1.0.7) (2026-07-31)


### Bug Fixes

* **devices:** a card being played through is still a card ([e70b81a](https://github.com/sonn-audio/sonn-client/commit/e70b81a35155a1d0fc5ae9df2140bbfac01cf6a3))

## [1.0.6](https://github.com/sonn-audio/sonn-client/compare/v1.0.5...v1.0.6) (2026-07-31)


### Bug Fixes

* **config:** never let a typo change who this device is ([f9fa253](https://github.com/sonn-audio/sonn-client/commit/f9fa253146f1e3c46a07b7c77dd81cf871bd5a07))

## [1.0.5](https://github.com/sonn-audio/sonn-client/compare/v1.0.4...v1.0.5) (2026-07-31)


### Bug Fixes

* **config:** say when a setting in config.toml was not understood ([040de92](https://github.com/sonn-audio/sonn-client/commit/040de928930c6a9042d1f247d74b0196d76d8208))

## [1.0.4](https://github.com/sonn-audio/sonn-client/compare/v1.0.3...v1.0.4) (2026-07-31)


### Bug Fixes

* **devices:** offer a card by its name, never by its number ([6b30418](https://github.com/sonn-audio/sonn-client/commit/6b30418d9f17fc945ad28cf890f553aba0fdfbf5))
* **discovery:** treat a named server as an instruction, and take no for an answer ([ed4b224](https://github.com/sonn-audio/sonn-client/commit/ed4b224fef3080440abb0a65aa35867ca250cdd3))
* stop libasound writing its own configuration problems to the journal ([f1b5d20](https://github.com/sonn-audio/sonn-client/commit/f1b5d20bd95f0b005d2ec0042b2c31b1eec8f16d))

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
