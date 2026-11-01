# Implementation status

Last updated: 2026-11-01.

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

- [x] Real Paper interoperability test against the official plugin: the protocol crate now completes the full official handshake/auth/keepalive/microphone flow against a live Paper 26.3 server running unmodified `voicechat-bukkit-2.6.27` (see "Live interop validation").
- [ ] Two-client audio relay test (a second client actually hearing the first) and cross-talk with official Fabric/Forge/NeoForge clients on the same server.
- [ ] Run the manual GitHub Actions workflow on CI runners (Windows engine compile + packaging matrix); the source is now build-validated locally (see below).
- [ ] Adaptive jitter delay and end-to-end packet scheduling validation. Short-loss Opus PLC and bounded sequence reordering are implemented; long loss bursts still skip to the next available frame.
- [ ] Group invites, persistent group settings and a full graphical group UI. Password entry and custom group names/types are available through `/svchorizon group` chat commands; basic discovery/cycling and quick controls remain keybind-driven.
- [ ] Remaining TCP custom payloads for recording-related controls and any newly introduced upstream extensions. Player-state and volume-category payloads are now decoded; the official UDP microphone/player/group/location sound variants and connection packets are implemented.
- [ ] Full player-list screen, persistent per-player preferences, whisper-target selection, and recording controls. Player-state parsing and keybind-driven per-player ignore/volume controls are implemented.
- [ ] Persistent audio settings UI and in-game gain/voice-activation threshold sliders. Microphone/speaker device selection is implemented through F6/F7 keybinds and is retained across reconnects for the current game session.
- [ ] Server-transfer race testing and real reconnect interoperability testing. Basic automatic reconnect with exponential backoff and live audio-device reconfiguration are implemented in source.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## Compatibility pin

The native bundle targets Minecraft 26.3, Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`, and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. The TCP request advertises Simple Voice Chat compatibility version **20** — the number the shipped bukkit plugin for MC 26.3, `voicechat-bukkit-2.6.27`, reports and requires (upstream's unreleased 2.7.0-pre1 branch moved to 21 together with a wire change; both must be bumped together when that ships). This value is release-specific, not a universal protocol version.

## Interop-critical wire details (verified against the 2.6.27 plugin jar and a live server)

- The client sends `minecraft:register` with every `voicechat:*` channel before requesting the secret; Bukkit/Paper deliver payloads only to channels the client registered.
- The `voicechat:secret` payload (compatibility 20) has **no** connection UUID: secret, port, player UUID, codec, mtu, distance, keepalive, groups, host, recording. Upstream 2.7.0-pre1 inserts a server-assigned connection UUID after the secret.
- Every client UDP datagram prefixes the **player's own profile UUID**; the plugin keeps its per-player AES secrets in a map keyed by `player.getUniqueId()` and resolves the key with that UUID (verified by decompiling `NetworkMessage.readPacketServer` and `ServerVoiceEvents.initializePlayerConnection` in the shipped jar).
- `AuthenticatePacket` (0x05) carries the player UUID followed by the raw 16-byte secret; `AuthenticateAckPacket`, `ConnectionCheckPacket` and `ConnectionCheckAckPacket` are empty in 2.6.27 (the nonce-carrying variants exist only in upstream 2.7.0-pre1).
- AES-128-GCM with a random 12-byte IV prefix and 128-bit tag, exactly as implemented (`Secret.IV_SIZE_BYTES = 12` in the jar).
- The command registration hook (`native_commands`) does not exist at the pinned Horizon commit (hooks API 3), so `/svchorizon` runs through the `native_command` hook alone; chat feedback uses `native_chat`.

The vendored Opus implementation is from [restsend/opus-rs](https://github.com/restsend/opus-rs) and retains its BSD license in the engine bundle.

## Local build validation (2026-10-31, branch dev/continue-port)

Validated against Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c` with horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e` (Rust 1.99.1, Linux x86-64):

- `horizon-mixins plan` and `apply` pass on the pinned source (6 injections, 2 modules, 1 bundle crate).
- The modded `horizon-client` compiles; `cargo test -p horizon-client` runs the game's own suites green (733 lib + 215 bin tests, 0 failures), including this mod's 9 runtime tests.
- `svchorizon-engine` compiles inside the staged workspace and passes its suites (81 lib + 2 roundtrip tests standalone; 18 + 8 + 4 + 2 + 1 via the staged workspace run).
- `cargo clippy --workspace --all-targets -- -D warnings` is clean on the protocol workspace.
- `tools/package.py` produces `target/svchorizon-0.1.0.hnmod`; every bundle path passes the launcher's `mixins/<id>/` `.rs`/`.toml` file rule (the vendored BSD license text stays outside the bundle, in `engine/COPYING`).

Wire corrections (2026-11-01) proven by the live interop run below — the previous audit had derived the wire from upstream's unreleased 2.7.0-pre1 branch instead of the shipped 2.6.27 plugin: compatibility version 21 -> 20; the secret payload's connection UUID field removed (not present in 2.6.27); the UDP datagram prefix is the player UUID (was mis-set to the secret UUID, which made the server reject every datagram with "Player <uuid> does not have a secret"); `AuthenticatePacket` now sends the player UUID + raw secret (was empty).

## Live interop validation (2026-11-01, branch dev/continue-port)

`tools/interop-harness` (workspace member) joins a real Paper 26.3 build 235
server running the unmodified official `voicechat-bukkit-2.6.27` plugin on
127.0.0.1, using a hand-rolled protocol-777 Minecraft login (packet IDs from
the pinned Horizon id table) plus `svchorizon-protocol` for the entire SVC
wire. Reproduce with the repository-root helper `scripts/run_interop.sh`
(boots the server headless with `-Dvoicechat.debug=true`, waits for readiness,
runs the harness, prints the plugin's log lines).

Observed from the plugin, unedited:

- `Received secret request of svchorizon (20)` → `Sent secret to svchorizon`
- `Successfully authenticated player 159fe646-f0d3-3e45-85de-310fb9410b28`
- `Successfully validated connection of player 159fe646-f0d3-3e45-85de-310fb9410b28`
- `Player svchorizon (159fe646-f0d3-3e45-85de-310fb9410b28) successfully connected to voice chat`

The harness then answers server keepalives, echoes pings and streams
microphone packets for five seconds and exits 0. This validates the TCP
handshake, secret decoding, AES-GCM transport, UDP authentication, connection
validation and keepalive path against the real plugin — it does not yet prove
two-client audio relay (needs a second voice client on the server).

## Validation policy

Protocol unit tests alone do not prove server interoperability. The full SVC
wire path (TCP handshake, secret, UDP authentication, connection validation,
keepalive) is now proven against a real Paper server running the unmodified
official plugin — see "Live interop validation". What remains unproven is the
packaged mod running inside the actual Horizon client: the two-client audio
relay, official Fabric/Forge/NeoForge cross-talk and the in-game scenario
matrix still have to pass before the mod can be called done.
