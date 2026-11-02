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
- [x] Cloud Windows game build: the `Build game (Windows)` workflow produces a runnable `horizon-client.exe` artifact (first confirmed working run on 2026-11-01, branch `ci/windows-game-build`).

## Still missing before a complete Simple Voice Chat client

- [x] Real Paper interoperability test against the official plugin: the protocol crate now completes the full official handshake/auth/keepalive/microphone flow against a live Paper 26.3 server running unmodified `voicechat-bukkit-2.6.27` (see "Live interop validation").
- [x] The vendored engine's `VoiceClient` (the code the game runs) passes the same live-server handshake, validation and keepalive flow, including the empty-voice_host fallback (see "Live engine validation").
- [ ] Two-client audio relay test (a second client actually hearing the first) and cross-talk with official Fabric/Forge/NeoForge clients — on a server whose voice UDP port is confirmed open (see "First live-server runtime test").
- [x] Run the manual GitHub Actions workflow on CI runners: the `Build game (Windows)` workflow produced a working exe (2026-11-01); the Linux native-mixin and packaging jobs run on every push.
- [ ] Adaptive jitter delay and end-to-end packet scheduling validation. Short-loss Opus PLC and bounded sequence reordering are implemented; long loss bursts still skip to the next available frame.
- [ ] Group invites, persistent group settings and a full graphical group UI. Password entry and custom group names/types are available through `/svchorizon group` chat commands; basic discovery/cycling and quick controls remain keybind-driven.
- [ ] Remaining TCP custom payloads for recording-related controls and any newly introduced upstream extensions. Player-state and volume-category payloads are now decoded; the official UDP microphone/player/group/location sound variants and connection packets are implemented.
- [ ] Full player-list screen, persistent per-player preferences, whisper-target selection, and recording controls. Player-state parsing and keybind-driven per-player ignore/volume controls are implemented.
- [ ] Persistent audio settings UI and in-game gain/voice-activation threshold sliders. Microphone/speaker device selection is implemented through F6/F7 keybinds and is retained across reconnects for the current game session.
- [ ] Server-transfer race testing and real reconnect interoperability testing. Basic automatic reconnect with exponential backoff and live audio-device reconfiguration are implemented in source.
- [ ] Better spatial audio interpolation and cross-dimension handling.

## CI fix: `VoiceError` has no `kind()` (2026-11-01, branch fix/voice-error-kind)

The `Build game (Windows)` job failed to compile `svchorizon-engine` with three
`E0599: no method named kind found for enum VoiceError` errors in `voice.rs`.
The diagnostics added in `f4057e0` call `error.kind()` on the result of
`send_packet()`, which returns `VoiceError`, not `std::io::Error`. `VoiceError`
now exposes `io_kind()` and `is_connection_reset()`; the authentication,
connection-check and keepalive send paths use the latter to keep ignoring the
transient Windows `WSAECONNRESET` (caused by an ICMP port-unreachable on a
connected UDP socket) while still logging every other send failure. A
regression unit test covers the helper.

Wire format re-audited against upstream `henkelmax/simple-voice-chat` (branch
`26.3`, last commit before the 2026-10-30 compat-21 changes, i.e. the 2.6.x
wire): framing, `MicPacket`, `PlayerSoundPacket`, `GroupSoundPacket`,
`LocationSoundPacket`, `PingPacket`, `AuthenticatePacket`, the 1275-byte Opus and
2048-byte datagram limits, and the 12-byte-IV AES-GCM envelope all match the
engine's codec.

## Voice endpoint selection and server versions (2026-11-01)

Symptom on a 1.21.11 server: the HUD reached "Voice server unreachable (UDP
blocked?)" after "Connecting". That state is only reachable after the secret
packet was received and decoded, so the Minecraft version is not the cause.

Version findings (upstream `henkelmax/simple-voice-chat`):
- The wire is selected by the Simple Voice Chat *compatibility version*, not the
  Minecraft version. Every 2.6.x branch (1.19.2 through 26.1, 1.21.11 and 26.3,
  all 2.6.28) is compat 20, and the wire files are byte-identical between the
  1.21.11 and 26.3 builds. This mod speaks compat 20 and therefore works with
  any 2.6.x plugin or mod regardless of the server's Minecraft version.
- Only the 2.7.0-pre1 line is compat 21 (new handshake). Not supported yet.
- A server that sees a different compat version sends no secret, only a chat
  message, so a client cannot learn the server's version from the handshake.

Two real differences from the official client were fixed:
1. UDP endpoint address family: the first DNS result was used, which on a
   dual-stack host can be an AAAA address the (IPv4-only) voice server never
   answers. IPv4 is now preferred, like Java and like Horizon's own TCP code.
2. Empty `voice_host`: the typed server name was used. The official client uses
   the remote address of the connection it made (after SRV redirects), which can
   be a different machine. Two small support modules
   (`svchorizon_peer` in horizon-protocol `connection.rs`, `svchorizon_net` in
   horizon-client `net.rs`) expose the real peer address; it is used first and the
   typed name is only a fallback.

The HUD now shows `-> ip:port` next to the unreachable message and the log prints
every resolved address, the typed address and the connected peer.

Validated here: engine tests (84), `horizon-mixins apply` on the pinned Horizon,
and `cargo check` of the staged `horizon-client` (whole game plus mod) with
Rust 1.95.0. Not validated: a live server. If it is still unreachable the voice
UDP port is almost certainly not open or forwarded on the host.

## axocraftseason8 live result: endpoint correct, no answer (2026-11-01)

