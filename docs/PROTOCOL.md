# Protocol notes

Audited against the official Simple Voice Chat source.

## TCP initialization — not implemented yet

- `voicechat:request_secret`: client sends a 32-bit compatibility version.
- `voicechat:secret`: server returns a 16-byte secret, UDP port, player UUID, codec ordinal, MTU, voice distance, keepalive interval, groups flag, voice host, and recording permission.

The exact current upstream packet codecs and compatibility version must be ported before connecting.

## UDP envelope

Client to server:
1. Magic byte `0xff`.
2. Player UUID (16 bytes).
3. Minecraft VarInt-length-prefixed encrypted NetworkMessage.

Server to client:
1. Magic byte `0xff`.
2. Minecraft VarInt-length-prefixed encrypted NetworkMessage.

The encrypted message is packet ID + packet body. AES-128-GCM uses a random 12-byte IV prepended to ciphertext and the 16-byte authentication tag. No additional AAD is used by upstream.

## Packet IDs

| ID | Packet | Status |
|---:|---|---|
| 0x01 | MicPacket | Initial encoding implemented |
| 0x02 | PlayerSoundPacket | Initial encoding implemented |
| 0x03 | GroupSoundPacket | Missing |
| 0x04 | LocationSoundPacket | Missing |
| 0x05 | AuthenticatePacket | Initial encoding implemented |
| 0x06 | AuthenticateAckPacket | Implemented |
| 0x07 | PingPacket | Missing |
| 0x08 | KeepAlivePacket | Implemented |
| 0x09 | ConnectionCheckPacket | Implemented |
| 0x0a | ConnectionCheckAckPacket | Implemented |

Official source references:
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/NetworkMessage.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/Secret.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientNetworkMessage.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientVoicechatConnection.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/MicPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/PlayerSoundPacket.java`

These notes are not protocol conformance certification. Verify against the exact target plugin/client version.
