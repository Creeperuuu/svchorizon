# Implementation status

Last updated: 2026-11-01.

## Implemented in source

- [x] Rust workspace and isolated protocol crate.
- [x] Minecraft VarInt, UUID, byte-array, integer, float and double primitives.
- [x] AES-128-GCM using Horizon's existing `ring` dependency (random 12-byte IV + authenticated ciphertext).
- [x] Official client/server UDP datagram envelopes and packet framing.
- [x] Cross-generation protocol support: the client detects and speaks all three deployed Simple Voice Chat wire generations - compatibility 20 (SVC 2.6.x), compatibility 21 (SVC 2.7.x, connection-UUID envelopes with nonce-challenge validation) and compatibility 18 (SVC 2.5.x, wire-identical to 20) - so older and newer servers work without any single-version workaround.
- [x] Authentication, connection-check, keepalive, ping, microphone and player/group/location sound packet codecs.
- [x] TCP `voicechat:request_secret` payload and `voicechat:secret` metadata decoder.
- [x] Official group create/join/leave request codecs and joined/add/remove group response decoders.
- [x] Official player-state, player-state-list and remove-player-state payload parsing, including all upstream state flags and optional group UUID.
- [x] Official add/remove volume-category payload decoding with bounded metadata, optional translation fields and 16x16 icon handling; category metadata is tracked and surfaced in the native HUD.
- [x] Native player selection, local ignore/mute and individual volume adjustment keybinds, backed by the engine's per-speaker mixer controls.
- [x] Native group discovery, group cycling, quick-create, join-public-group and leave-group keybinds, plus native chat commands for listing groups, joining password-protected groups and creating named/passworded NORMAL, OPEN or ISOLATED groups.
- [x] Native Horizon packet hook sends the request on play login and consumes the server's secret response.
- [x] UDP worker with authentication retry, connection validation, ping echo, keepalive response and timeout handling.
- [x] Voice endpoint candidate rotation: every address the voice host resolves to (dual-stack A/AAAA or multi-record) is tried in turn while authentication goes unanswered, on a connectionless socket shared by both families.
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
- [x] Cloud Windows game build: the `Build game (Windows)` workflow produces a runnable `horizon-client.exe` artifact (first confirmed working run on 2026-11-01, branch `ci/windows-game-build`).

## Still missing before a complete Simple Voice Chat client

- [x] Real Paper interoperability test against the official plugin: the protocol crate now completes the full official handshake/auth/keepalive/microphone flow against a live Paper 26.3 server running unmodified `voicechat-bukkit-2.6.27` (see "Live interop validation").
- [x] The vendored engine's `VoiceClient` (the code the game runs) passes the same live-server handshake, validation and keepalive flow, including the empty-voice_host fallback (see "Live engine validation").
- [ ] Two-client audio relay test (a second client actually hearing the first) and cross-talk with official Fabric/Forge/NeoForge clients — on a server whose voice UDP port is confirmed open (see "First live-server runtime test").
- [x] Run the manual GitHub Actions workflow on CI runners: the `Build game (Windows)` workflow produced a working exe (2026-11-01); the Linux native-mixin check runs on every push, the full matrix and packaging stay manual to save Windows runner minutes.
- [ ] Adaptive jitter delay and end-to-end packet scheduling validation. Short-loss Opus PLC and bounded sequence reordering are implemented; long loss bursts still skip to the next available frame.
- [ ] Group invites, persistent group settings and a full graphical group UI. Password entry and custom group names/types are available through `/svchorizon group` chat commands; basic discovery/cycling and quick controls remain keybind-driven.
- [ ] Remaining TCP custom payloads for recording-related controls and any newly introduced upstream extensions. Player-state and volume-category payloads are now decoded; the official UDP microphone/player/group/location sound variants and connection packets are implemented.
- [ ] Full player-list screen, persistent per-player preferences, whisper-target selection, and recording controls. Player-state parsing and keybind-driven per-player ignore/volume controls are implemented.
- [ ] Persistent audio settings UI and in-game gain/voice-activation threshold sliders. Microphone/speaker device selection is implemented through F6/F7 keybinds and is retained across reconnects for the current game session.
- [ ] Server-transfer race testing and real reconnect interoperability testing. Basic automatic reconnect with exponential backoff and live audio-device reconfiguration are implemented in source.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## Compatibility pin

