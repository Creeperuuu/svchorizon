# Simple Voice Chat protocol notes

These notes track the implementation against the three official Simple
Voice Chat wire generations this client speaks. Compat 20 is the **shipped**
release line (e.g. `voicechat-bukkit-2.6.27`, published for every Minecraft
version from 1.8.8 to the current 26.x); compat 21 is upstream's 2.7.x
generation (released as 2.7.0-pre1); compat 18 is the previous 2.5.x release
line still running on servers that have not updated. Every compat-20 detail
below was verified twice: first against the plugin jar's bytecode (javap),
then by a live end-to-end run against a real Paper 26.3 server running that
unmodified plugin. The compat-21 differences were verified against the
upstream `2.7.0` source (branches `1.21.11`, `26.x`): an extra connection
UUID in the secret payload and the UDP envelopes, an empty Authenticate
packet, and a nonce challenge echoed through the connection check. The
compat-18 wire was verified against upstream `outdated/1.21.6-2.5.30`: its
secret payload, UDP envelope, packet type ids and Authenticate layout are
byte-identical to compat 20 - only the handshake number differs.

The compatibility number is a property of the server's plugin build, not of
one Minecraft version. The client therefore probes 20, 21 and 18 **one at a
time** on every join and switches its wire format to whichever generation the
server answers with, so older and newer servers are supported without any
single-version workaround.

The Rust protocol and audio engine are isolated from Horizon's game hooks so
their wire behavior can be tested independently.

## TCP initialization

Bukkit/Paper deliver a custom payload only for channels the client registered, so on every play
`Login` the mod first sends the vanilla channel-registration payload and then the secret request:

1. Channel: `minecraft:register`, body: every `voicechat:*` channel the official plugin
   registers (NetManager.init), NUL-separated, mirroring the official client.
2. Channel: `voicechat:request_secret`, body: 32-bit big-endian compatibility
   version - **one generation per request**, in the order 20, 21, 18. The
   server compares the number against its own build: a match answers with the
   secret, a mismatch answers with the `message.voicechat.incompatible_version`
   system chat notice (ServerVoiceEvents.getIncompatibleMessage) and **no**
   secret, and a repeat of the matching number is silently ignored because the
   secret is generated once per login ("Player already requested secret").

Probing is strictly serial for three reasons:

- A rejection notice is a definitive answer. The client detects the notice
  key in the `system_chat` packet and immediately probes the next generation
  instead of waiting for a timeout, and never probes a rejected generation
  again this login (at most one notice per server in the chat).
