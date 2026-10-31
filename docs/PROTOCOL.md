# Simple Voice Chat protocol notes

These notes track the implementation against the official `henkelmax/simple-voice-chat` 26.3 source. The Rust protocol and audio engine are isolated from Horizon's game hooks so their wire behavior can be tested independently.

## TCP initialization

Bukkit/Paper deliver a custom payload only for channels the client registered, so on every play
`Login` the mod first sends the vanilla channel-registration payload and then the secret request:

1. Channel: `minecraft:register`, body: every `voicechat:*` channel the official plugin
   registers (NetManager.init), NUL-separated, mirroring the official client.
2. Channel: `voicechat:request_secret`, body: 32-bit big-endian compatibility version `21`
   (upstream gradle.properties `voicechat_compatibility_version=21` for 2.6.27+ / MC 26.3).
   The plugin compares the client version exactly and sends an incompatibility notice instead
   of a secret on any mismatch.

The server replies on `voicechat:secret` with (SecretPacket.toBytes order):

1. 16-byte AES secret
2. Connection UUID (16 bytes): keys every UDP datagram this client sends
3. UDP port (32-bit integer)
4. Player UUID (16 bytes)
5. Codec ordinal (byte)
6. MTU (32-bit integer)
7. Voice distance (64-bit double)
8. Keepalive interval (32-bit integer, milliseconds)
9. Groups-enabled boolean
10. Voice host (Minecraft VarInt-length-prefixed UTF-8)
11. Allow-recording boolean

The decoder rejects invalid ports, unsupported codec ordinals, invalid limits, truncated values and trailing bytes. The voice host resolves like the official client: empty means the game server's host and port, a bare number means a port override, otherwise `host[:port]` (bracketed IPv6 supported).

## Implemented TCP custom payloads

| Channel | Direction | Current behavior |
|---|---|---|
| `voicechat:request_secret` | Client → server | Requests the voice session using compatibility version 21 |
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

Client to server (ClientNetworkMessage.writeClient):

1. Magic byte `0xff`
2. Connection UUID (16 bytes) — the UUID from the secret payload, NOT the player's profile UUID
3. Minecraft VarInt-length-prefixed encrypted NetworkMessage

Server to client (ClientNetworkMessage.readPacketClient):

1. Magic byte `0xff`
2. Minecraft VarInt-length-prefixed encrypted NetworkMessage

The encrypted message is a packet ID followed by its packet body. AES-128-GCM uses the 16-byte secret, a fresh random 12-byte IV prepended to ciphertext, and the 16-byte authentication tag. No additional AAD is used by upstream.

Upstream's AuthenticatePacket carries no fields: its body is exactly the single packet ID byte. The client repeats AuthenticatePacket until AuthenticateAckPacket, then repeats ConnectionCheckPacket until ConnectionCheckAckPacket. It responds to PingPacket and KeepAlivePacket and enforces the server's keepalive timeout. Automatic reconnect uses exponential backoff while the Minecraft play session remains active.

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
- End-to-end interoperability testing against the official Paper/Spigot plugin and official Fabric/Forge/NeoForge clients.

This document describes intended wire behavior and source-level implementation; it is not an interoperability certification.