The native bundle targets Minecraft 26.3, Horizon commit `ba6344a59cb9e755b730dcf02285fc0d33b97d3c`, and horizon-mixins `d9c83d7096b7ea5b83678da2efdfd6161f74997e`. The client speaks Simple Voice Chat compatibility versions **20**, **21** and **18**: on every join the generations are probed one at a time (the server answers only the request whose number matches its own plugin build and sends the player an incompatibility chat notice for every other number; the client detects that notice and skips to the next generation), and the UDP layer switches to the matching wire format. Compat 20 is the release line published for Minecraft 1.12.2 through the current 1.21.x/26.x servers (`voicechat-bukkit-2.6.27` and friends); compat 21 is upstream 2.7.x (2.7.0-pre1); compat 18 is the 2.5.x line on servers that never updated, wire-identical to compat 20. This is a property of the server's plugin build, not of one Minecraft version, so probing all three covers older and newer servers alike.

## Interop-critical wire details (verified against the 2.6.27 plugin jar and a live server)

- The client sends `minecraft:register` with every `voicechat:*` channel before requesting the secret; Bukkit/Paper deliver payloads only to channels the client registered.
- The `voicechat:secret` payload (compatibility 20) has **no** connection UUID: secret, port, player UUID, codec, mtu, distance, keepalive, groups, host, recording. Compat 21 (SVC 2.7.x) inserts a server-assigned connection UUID after the secret; the client decodes both layouts strictly and picks the one that parses.
- Every client UDP datagram prefixes the **player's own profile UUID** on compat-20 servers; the plugin keeps its per-player AES secrets in a map keyed by `player.getUniqueId()` and resolves the key with that UUID (verified by decompiling `NetworkMessage.readPacketServer` and `ServerVoiceEvents.initializePlayerConnection` in the shipped jar). On compat-21 servers the envelope carries the server-assigned **connection UUID** instead.
- `AuthenticatePacket` (0x05) carries the player UUID followed by the raw 16-byte secret on compat-20 servers and is empty on compat-21 servers; `AuthenticateAckPacket` (0x06) and `ConnectionCheckPacket` (0x09) are empty on compat-20 servers, while compat-21 servers hand out a 16-byte HMAC nonce in the ack that the client must echo in the connection check from the same source address (upstream `Server.createNonce` binds it to connectionId + client socket address).
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

## Live engine validation (2026-11-01, branch ci/windows-game-build)

`tools/engine-harness` (standalone, excluded from the workspace like the
engine itself) repeats the same live-server flow but drives the session with
`svchorizon_engine::voice::VoiceClient` — the exact code the compiled game
runs — including the empty-`voice_host` fallback to the game server address.
Reproduce: boot the server as above, then `cargo run --release
--manifest-path tools/engine-harness/Cargo.toml`.

Observed, unedited: the engine reports `VoiceEvent::Connected` and the plugin
logs the same authentication/validation lines as above, then answers
keepalives for six seconds with no disconnect (exit 0). The sandbox has no
sound devices, so the engine's expected `AudioUnavailable` events for the
default microphone and speaker appear; every other path is the one the game
runs. The mod's in-game diagnosis paths were extended the same day: the game
log now shows the resolved voice endpoint, unanswered-authentication warnings,
microphone/speaker device names, and the HUD distinguishes "Not on a server",
"No voice chat on this server", "Connecting" and "Reconnecting"; a failed
initial connect now retries through the reconnect path instead of stalling.

## First live-server runtime test (2026-11-01, branch ci/windows-game-build)

The Actions-built Windows exe was tested against a real third-party server
(`axocraftseason8.playwithbao.com`, Paper 26.3 + official SVC plugin). The TCP
side is fully healthy: channel registration, secret request, and the secret
payload (strict compat-20 decode, groups enabled, voice port 47230, keepalive
1000 ms, empty voice_host) all work; group discovery works. The UDP side gets
no answer: the client sends a valid `AuthenticatePacket` every second and the
server never replies — 45 unanswered attempts, zero socket errors.

Evidence gathered the same day:

- DNS: the A record and the `_minecraft._tcp` SRV record both resolve to
  `172.197.205.171` (game on SRV port 47157), so the empty-voice_host fallback
  computes the same IP an official client would use.
- Official server source (henkelmax/simple-voice-chat branch `26.3`,
  `Server.java` ProcessThread): only `AuthenticatePacket` and
  `ConnectionCheckPacket` are answered before authentication; pings from
  unauthenticated sources are ignored. So external UDP probing cannot
  distinguish a blocked port from a healthy one — but a missing
  AuthenticateAck for a *valid* auth is decisive, and the engine harness had
  already proven that exact packet against the real 2.6.27 plugin.
