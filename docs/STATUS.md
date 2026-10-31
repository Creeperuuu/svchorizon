# Implementation status

Last updated: 2026-10-31.

## Implemented in source

- [x] Rust workspace and standalone protocol crate.
- [x] Minecraft VarInt, UUID, byte-array, integer, float and double primitives.
- [x] AES-128-GCM encryption envelope (random 12-byte IV + ciphertext + 16-byte tag).
- [x] Client and server UDP datagram envelopes.
- [x] Initial authentication, connection-check, keepalive, ping, microphone and all three sound packet serializers.
- [x] Exact current upstream maximum UDP packet (2048 bytes) and Opus frame payload (1275 bytes).
- [x] TCP request-secret payload encoding and SecretPacket decoder.
- [x] Protocol unit tests for serialization, crypto, packet variants and handshake metadata.
- [x] Pinned Horizon source-mixin manifest targeting the real play packet handler.
- [x] Initial native hook sends voicechat:request_secret on play login and captures voicechat:secret.
- [x] Manual-only Windows/Linux protocol checks and Linux native-mixin validation/package workflow.
- [x] Source-only hnmod packager.

## Still not implemented — required before usable voice chat

- [ ] UDP socket worker, DNS/voice-host resolution, authentication retry state machine, connection-check, ping/pong and keepalive timeout.
- [ ] Opus encoder/decoder and codec enum negotiation.
- [ ] CPAL microphone/speaker I/O, device selection, resampling, jitter buffer and device-loss handling.
- [ ] Transmit microphone frames as MicPacket; route received sound packets into spatial audio.
- [ ] Push-to-talk, voice activation, whispering, mute, per-player volume and group controls.
- [ ] Disconnect/reconfigure lifecycle cleanup and server transfer handling.
- [ ] Native settings UI, keybinds and speaking HUD.
- [ ] Real Paper/Spigot + official Simple Voice Chat interoperability tests in both directions.

## Compatibility pin

The current native bundle targets Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c` and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. Its TCP request currently advertises Simple Voice Chat compatibility version 20, taken from upstream 2.6.28+26.3. That value must be updated with the upstream release targeted by the build; it is not a universal protocol version.

## Validation policy

A successful source build or unit test is not proof of server interoperability. No usable-client or interoperability claim is made until a real Paper server running the official plugin is tested against this client and an unmodified official client.
