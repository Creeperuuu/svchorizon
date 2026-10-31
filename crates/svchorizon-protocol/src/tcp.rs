use crate::wire::{Reader, WireError, Writer};
use thiserror::Error;
use uuid::Uuid;

/// Compatibility version in Simple Voice Chat 2.6.27+ / MC 26.3
/// (upstream gradle.properties: voicechat_compatibility_version=21).
pub const COMPATIBILITY_VERSION: i32 = 21;
pub const REQUEST_SECRET_CHANNEL: &str = "voicechat:request_secret";
pub const SECRET_CHANNEL: &str = "voicechat:secret";

#[derive(Debug, Error)]
pub enum TcpPayloadError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("invalid voice server UDP port {0}")]
    InvalidPort(i32),
    #[error("invalid voice server metadata")]
    InvalidMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceServerInfo {
    pub secret: [u8; 16],
    /// The connection UUID that keys every client UDP datagram.
    pub connection_id: Uuid,
    pub server_port: u16,
    pub player_uuid: Uuid,
    pub codec_ordinal: u8,
    pub mtu_size: i32,
    pub voice_chat_distance: f64,
    pub keep_alive_seconds: i32,
    pub groups_enabled: bool,
    pub voice_host: String,
    pub allow_recording: bool,
}

pub fn request_secret_payload() -> Vec<u8> {
    let mut w = Writer::default();
    w.i32(COMPATIBILITY_VERSION);
    w.data
}

impl VoiceServerInfo {
    /// Decode SecretPacket.toBytes() from the data portion of the custom payload.
    pub fn decode(data: &[u8]) -> Result<Self, TcpPayloadError> {
        let mut r = Reader::new(data);
        let secret = r.take(16)?.try_into().expect("length checked");
        let connection_id = r.uuid()?;
        let port = r.i32()?;
        let server_port = u16::try_from(port).map_err(|_| TcpPayloadError::InvalidPort(port))?;
        if server_port == 0 { return Err(TcpPayloadError::InvalidPort(port)); }
        let player_uuid = r.uuid()?;
        let codec_ordinal = r.u8()?;
        let mtu_size = r.i32()?;
        let voice_chat_distance = r.f64()?;
        let keep_alive_seconds = r.i32()?;
        let groups_enabled = r.bool()?;
        let voice_host = r.utf(32767)?;
        let allow_recording = r.bool()?;
        r.finish()?;
        if mtu_size <= 0 || keep_alive_seconds <= 0 || !voice_chat_distance.is_finite() || voice_chat_distance < 0.0 {
            return Err(TcpPayloadError::InvalidMetadata);
        }
        Ok(Self { secret, connection_id, server_port, player_uuid, codec_ordinal, mtu_size,
            voice_chat_distance, keep_alive_seconds, groups_enabled, voice_host, allow_recording })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_upstream_big_endian_i32() {
        assert_eq!(request_secret_payload(), COMPATIBILITY_VERSION.to_be_bytes());
    }

    #[test]
    fn decodes_secret_packet_fields_in_upstream_order() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let connection = Uuid::parse_str("99aa77bb-1111-2222-3333-445566778899").unwrap();
        let mut w = Writer::default();
        w.raw(&[0x44; 16]); w.uuid(connection); w.i32(24454); w.uuid(id); w.u8(0);
        w.i32(1200); w.f64(48.0); w.i32(1000); w.bool(true);
        w.utf("voice.example.test"); w.bool(false);
        let parsed = VoiceServerInfo::decode(&w.data).unwrap();
        assert_eq!(parsed.secret, [0x44; 16]);
        assert_eq!(parsed.connection_id, connection);
        assert_eq!(parsed.server_port, 24454);
        assert_eq!(parsed.player_uuid, id);
        assert_eq!(parsed.mtu_size, 1200);
        assert_eq!(parsed.voice_chat_distance, 48.0);
        assert_eq!(parsed.keep_alive_seconds, 1000);
        assert!(parsed.groups_enabled);
        assert_eq!(parsed.voice_host, "voice.example.test");
        assert!(!parsed.allow_recording);
    }

    #[test]
    fn rejects_invalid_port() {
        let mut w = Writer::default();
        w.raw(&[0; 16]); w.uuid(Uuid::nil()); w.i32(70_000); w.uuid(Uuid::nil()); w.u8(0);
        w.i32(1200); w.f64(48.0); w.i32(1000); w.bool(false); w.utf(""); w.bool(false);
        assert!(matches!(VoiceServerInfo::decode(&w.data), Err(TcpPayloadError::InvalidPort(_))));
    }
}
