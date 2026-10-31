# Implementation status

Last updated: 2026-10-31.

## Implemented in source

- [x] Rust workspace and isolated protocol crate.
- [x] Minecraft VarInt, UUID, byte-array, integer, float and double primitives.
- [x] AES-128-GCM using Horizon's existing `ring` dependency (random 12-byte IV + authenticated ciphertext).
- [x] Official client/server UDP datagram envelopes and packet framing.
- [x] Authentication, connection-check, keepalive, ping, microphone and player/group/location sound packet codecs.
- [x] TCP `voicechat:request_secret` payload and `voicechat:secret` metadata decoder.
- [x] Official group create/join/leave request codecs and joined/add/remove group response decoders.
- [x] Official player-state, player-state-list and remove-player-state payload parsing, including all upstream state flags and optional group UUID.
- [x] Official add/remove volume-category payload decoding with bounded metadata, optional translation fields and 16x16 icon handling; category metadata is tracked and surfaced in the native HUD.
- [x] Native player selection, local ignore/mute and individual volume adjustment keybinds, backed by the engine's per-speaker mixer controls.
- [x] Native group discovery, group cycling, quick-create, join-public-group and leave-group keybinds, plus native chat commands for listing groups, joining password-protected groups and creating named/passworded NORMAL, OPEN or ISOLATED groups.
- [x] Native Horizon packet hook sends the request on play login and consumes the server's secret response.
- [x] UDP worker with authentication retry, connection validation, ping echo, keepalive response and timeout handling.
- [x] Automatic voice UDP reconnect worker with exponential backoff, session-generation checks and preserved per-player audio controls.
- [x] Vendored pure-Rust Opus encoder/decoder, with tests for 48 kHz 20 ms VOIP frames and all upstream codec modes.
- [x] CPAL microphone capture and speaker playback with local input/output device enumeration, F6/F7 device cycling, safe stream restart and default-device fallback.
- [x] Selected microphone/speaker preferences survive voice UDP reconnects within the running game session.
- [x] Configurable push-to-talk key registered in Horizon Options > Mods (default V).
- [x] Native HUD connection/transmitting indicator.
- [x] Player UUID-to-entity position mapping, location/player voice distance attenuation and stereo panning.
- [x] Per-speaker sequential PCM scheduling with simultaneous mixing so multiple players can talk over one another instead of being played serially.
- [x] Per-player playback volume (0–200%) and mute controls in the native audio engine.
- [x] Outgoing whisper-mode flag control for microphone packets, exposed as a hold-to-whisper keybind.
- [x] Optional RMS voice activation with a native toggle keybind (default C).
- [x] Local microphone mute toggle (default M) that gates both push-to-talk and voice activation, synchronized through the official `voicechat:client_state` TCP payload.
- [x] Microphone amplification setter (0–400%) in the native audio engine.
- [x] Bounded per-speaker Opus jitter queues with sequence reordering, a 40 ms gap window and decoder-driven concealment for up to three missing frames.
- [x] Manual-only Windows/Linux protocol tests, Windows engine compile, Linux native-mixin validation and .hnmod packaging workflow.
- [x] Source-only .hnmod packager with pinned Horizon and horizon-mixins revisions.

## Still missing before a complete Simple Voice Chat client

- [ ] Real Paper/Spigot interoperability tests against the official plugin and unmodified Fabric/Forge/NeoForge clients.
- [ ] Run the manual GitHub Actions workflow on CI runners (Windows engine compile + packaging matrix); the source is now build-validated locally (see below).
- [ ] Adaptive jitter delay and end-to-end packet scheduling validation. Short-loss Opus PLC and bounded sequence reordering are implemented; long loss bursts still skip to the next available frame.
- [ ] Group invites, persistent group settings and a full graphical group UI. Password entry and custom group names/types are available through `/svchorizon group` chat commands; basic discovery/cycling and quick controls remain keybind-driven.
- [ ] Remaining TCP custom payloads for recording-related controls and any newly introduced upstream extensions. Player-state and volume-category payloads are now decoded; the official UDP microphone/player/group/location sound variants and connection packets are implemented.
- [ ] Full player-list screen, persistent per-player preferences, whisper-target selection, and recording controls. Player-state parsing and keybind-driven per-player ignore/volume controls are implemented.
- [ ] Persistent audio settings UI and in-game gain/voice-activation threshold sliders. Microphone/speaker device selection is implemented through F6/F7 keybinds and is retained across reconnects for the current game session.
- [ ] Server-transfer race testing and real reconnect interoperability testing. Basic automatic reconnect with exponential backoff and live audio-device reconfiguration are implemented in source.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## Compatibility pin

The native bundle targets Minecraft 26.3, Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`, and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. The TCP request advertises Simple Voice Chat compatibility version 21 from upstream gradle.properties (2.6.27+ / MC 26.3); the plugin requires an exact match. This value is release-specific, not a universal protocol version.

## Interop-critical wire details (verified against upstream 2.6.27+ source)

- The client sends `minecraft:register` with every `voicechat:*` channel before requesting the secret; Bukkit/Paper deliver payloads only to channels the client registered.
- The `voicechat:secret` payload carries a connection UUID between the AES secret and the UDP port; every client UDP datagram prefixes that connection UUID (not the player UUID), mirroring `ClientNetworkMessage.writeClient`.
- `AuthenticatePacket` (0x05) has an empty body: the encrypted plaintext is the single packet-ID byte.
- The command registration hook (`native_commands`) does not exist at the pinned Horizon commit (hooks API 3), so `/svchorizon` runs through the `native_command` hook alone; chat feedback uses `native_chat`.

The vendored Opus implementation is from [restsend/opus-rs](https://github.com/restsend/opus-rs) and retains its BSD license in the engine bundle.

## Local build validation (2026-10-31, branch dev/continue-port)

Validated against Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c` with horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e` (Rust 1.99.1, Linux x86-64):

- `horizon-mixins plan` and `apply` pass on the pinned source (6 injections, 2 modules, 1 bundle crate).
- The modded `horizon-client` compiles; `cargo test -p horizon-client` runs the game's own suites green (733 lib + 215 bin tests, 0 failures), including this mod's 9 runtime tests.
- `svchorizon-engine` compiles inside the staged workspace and passes its suites (81 lib + 2 roundtrip tests standalone; 18 + 8 + 4 + 2 + 1 via the staged workspace run).
- `cargo clippy --workspace --all-targets -- -D warnings` is clean on the protocol workspace.
- `tools/package.py` produces `target/svchorizon-0.1.0.hnmod`; every bundle path passes the launcher's `mixins/<id>/` `.rs`/`.toml` file rule (the vendored BSD license text stays outside the bundle, in `engine/COPYING`).

Protocol fixes found by the audit against upstream `henkelmax/simple-voice-chat` (2.6.27+ / MC 26.3): compatibility version 20 -> 21; the secret payload's connection UUID is now decoded and used as the datagram prefix (was the player UUID); `AuthenticatePacket` is sent with an empty body; the client registers all official `voicechat:*` channels via `minecraft:register` before requesting the secret (Bukkit/Paper deliver payloads only to registered channels); create-group passwords are limited to the upstream 24-character name limit.

## Validation policy

Protocol unit tests and source-level integration do not prove server interoperability. No usable-client or interoperability claim is made until the resulting bundle is tested against a real Paper server running the official plugin.
