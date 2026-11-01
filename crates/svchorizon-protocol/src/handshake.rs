//! Minecraft custom-payload handshake for the official Simple Voice Chat plugin.
//!
//! Source reference: RequestSecretPacket.java and SecretPacket.java in
//! henkelmax/simple-voice-chat. The TCP payload itself is not encrypted;
//! the secret is used for the separate UDP AES-GCM connection.

use crate::{
    crypto::VoiceSecret,
    wire::{Reader, WireError, Writer},
};
use thiserror::Error;
use uuid::Uuid;

pub const REQUEST_SECRET_CHANNEL: &str = "voicechat:request_secret";
pub const SECRET_CHANNEL: &str = "voicechat:secret";

/// Compatibility number of the Simple Voice Chat plugin generation this build
/// interoperates with. The shipped bukkit plugin for MC 26.3, 2.6.27, reports
/// compatibility version 20 (verified against the plugin jar: the server logs
/// "Compatibility version 20" and rejects every other number before it sends a
/// secret). Upstream's 2.7.0-pre1 development branch has already moved to 21
/// and also changed the wire format, so both numbers must be bumped together
/// once 2.7.x actually ships.
pub const COMPATIBILITY_VERSION: i32 = 20;

/// Compatibility number spoken by upstream's unreleased 2.7.0-pre1 branch.
pub const COMPATIBILITY_VERSION_2_7_PRE: i32 = 21;
pub const MAX_VOICE_HOST_CHARS: usize = 32_767;

#[derive(Debug, Error)]
pub enum HandshakeError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("invalid voice codec ordinal {0}")]
    InvalidCodec(u8),
    #[error("invalid voice UDP port {0}")]
    InvalidPort(i32),
    #[error("invalid MTU size {0}")]
    InvalidMtu(i32),
    #[error("invalid keepalive interval {0}")]
    InvalidKeepAlive(i32),
    #[error("voice host is malformed: {0}")]
    InvalidHost(String),
    #[error("unexpected trailing bytes in voice secret payload")]
    TrailingBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VoiceCodec {
    Voip = 0,
    Audio = 1,
    RestrictedLowDelay = 2,
}

impl TryFrom<u8> for VoiceCodec {
    type Error = HandshakeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Voip),
            1 => Ok(Self::Audio),
            2 => Ok(Self::RestrictedLowDelay),
            other => Err(HandshakeError::InvalidCodec(other)),
        }
    }
}

/// The complete SecretPacket fields sent by the official server plugin
/// (SVC 2.6.27, compatibility version 20).
///
/// In this wire generation the 16 secret bytes are only the AES key: every
/// client UDP datagram is prefixed with the player's own profile UUID and the
/// server looks its secret up with that UUID (`secrets` map keyed by
/// `player.getUniqueId()`, resolved in `NetworkMessage.readPacketServer`).
/// Upstream's 2.7.0-pre1 branch inserts a separate server-assigned connection
/// UUID into the payload; that format is not yet shipped.
#[derive(Clone)]
pub struct VoiceServerInfo {
    pub secret: VoiceSecret,
    pub server_port: u16,
    pub player_uuid: Uuid,
    pub codec: VoiceCodec,
    pub mtu_size: u16,
    pub voice_chat_distance: f64,
    pub keep_alive_ms: u32,
    pub groups_enabled: bool,
    pub voice_host: String,
    pub allow_recording: bool,
}

impl std::fmt::Debug for VoiceServerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceServerInfo")
            .field("secret", &"[REDACTED]")
            .field("server_port", &self.server_port)
            .field("player_uuid", &self.player_uuid)
            .field("codec", &self.codec)
            .field("mtu_size", &self.mtu_size)
            .field("voice_chat_distance", &self.voice_chat_distance)
            .field("keep_alive_ms", &self.keep_alive_ms)
            .field("groups_enabled", &self.groups_enabled)
            .field("voice_host", &self.voice_host)
            .field("allow_recording", &self.allow_recording)
            .finish()
    }
}

impl VoiceServerInfo {
    /// Decode SecretPacket.toBytes() in its exact field order for SVC 2.6.27
    /// (secret, port, player, codec, mtu, distance, keepalive, groups, host,
    /// allow recording).
    pub fn decode(payload: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(payload);
        let secret = VoiceSecret::from_bytes(r.take(16)?.try_into().expect("length checked"));
        let server_port = r.i32()?;
        if !(1..=u16::MAX as i32).contains(&server_port) {
            return Err(HandshakeError::InvalidPort(server_port));
        }
        let player_uuid = r.uuid()?;
        let codec = VoiceCodec::try_from(r.u8()?)?;
        let mtu_size = r.i32()?;
        if !(512..=2048).contains(&mtu_size) {
            return Err(HandshakeError::InvalidMtu(mtu_size));
        }
        let voice_chat_distance = r.f64()?;
        let keep_alive_ms = r.i32()?;
        if keep_alive_ms < 1000 {
            return Err(HandshakeError::InvalidKeepAlive(keep_alive_ms));
        }
        let groups_enabled = r.bool()?;
        let voice_host = r.utf(MAX_VOICE_HOST_CHARS)?;
        let allow_recording = r.bool()?;
        if !r.is_empty() {
            return Err(HandshakeError::TrailingBytes);
        }
        Ok(Self {
            secret,
            server_port: server_port as u16,
            player_uuid,
            codec,
            mtu_size: mtu_size as u16,
            voice_chat_distance,
            keep_alive_ms: keep_alive_ms as u32,
            groups_enabled,
            voice_host,
            allow_recording,
        })
    }

