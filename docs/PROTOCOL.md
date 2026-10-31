# Simple Voice Chat protocol notes

These notes are based on the official `henkelmax/simple-voice-chat` client and server source. The Rust engine is intentionally isolated from Horizon's game hooks so its wire behavior can be tested independently.

## TCP initialization

The native Horizon hook sends the official custom payload:

- Channel: `voicechat:request_secret`
- Body: 32-bit big-endian compatibility version `20` (the value for upstream 2.6.28+26.3)

The server replies on `voicechat:secret` with, in order:

1. 16-byte AES secret
2. UDP port (32-bit integer)
3. player UUID (16 bytes)
4. codec ordinal (byte)
5. MTU (32-bit integer)
6. voice distance (64-bit double)
7. keepalive interval (32-bit integer, milliseconds)
8. groups-enabled boolean
9. voice host (Minecraft VarInt-length-prefixed UTF-8)
10. allow-recording boolean

The decoder rejects invalid ports, unsupported codec ordinals, invalid limits, truncated values and trailing bytes.

## UDP envelope and encryption

Client to server:

1. Magic byte `0xff`
2. Player UUID (16 bytes)
3. Minecraft VarInt-length-prefixed encrypted NetworkMessage

Server to client:

1. Magic byte `0xff`
2. Minecraft VarInt-length-prefixed encrypted NetworkMessage

The encrypted message is a packet ID followed by its packet body. AES-128-GCM uses the 16-byte secret, a fresh random 12-byte IV prepended to ciphertext, and the 16-byte authentication tag. No additional AAD is used by upstream.

The client repeats AuthenticatePacket until AuthenticateAckPacket, then repeats ConnectionCheckPacket until ConnectionCheckAckPacket. It responds to PingPacket and KeepAlivePacket and enforces the server's keepalive timeout.

## Packet coverage

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

The voice engine uses a pure-Rust Opus implementation and CPAL. It captures mono microphone audio, resamples to 48 kHz, encodes 20 ms frames, decodes incoming Opus frames, and resamples for the default speaker device. Player voice is spatialized using Horizon's tracked player positions; location sound uses its packet coordinates. Group sound is currently centered.

Group membership/control packets, static/entity sound variants, jitter buffering and per-player volume/mute are not implemented yet.

## Upstream source files

- `common/src/main/java/de/maxhenkel/voicechat/net/RequestSecretPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/net/SecretPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/NetworkMessage.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/Secret.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientNetworkMessage.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientVoicechatConnection.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/MicPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/PlayerSoundPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/GroupSoundPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/LocationSoundPacket.java`

This document describes the implementation's intended wire behavior; it is not an interoperability certification.
