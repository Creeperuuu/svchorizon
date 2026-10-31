# Implementation status

Last updated: 2026-10-31.

## Implemented

- [x] Rust workspace and isolated protocol crate.
- [x] Minecraft VarInt, UUID, byte-array, integer and float primitives.
- [x] AES-128-GCM envelope (12-byte random IV + ciphertext + 16-byte tag).
- [x] Client UDP envelope (magic + player UUID + encrypted length-prefixed payload).
- [x] Server UDP envelope (magic + encrypted length-prefixed payload).
- [x] Initial auth, connection-check, keepalive, microphone and player-sound packet serialization.
- [x] Unit tests for wire primitives, crypto and packet framing.

## Not implemented — required before usable hnmod

- [ ] Exact compatibility-version negotiation and TCP custom-payload interception.
- [ ] Secret packet decoding and voice-host/port resolution.
- [ ] Horizon mixin integration for sending/receiving Minecraft custom payloads.
- [ ] Server/world lifecycle hooks and reconnect handling.
- [ ] UDP worker, authentication retry state machine, ping/pong, keepalive and timeout handling.
- [ ] GroupSoundPacket, LocationSoundPacket and remaining packet variants.
- [ ] Opus encoder/decoder and codec negotiation.
- [ ] CPAL microphone/speaker I/O, resampling, jitter buffering and device-loss handling.
- [ ] Push-to-talk, voice activation, mute, per-player volume and groups.
- [ ] Native settings, keybinds and speaking HUD.
- [ ] hnmod packaging and Windows x64 build.
- [ ] Paper interoperability tests in both directions.

No compatibility claim is made until tested against a real Paper server with the official plugin and an unmodified official client.