- Upstream stores the compatibility number of the **last** request per player
  and drops every non-secret payload from a player whose stored number does
  not match (PaperNetManager's `isCompatible` gate). A stale second probe
  would flip that flag after a successful handshake and make the server
  silently discard this client's state and group packets.
- One request at a time leaves the server's per-player compatibility flag on
  the generation that actually worked.

While no generation has answered, the probe round repeats (4 s per
generation during the first 15 s, then every 10 s) so a dropped first
request - a channel-registration race - recovers without a relog.

The server replies on `voicechat:secret` with (SecretPacket.toBytes order,
verified from the 2.6.27 jar; 2.5.x sends the identical field order):

1. 16-byte AES secret (random bytes; there is **no** connection UUID in the
   2.6.27 payload — the 2.7.x generation inserts a server-assigned connection
   UUID here, which is what compatibility version 21 signals; the client
   decodes both layouts strictly and picks the one that parses, expecting
   the layout of the generation it probed)
2. UDP port (32-bit integer)
3. Player UUID (16 bytes)
4. Codec ordinal (byte)
5. MTU (32-bit integer)
6. Voice distance (64-bit double)
7. Keepalive interval (32-bit integer, milliseconds)
8. Groups-enabled boolean
9. Voice host (Minecraft VarInt-length-prefixed UTF-8)
10. Allow-recording boolean

The decoder rejects invalid ports, unsupported codec ordinals, invalid limits, truncated values and trailing bytes. The voice host resolves like the official client: empty means the game server's host and port, a bare number means a port override, otherwise `host[:port]` (bracketed IPv6 supported).

## Implemented TCP custom payloads

| Channel | Direction | Current behavior |
|---|---|---|
| `voicechat:request_secret` | Client → server | Probes compatibility 20, then 21, then 18 one at a time; a rejection notice skips to the next generation, a secret locks it in for the session |
| `voicechat:secret` | Server → client | Decodes session secret and UDP metadata; starts voice workers |
| `voicechat:client_state` | Client → server | Synchronizes local microphone mute/disabled flags |
| `voicechat:states` | Server → client | Parses bounded player-state snapshots |
| `voicechat:state` | Server → client | Parses one player-state update |
| `voicechat:remove_state` | Server → client | Removes a player and clears their local audio controls |
| `voicechat:add_group` | Server → client | Tracks group UUID, name, password, persistence, visibility and type |
| `voicechat:remove_group` | Server → client | Removes group metadata |
| `voicechat:joined_group` | Server → client | Tracks active group and wrong-password result |
| `voicechat:create_group` | Client → server | Creates a public group named Horizon |
| `voicechat:set_group` | Client → server | Joins a selected group, optionally with its password |
| `voicechat:leave_group` | Client → server | Leaves the active group |
| `voicechat:add_category` | Server → client | Decodes and tracks bounded volume-category metadata, including optional fields and 16×16 icon bytes |
| `voicechat:remove_category` | Server → client | Removes a category by ID |

Category metadata is currently surfaced as a compact HUD summary; category-specific volume controls and the full settings interface are not implemented. Password-protected groups can be joined through `/svchorizon group join <password>`. Custom group names, optional passwords and NORMAL/OPEN/ISOLATED types can be created through `/svchorizon group create <name> [password|-] [type]`. Recording-related controls remain outstanding.

## UDP envelope and encryption

Client to server (NetworkMessage.readPacketServer on the plugin side):

1. Magic byte `0xff`
2. **The player's own profile UUID** (16 bytes) — the plugin keeps a
   `Map<UUID, Secret>` keyed by `player.getUniqueId()` and resolves the AES key
   with that UUID. It is not the secret bytes and (in 2.6.27) there is no
   server-assigned connection UUID.
3. Minecraft VarInt-length-prefixed encrypted NetworkMessage

Server to client (ClientNetworkMessage.readPacketClient):

1. Magic byte `0xff`
2. Minecraft VarInt-length-prefixed encrypted NetworkMessage

The encrypted message is a packet ID followed by its packet body. AES-128-GCM uses the 16-byte secret, a fresh random 12-byte IV prepended to ciphertext, and the 16-byte authentication tag (`Secret.IV_SIZE_BYTES = 12`, `TAG_LEN_BITS = 128`, verified against the jar). No additional AAD is used by upstream.

`AuthenticatePacket` (0x05) carries the player UUID followed by the raw 16-byte
secret (`AuthenticatePacket.toBytes` order, verified against the jar) — the
encrypted transport plus the echoed secret prove the client received its
secret. The client repeats AuthenticatePacket until the empty
AuthenticateAckPacket, then repeats the empty ConnectionCheckPacket until the
empty ConnectionCheckAckPacket. It responds to PingPacket (echo) and
KeepAlivePacket (reply) and enforces the server's keepalive timeout. Automatic
reconnect uses exponential backoff while the Minecraft play session remains
active.

## Unauthenticated reachability probe (PING_V1)

Every released generation ships a debug ping (`debug/PingHandler`): a datagram
addressed to the magic UUID `58bc9ae9-c7a8-45e4-a11c-efbb67199425` is answered
with an unencrypted 24-byte echo whenever `allow_pings` is on (the default).
The frame is `[0xff][PING_V1 uuid][varint 24][16-byte id][8-byte timestamp]`
and the answer is exactly the 24-byte payload. It needs no secret, no
encryption and no envelope key, and it is byte-identical across 2.5.x, 2.6.x
and 2.7.x.

The client sends this probe every 5 seconds beside the authentication loop
while it is unauthenticated and reports the result:

- **Pong received** - the datagram path to the voice server works; the server
  is ignoring the authentication datagrams themselves (protocol or secret
  mismatch, e.g. a custom server build). Log: "Voice server answered an
  unauthenticated ping ..." and the HUD shows
  `SVC: Voice server rejects authentication`.
- **No pong** - nothing at all came back over UDP; the path is blocked
  (Windows Firewall/antivirus for horizon-client.exe, a proxy that does not
  forward UDP, or the server's firewall/hoster). Log: "No answer at all from
  the voice server ..." and the HUD shows `SVC: Voice server not responding`.

## Datagram envelope fallback

The envelope UUID keys the server-side secret lookup and differs per
generation: the player's profile UUID (2.5.x/2.6.x) or the per-connection UUID
from the secret response (2.7.x). The 16-byte secret is itself a UUID, and
custom server builds are known to key the lookup by it instead. While
authentication goes unanswered the worker therefore cycles the envelope
through `player UUID -> secret UUID -> connection UUID` (generation primary
first, duplicates and zero keys dropped) every 5 attempts, and freezes the key
that received the ack for all further traffic. On upstream builds the first
candidate always wins, so the fallback changes nothing.

A server that answers a `request_secret` with the incompatibility notice and
*still* issues a secret in another generation's wire layout (the server
rejected version 20 but sent a compat-20-layout secret, for example) is
running a custom build; the client logs the contradiction and trusts the
secret bytes, because the secret payload is the authoritative wire description.

## UDP packet coverage

| ID | Packet | Current status |
|---:|---|---|
| 0x01 | MicPacket | Client encoding implemented |
| 0x02 | PlayerSoundPacket | Server decoding + Opus playback implemented |
| 0x03 | GroupSoundPacket | Server decoding + Opus playback implemented |
| 0x04 | LocationSoundPacket | Server decoding + positional Opus playback implemented |
| 0x05 | AuthenticatePacket | Client encoding implemented |
| 0x06 | AuthenticateAckPacket | Server decoding implemented |
| 0x07 | PingPacket | Echo implemented |
| 0x08 | KeepAlivePacket | Reply implemented |
| 0x09 | ConnectionCheckPacket | Client encoding implemented |
| 0x0a | ConnectionCheckAckPacket | Server decoding implemented |

The voice engine uses a vendored pure-Rust Opus implementation and CPAL. It captures mono microphone audio, resamples to 48 kHz, encodes 20 ms frames, decodes incoming Opus frames, and resamples for the selected speaker device (or operating-system default). Player voice is spatialized using Horizon's tracked player positions; location sound uses packet coordinates; group sound is currently centered. Per-speaker playback queues allow multiple speakers to overlap, with bounded sequence reordering and short-burst Opus packet-loss concealment.

## Remaining protocol and integration work

- Group invites, persistent group settings and full group UI. Password entry and custom group names/types are available through native chat commands.
- Category-specific volume sliders and the complete settings interface.
- Recording-related TCP controls and recording UI.
- Two-client audio relay verification (a second player hearing the first), official Fabric/Forge/NeoForge client cross-talk, and server-transfer/reconnect races.

## Live interop evidence (2026-11-01)

`tools/interop-harness` joins a real Paper 26.3 (build 235) server running the
unmodified official `voicechat-bukkit-2.6.27` plugin using this crate's wire
implementation plus a hand-rolled protocol-777 Minecraft login. The run
completes the full official voice flow and the plugin logs, unedited:

- `Received secret request of svchorizon (20)` / `Sent secret to svchorizon`
- `Successfully authenticated player <uuid>` (UDP AES-GCM + Authenticate)
- `Successfully validated connection of player <uuid>` (ConnectionCheck)
- `Player svchorizon (<uuid>) successfully connected to voice chat`

The harness then exchanges keepalives and microphone frames for five seconds
and exits 0. Re-run it with the repository-root helper `scripts/run_interop.sh`
(boot Paper + plugin headless, wait for readiness, run the harness, print the
plugin's log lines).