- Sandbox probes: TCP 47157 open; UDP 47230 and a control closed port are
  both silently dropped (ICMP suppressed by the provider), so no vantage point
  could see the plugin at all.
- Upstream troubleshooting documents exactly this failure mode for hosted
  servers: the voice UDP port is not opened/forwarded by the hosting panel
  (most Pterodactyl-style hosts need a second UDP allocation for voice chat).

Conclusion (updated after the user's differential test, same day): the
official voice chat client works on this server from the same PC, which rules
out server-side port blocking and user-side network filtering. Endpoint,
DNS, wire generation and packet bytes are all proven identical to what the
official client sends (single stable A record = SRV target; compat-20 secret
decodes strictly; a compat-21 server would have refused to send a secret at
all). The remaining suspects are local per-application filtering of
horizon-client.exe's UDP replies (Windows Firewall block rule created by a
dismissed first-run prompt, or antivirus network protection — established
outbound TCP keeps working while UDP reply flows are dropped), or a
process-level UDP receive restriction. Client-side changes shipped with this
diagnosis: a localhost UDP self-test at connect (proves the game process can
receive UDP at all), the unanswered-attempt count surfaced on the HUD
(a red `SVC: Voice server not responding` after 15 s, taking
precedence over the misleading "Transmitting" label), send failures logged
with Windows WSAECONNRESET/ICMP noise filtered, a stray ConnectionReset no
longer tears down the UDP worker, and a one-time log line at 15 s naming the
firewall/AV check and the hosting-panel port-forwarding fallback.

## Cross-generation and endpoint-rotation hardening (2026-11-01, branch ci/windows-game-build)

After the first third-party runtime test, the client was extended along two
independent axes, both verified against upstream source (the `2.6.x/*`
branches and the 2.7.0-pre1 release branches of henkelmax/simple-voice-chat):

- **All three wire generations, probed serially.** The plugin-channel secret
  request asks about one compatibility number at a time - 20, then 21, then
  18 (only the number the server accepts gets a secret; every other number
  gets the player `message.voicechat.incompatible_version` as a system chat
  notice, which the client detects and treats as a definitive "not this
  generation"). The secret payload decodes under all layouts, and the UDP
  layer switches per generation: envelope UUID (player vs connection),
  Authenticate payload (player+secret vs empty) and the 2.7.x nonce
  challenge (AuthenticateAck carries a 16-byte nonce echoed in
  ConnectionCheck). The round repeats while no generation has answered so a
  dropped first request recovers without a relog, and rejected generations
  are never probed again this login (at most one notice per server).
  Serial probing is not a style choice: upstream stores the compatibility
  number of the LAST request per player and silently drops all non-secret
  payloads from a player whose stored number does not match, so a stale
  second probe would break state/group sync even after a successful
  handshake. No part of this is keyed to one Minecraft version: it follows
  the server's plugin build.
- **Endpoint rotation.** All addresses the voice host resolves to (dual
  stack or multi-record) become candidates; the worker starts with the
  first (the address an official client would use) and rotates every 5
  unanswered seconds, logging each switch. The socket is now
  connectionless with a dual-stack bind, which also removes the Windows
  WSAECONNRESET class of failures for connected sockets.

The HUD's 15-second message now says `SVC: Voice server not responding`
(the previous "UDP blocked?" wording blamed either side before any evidence).
During generation detection the HUD shows `SVC: Detecting voice chat`, a
server that rejects every supported generation shows
`SVC: Server voice chat version incompatible`, and a server that stays
silent for 15 s falls back to `SVC: No voice chat on this server` while
probing continues in the background.
The build workflow gained a `profile` input (`fast` = Horizon's
`[profile.fast]`, no LTO / 16 codegen units / incremental, the new default;
`release` = the shippable thin-LTO build), split cache-restore/save steps
that persist the cache even when a run fails, a cached `horizon-mixins/target`,
and the Linux native-mixin check now runs on every push.

## Validation policy

Protocol unit tests alone do not prove server interoperability. The full SVC
wire path (TCP handshake, secret, UDP authentication, connection validation,
keepalive) is now proven against a real Paper server running the unmodified
official plugin — see "Live interop validation". What remains unproven is the
packaged mod running inside the actual Horizon client: the two-client audio
relay, official Fabric/Forge/NeoForge cross-talk and the in-game scenario
matrix still have to pass before the mod can be called done.