Log from a real run: voice host empty, connected peer 172.197.205.171:47157,
voice endpoint 172.197.205.171:47230 (the port the server announced), 20
authentication attempts with no reply. So the endpoint selection is right (the
typed name and the SRV/peer address resolve to the same IP here) and the secret
exchange works; the voice UDP datagrams get no answer.

The loopback "UDP self-test" only proves the game can receive UDP locally; it
does not exercise the firewall or the network, and its message now says so.

The engine now tells the two failure modes apart by counting ICMP
port-unreachable replies (`ConnectionReset` on Windows, `ConnectionRefused`
elsewhere), instead of swallowing them:
- rejections > 0: the host is reachable but nothing listens on that UDP port
  (voice server not bound there, or the panel/NAT does not map the port). HUD:
  "Voice port closed on server (nothing listening)".
- rejections == 0: datagrams are silently dropped (host/cloud firewall not
  allowing inbound UDP on that port, or local filtering). HUD: "Voice server
  unreachable (UDP dropped)".

## Port open and official voice works, yet no answer (2026-11-01)

The wire was re-audited against the 2.6.28 server for both the common and the
Bukkit/Paper builds: magic byte, raw player UUID, VarInt-prefixed
`IV(12) || AES-128-GCM(type || body) || tag(16)`, no AAD, and the SecretPacket
field order all match the engine. The server silently drops (debug log only) any
datagram with a bad magic byte, an unknown player UUID or a failed decrypt, so a
mismatch there is indistinguishable from a dead network from the client side.

Server behaviour that matters (Server.java, AuthenticatePacket branch): the
player's connection is bound to the first UDP source address that authenticates,
and every later AuthenticateAck is sent to that stored address, not to the
sender. `generateNewSecret` ignores a second secret request while a secret
exists. A player therefore gets exactly one UDP socket per login session; a
second socket for the same secret is never answered.

Bug found in the mod: after the secret arrived, `on_tick` saw "no client yet but
server info exists" and started `start_reconnect`, which opened a second socket
one second later (the second "Voice endpoint" line with a single "Secret
received" in the log). Fixed with a `CONNECTING` flag; the reconnect thread now
never opens a socket while a client exists or is starting.

New diagnostics: every server datagram is counted, and one that cannot be used is
logged with the reason (bad magic, bad length, wrong secret/GCM failure, unknown
packet). HUD states after 15 unanswered attempts: "Server replies but the reply
was rejected", "Voice port closed on server", "Voice server unreachable (UDP
dropped)".

## lunamoonsmp.com: server answers, replies were rejected (2026-11-02)

Second live server (voice port 12004, `<same as game server>` host). With the
duplicate-socket fix in place there is one `Voice endpoint` line, and the server
answers every authentication attempt (14 datagrams in 15s) - so the network path
and the secret are fine: each reply passed AES-GCM authentication with our key.
The replies are 287 bytes: magic, a 2-byte VarInt length (284), IV, then a
256-byte plaintext, where an AuthenticateAck is normally 1 byte.

The engine rejected them because `decode_packet` required the plaintext to be
consumed exactly. The reference client does not: `NetworkMessage.readFromBytes`
reads the type byte and the fields it expects and ignores what follows, so a
server or proxy that pads its packets is valid for it. The engine now decodes the
prefix and ignores trailing bytes (logging once that the server pads). The strict
decoder remains for our own encodings. A test builds a 287-byte padded ack.

If a reply is still rejected, the log now prints the plaintext length, type byte
and first 16 plaintext bytes. The `connected peer` log now says why it is
unavailable (it was `<unknown>` on this server).

## Noise suppression, faster CI (2026-11-02)

Noise suppression (Simple Voice Chat's "denoiser", RNNoise) is on by default. It runs
on the microphone between resampling and the voice-activation check, so the threshold
sees the cleaned audio, like the official client. `/svchorizon denoise [on|off]`
toggles it and the choice is saved (`native_set_setting`).

The denoiser is the core of the `nnnoiseless` crate (BSD-3-Clause, a Rust port of
RNNoise) vendored in `engine/src/denoise`, with the FFT replaced by `denoise/fft.rs`
(an exact-size 960-point mixed-radix FFT) because Horizon builds with `--locked` and
its lockfile has neither `nnnoiseless` nor its FFT crate. The model weights are a Rust
byte table (`weights.rs`): a native bundle may only contain `.rs`/`.toml` files.
Checked against the original crate on a 4 s noisy-speech signal: relative error energy
1.8e-13, speech probability within 1.3e-6. Tests also check the FFT against a naive
DFT and that steady noise falls by more than 6 dB while a voiced sound keeps energy.

CI: `game-build.yml` takes a `profile` input (default Horizon's `fast`), restores its
cache with a per-run key and saves it with `if: always()` (a failed build no longer
discards compiled dependencies), and caches the horizon-mixins CLI. New
`quick-check.yml` applies the mixins and `cargo check`s the game on Linux on every push.

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
(`SVC: Voice server unreachable (UDP blocked?)` in red after 15 s, taking
precedence over the misleading "Transmitting" label), send failures logged
with Windows WSAECONNRESET/ICMP noise filtered, a stray ConnectionReset no
longer tears down the UDP worker, and a one-time log line at 15 s naming the
firewall/AV check and the hosting-panel port-forwarding fallback.

## Validation policy

Protocol unit tests alone do not prove server interoperability. The full SVC
wire path (TCP handshake, secret, UDP authentication, connection validation,
keepalive) is now proven against a real Paper server running the unmodified
official plugin — see "Live interop validation". What remains unproven is the
packaged mod running inside the actual Horizon client: the two-client audio
relay, official Fabric/Forge/NeoForge cross-talk and the in-game scenario
matrix still have to pass before the mod can be called done.
