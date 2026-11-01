# Simple Voice Chat protocol notes

These notes track the implementation against the two deployed official Simple
Voice Chat wire generations. Compat 20 is the **shipped** release line (e.g.
`voicechat-bukkit-2.6.27`); compat 21 is upstream's 2.7.x generation. Every
compat-20 detail below was verified twice: first against the plugin jar's
bytecode (javap), then by a live end-to-end run against a real Paper 26.3
server running that unmodified plugin. The compat-21 differences were
verified against the upstream `2.7.0` source (branches `1.21.11`, `26.x`):
an extra connection UUID in the secret payload and the UDP envelopes, an
empty Authenticate packet, and a nonce challenge echoed through the
connection check. The client probes compatibility 21 first (the current
upstream SVC 2.7.0-pre1 build for Minecraft 26.3), then probes 20 after a
short timeout for older 2.6.x servers. Probes are sequential because the
official server sends a visible incompatibility chat message for a mismatched
request. Once a secret arrives, probing stops and the matching UDP wire
generation is used.

The Rust protocol and audio engine are isolated from Horizon's game hooks so
their wire behavior can be tested independently.

## TCP initialization

Bukkit/Paper deliver a custom payload only for channels the client registered, so on every play
`Login` the mod first sends the vanilla channel-registration payload and then the secret request:

1. Channel: `minecraft:register`, body: every `voicechat:*` channel the official plugin
   registers (NetManager.init), NUL-separated, mirroring the official client.
2. Channel: `voicechat:request_secret`, body: 32-bit big-endian compatibility
   version, `21` first and `20` after a short timeout if no secret arrives. The server compares the client version exactly; a mismatch triggers an incompatibility chat message rather than silently ignoring the request. A successful secret response ends probing and selects that wire generation.

The server replies on `voicechat:secret` with (SecretPacket.toBytes order,
verified from the 2.6.27 jar):

1. 16-byte AES secret (random bytes; there is **no** connection UUID in the
   2.6.27 payload — the 2.7.x generation inserts a server-assigned connection
   UUID here, which is what compatibility version 21 signals; the client
   decodes both layouts strictly and picks the one that parses)
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
| `voicechat:request_secret` | Client → server | Probes compatibility 21 first, then 20 after a short timeout; remembers the generation the server accepted |
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
