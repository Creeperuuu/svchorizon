# Simple Voice Chat protocol notes

Audited against the official `henkelmax/simple-voice-chat` source tree.

## TCP initialization

- Client channel: `voicechat:request_secret`; data is a big-endian 32-bit compatibility version.
- Server channel: `voicechat:secret`; data is decoded in this exact order:
  1. 16-byte secret
  2. UDP port (32-bit signed integer)
  3. Player UUID (16 bytes)
  4. Codec ordinal (byte)
  5. MTU size (32-bit integer)
  6. Voice distance (64-bit float)
  7. Keepalive interval (32-bit integer)
  8. Groups enabled (boolean)
  9. Voice host (Minecraft VarInt-length-prefixed UTF-8)
  10. Recording allowed (boolean)

The current source pin uses compatibility version 20 from Simple Voice Chat 2.6.28+26.3. The version must match the server plugin exactly.

## UDP envelope

Client to server:
1. Magic byte `0xff`.
2. Player UUID (16 bytes).
3. Minecraft VarInt-length-prefixed encrypted payload.

Server to client:
1. Magic byte `0xff`.
2. Minecraft VarInt-length-prefixed encrypted payload.

The encrypted message is packet ID + packet body. AES-128-GCM uses a random 12-byte IV prepended to ciphertext and a 16-byte authentication tag. The current upstream implementation uses no additional AAD.

## Packet IDs

| ID | Packet | Status |
|---:|---|---|
| 0x01 | MicPacket | Serializer implemented |
| 0x02 | PlayerSoundPacket | Serializer implemented |
| 0x03 | GroupSoundPacket | Serializer implemented |
| 0x04 | LocationSoundPacket | Serializer implemented |
| 0x05 | AuthenticatePacket | Serializer implemented |
| 0x06 | AuthenticateAckPacket | Implemented |
| 0x07 | PingPacket | Serializer implemented |
| 0x08 | KeepAlivePacket | Implemented |
| 0x09 | ConnectionCheckPacket | Implemented |
| 0x0a | ConnectionCheckAckPacket | Implemented |

Upstream limits currently audited:
- Maximum encrypted voice packet: 2048 bytes (`Utils.MAX_VOICE_CHAT_PACKET_SIZE`).
- Maximum Opus payload: 1275 bytes (`AudioUtils.MAX_OPUS_PAYLOAD_SIZE`).

## Native Horizon integration

The initial native mixin hooks `Game::handle_packet` on the pinned Horizon revision. It sends the TCP request when the play Login packet arrives and decodes/stores the returned secret packet. The stored metadata is not yet connected to a UDP worker; therefore no audio is transmitted or played yet.

## Source references

- `common/src/main/java/de/maxhenkel/voicechat/voice/common/NetworkMessage.java`
- `common/src/main/java/de/maxhenkel/voicechat/voice/common/Secret.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientNetworkMessage.java`
- `common-client/src/main/java/de/maxhenkel/voicechat/voice/client/ClientVoicechatConnection.java`
- `common/src/main/java/de/maxhenkel/voicechat/net/RequestSecretPacket.java`
- `common/src/main/java/de/maxhenkel/voicechat/net/SecretPacket.java`

These notes are not protocol conformance certification. Verify against the exact target server/client release.