    /// Resolve the plugin's voice_host override using the same rules as the
    /// official client: empty => game server host/port; number => port only;
    /// host:port => host and optional port override.
    pub fn endpoint(&self, game_server_host: &str) -> Result<VoiceEndpoint, HandshakeError> {
        resolve_voice_endpoint(&self.voice_host, game_server_host, self.server_port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEndpoint {
    pub host: String,
    pub port: u16,
}

pub fn resolve_voice_endpoint(
    voice_host: &str,
    game_server_host: &str,
    default_port: u16,
) -> Result<VoiceEndpoint, HandshakeError> {
    if voice_host.is_empty() {
        return Ok(VoiceEndpoint {
            host: game_server_host.to_owned(),
            port: default_port,
        });
    }

    if let Ok(port) = voice_host.parse::<u16>() {
        if port == 0 {
            return Err(HandshakeError::InvalidPort(0));
        }
        return Ok(VoiceEndpoint {
            host: game_server_host.to_owned(),
            port,
        });
    }

    // URI authority parsing is intentionally strict and supports bracketed
    // IPv6 literals. It never performs DNS resolution on the game thread.
    let (host, port) = if let Some(rest) = voice_host.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| HandshakeError::InvalidHost(voice_host.into()))?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        if suffix.is_empty() {
            (host, default_port)
        } else if let Some(port_text) = suffix.strip_prefix(':') {
            (host, parse_port(port_text, voice_host)?)
        } else {
            return Err(HandshakeError::InvalidHost(voice_host.into()));
        }
    } else if let Some((host, port_text)) = voice_host.rsplit_once(':') {
        if host.contains(':') {
            return Err(HandshakeError::InvalidHost(voice_host.into()));
        }
        (host, parse_port(port_text, voice_host)?)
    } else {
        (voice_host, default_port)
    };

    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(HandshakeError::InvalidHost(voice_host.into()));
    }
    Ok(VoiceEndpoint {
        host: host.to_owned(),
        port,
    })
}

fn parse_port(value: &str, original: &str) -> Result<u16, HandshakeError> {
    let port = value
        .parse::<u16>()
        .map_err(|_| HandshakeError::InvalidHost(original.into()))?;
    if port == 0 {
        return Err(HandshakeError::InvalidHost(original.into()));
    }
    Ok(port)
}

/// Encodes RequestSecretPacket.toBytes(): one big-endian 32-bit compatibility
/// integer, carried in the Minecraft custom payload voicechat:request_secret.
pub fn encode_request_secret(compatibility_version: i32) -> Vec<u8> {
    let mut w = Writer::default();
    w.i32(compatibility_version);
    w.data
}

pub fn decode_request_secret(payload: &[u8]) -> Result<i32, HandshakeError> {
    let mut r = Reader::new(payload);
    let version = r.i32()?;
    if !r.is_empty() {
        return Err(HandshakeError::TrailingBytes);
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_payload(host: &str) -> Vec<u8> {
        let mut w = Writer::default();
        w.raw(&[0x42; 16]);
        w.i32(24454);
        w.uuid(Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap());
        w.u8(VoiceCodec::Voip as u8);
        w.i32(1024);
        w.f64(48.0);
        w.i32(1000);
        w.bool(true);
        w.utf(host);
        w.bool(false);
        w.data
    }

    #[test]
    fn request_secret_is_one_big_endian_i32() {
        let payload = encode_request_secret(COMPATIBILITY_VERSION);
        assert_eq!(payload, 20i32.to_be_bytes());
        assert_eq!(
            decode_request_secret(&payload).unwrap(),
            COMPATIBILITY_VERSION
        );
    }

    #[test]
    fn decodes_secret_packet_fields_in_upstream_order() {
        let packet = VoiceServerInfo::decode(&secret_payload("")).unwrap();
        assert_eq!(packet.server_port, 24454);
        assert_eq!(
            packet.player_uuid.to_string(),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
        assert_eq!(packet.codec, VoiceCodec::Voip);
        assert_eq!(packet.mtu_size, 1024);
        assert_eq!(packet.voice_chat_distance, 48.0);
        assert!(packet.groups_enabled);
        assert!(!packet.allow_recording);
        assert_eq!(packet.secret.as_bytes(), &[0x42; 16]);
    }

    #[test]
    fn resolves_default_numeric_and_host_port_overrides() {
        assert_eq!(
            resolve_voice_endpoint("", "mc.example.org", 24454).unwrap(),
            VoiceEndpoint {
                host: "mc.example.org".into(),
                port: 24454
            }
        );
        assert_eq!(
            resolve_voice_endpoint("25565", "mc.example.org", 24454).unwrap(),
            VoiceEndpoint {
                host: "mc.example.org".into(),
                port: 25565
            }
        );
        assert_eq!(
            resolve_voice_endpoint("voice.example.org:24454", "mc.example.org", 1).unwrap(),
            VoiceEndpoint {
                host: "voice.example.org".into(),
                port: 24454
            }
        );
        assert_eq!(
            resolve_voice_endpoint("[2001:db8::1]:24454", "mc.example.org", 1).unwrap(),
            VoiceEndpoint {
                host: "2001:db8::1".into(),
                port: 24454
            }
        );
    }

    #[test]
    fn rejects_invalid_codec_and_extra_data() {
        let mut payload = secret_payload("");
        // secret(16) + port(4) + player UUID(16): the codec byte.
        payload[36] = 99;
        assert!(matches!(
            VoiceServerInfo::decode(&payload),
            Err(HandshakeError::InvalidCodec(99))
        ));
        assert!(matches!(
            decode_request_secret(&[0, 0, 0, 21, 1]),
            Err(HandshakeError::TrailingBytes)
        ));
    }
}
