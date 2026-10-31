# Implementation status

Last updated: 2026-10-31.

## Implemented in source

- [x] Rust workspace and isolated protocol crate.
- [x] Minecraft VarInt, UUID, byte-array, integer, float and double primitives.
- [x] AES-128-GCM using Horizon's existing `ring` dependency (random 12-byte IV + authenticated ciphertext).
- [x] Official client/server UDP datagram envelopes and packet framing.
- [x] Authentication, connection-check, keepalive, ping, microphone and player/group/location sound packet codecs.
- [x] TCP `voicechat:request_secret` payload and `voicechat:secret` metadata decoder.
- [x] Native Horizon packet hook sends the request on play login and consumes the server's secret response.
- [x] UDP worker with authentication retry, connection validation, ping echo, keepalive response and timeout handling.
- [x] Vendored pure-Rust Opus encoder/decoder, with tests for 48 kHz 20 ms VOIP frames and all upstream codec modes.
- [x] CPAL default microphone capture and speaker playback; mono resampling to/from 48 kHz.
- [x] Configurable push-to-talk key registered in Horizon Options > Mods (default V).
- [x] Native HUD connection/transmitting indicator.
- [x] Player UUID-to-entity position mapping, location/player voice distance attenuation and stereo panning.
- [x] Manual-only Windows/Linux protocol tests, Windows engine compile, Linux native-mixin validation and .hnmod packaging workflow.
- [x] Source-only .hnmod packager with pinned Horizon and horizon-mixins revisions.

## Still missing before a complete Simple Voice Chat client

- [ ] Real Paper/Spigot interoperability tests against the official plugin and unmodified Fabric/Forge/NeoForge clients.
- [ ] Run and pass the manual GitHub Actions workflow; the source has not yet been build-validated.
- [ ] Jitter buffer, sequence reordering/loss concealment and robust packet scheduling.
- [ ] Group creation, join/leave, invites, group settings and full group UI. Group sound reception is implemented, but group management is not.
- [ ] Static/entity sound packet variants and the rest of the official packet catalogue.
- [ ] Per-player volume, mute/ignore, whisper controls and recording controls.
- [ ] Voice activation, microphone/speaker device selection and audio settings UI.
- [ ] Disconnect/reconfigure edge cases, reconnect backoff and server transfer testing.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## Compatibility pin

The native bundle targets Minecraft 26.3, Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`, and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. The TCP request advertises Simple Voice Chat compatibility version 20 from upstream 2.6.28+26.3. This value is release-specific, not a universal protocol version.

The vendored Opus implementation is from [restsend/opus-rs](https://github.com/restsend/opus-rs) and retains its BSD license in the engine bundle.

## Validation policy

Protocol unit tests and source-level integration do not prove server interoperability. No usable-client or interoperability claim is made until the manual workflow passes and the resulting bundle is tested against a real Paper server running the official plugin.
