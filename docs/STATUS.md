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
- [x] Native group discovery, group cycling, quick-create, join-public-group and leave-group keybinds; password-protected groups are identified but cannot yet be joined until password entry is implemented.
- [x] Native Horizon packet hook sends the request on play login and consumes the server's secret response.
- [x] UDP worker with authentication retry, connection validation, ping echo, keepalive response and timeout handling.
- [x] Automatic voice UDP reconnect worker with exponential backoff, session-generation checks and preserved per-player audio controls.
- [x] Vendored pure-Rust Opus encoder/decoder, with tests for 48 kHz 20 ms VOIP frames and all upstream codec modes.
- [x] CPAL default microphone capture and speaker playback; mono resampling to/from 48 kHz.
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
- [ ] Run and pass the manual GitHub Actions workflow; the source has not yet been build-validated.
- [ ] Adaptive jitter delay and end-to-end packet scheduling validation. Short-loss Opus PLC and bounded sequence reordering are implemented; long loss bursts still skip to the next available frame.
- [ ] Password entry, custom group names/types, invites, group settings and full group UI. Basic group discovery, cycling, quick-create, public join and leave controls are now wired to the native runtime.
- [ ] Remaining TCP custom payloads for recording-related controls and any newly introduced upstream extensions. Player-state and volume-category payloads are now decoded; the official UDP microphone/player/group/location sound variants and connection packets are implemented.
- [ ] Full player-list screen, persistent per-player preferences, whisper-target selection, and recording controls. Player-state parsing and keybind-driven per-player ignore/volume controls are implemented.
- [ ] Microphone/speaker device selection, persistent audio settings UI and in-game gain/voice-activation threshold sliders. Engine setters for microphone gain and RMS threshold are implemented.
- [ ] Server-transfer race testing, audio-device reconfiguration and real reconnect interoperability testing. Basic automatic reconnect with exponential backoff is implemented.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## Compatibility pin

The native bundle targets Minecraft 26.3, Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`, and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. The TCP request advertises Simple Voice Chat compatibility version 20 from upstream 2.6.27+26.3. This value is release-specific, not a universal protocol version.

The vendored Opus implementation is from [restsend/opus-rs](https://github.com/restsend/opus-rs) and retains its BSD license in the engine bundle.

## Validation policy

Protocol unit tests and source-level integration do not prove server interoperability. No usable-client or interoperability claim is made until the manual workflow passes and the resulting bundle is tested against a real Paper server running the official plugin.
