use crate::{crypto::{CryptoError, VoiceSecret}, wire::{Reader, WireError, Writer}};
use thiserror::Error;
use uuid::Uuid;

pub const MAGIC: u8 = 0xff;
pub const MAX_VOICE_PACKET: usize = 32 * 1024;
pub const MAX_OPUS_PAYLOAD: usize = 1024;
const MIC: u8 = 0x01;
const PLAYER_SOUND: u8 = 0x02;
const AUTHENTICATE: u8 = 0x05;
const AUTHENTICATE_ACK: u8 = 0x06;
const KEEP_ALIVE: u8 = 0x08;
const CONNECTION_CHECK: u8 = 0x09;
const CONNECTION_CHECK_ACK: u8 = 0x0a;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(transparent)] Wire(#[from] WireError),
    #[error(transparent)] Crypto(#[from] CryptoError),
    #[error("invalid Simple Voice Chat magic byte")] BadMagic,
    #[error("unknown packet id {0:#04x}")] UnknownPacket(u8),
    #[error("invalid packet payload")] InvalidPacket,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VoicePacket {
    Mic { opus: Vec<u8>, sequence: i64, whispering: bool },
    Authenticate { player_id: Uuid, secret: [u8; 16] },
    ConnectionCheck,
    KeepAlive,
    AuthenticateAck,
    ConnectionCheckAck,
    PlayerSound {
        channel_id: Uuid, sender: Uuid, opus: Vec<u8>, sequence: i64,
        distance: f32, whispering: bool, category: Option<String>,
    },
}

impl VoicePacket {
    fn encode_plain(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut w = Writer::default();
        match self {
            Self::Mic { opus, sequence, whispering } => {
                if opus.len() > MAX_OPUS_PAYLOAD { return Err(ProtocolError::InvalidPacket); }
                w.u8(MIC); w.byte_array(opus); w.i64(*sequence); w.bool(*whispering);
            }
            Self::Authenticate { player_id, secret } => {
                w.u8(AUTHENTICATE); w.uuid(*player_id); w.raw(secret);
            }
            Self::ConnectionCheck => w.u8(CONNECTION_CHECK),
            Self::KeepAlive => w.u8(KEEP_ALIVE),
            Self::AuthenticateAck => w.u8(AUTHENTICATE_ACK),
            Self::ConnectionCheckAck => w.u8(CONNECTION_CHECK_ACK),
            Self::PlayerSound { channel_id, sender, opus, sequence, distance, whispering, category } => {
                if opus.len() > MAX_OPUS_PAYLOAD { return Err(ProtocolError::InvalidPacket); }
                w.u8(PLAYER_SOUND); w.uuid(*channel_id); w.uuid(*sender);
                w.byte_array(opus); w.i64(*sequence); w.f32(*distance);
                let mut flags = 0u8;
                if *whispering { flags |= 1; }
                if category.is_some() { flags |= 2; }
                w.u8(flags);
                if let Some(value) = category {
                    if value.chars().count() > 16 { return Err(ProtocolError::InvalidPacket); }
                    w.utf(value);
                }
            }
        }
        Ok(w.data)
    }

    fn decode_plain(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(bytes);
        let packet = match r.u8()? {
            MIC => Self::Mic { opus: r.byte_array(MAX_OPUS_PAYLOAD)?,
                sequence: r.i64()?, whispering: r.bool()? },
            AUTHENTICATE => Self::Authenticate { player_id: r.uuid()?,
                secret: r.take(16)?.try_into().expect("length checked") },
            CONNECTION_CHECK => Self::ConnectionCheck,
            KEEP_ALIVE => Self::KeepAlive,
            AUTHENTICATE_ACK => Self::AuthenticateAck,
            CONNECTION_CHECK_ACK => Self::ConnectionCheckAck,
            PLAYER_SOUND => {
                let channel_id = r.uuid()?; let sender = r.uuid()?;
                let opus = r.byte_array(MAX_OPUS_PAYLOAD)?; let sequence = r.i64()?;
                let distance = r.f32()?; let flags = r.u8()?;
                let category = if flags & 2 != 0 { Some(r.utf(16)?) } else { None };
                Self::PlayerSound { channel_id, sender, opus, sequence, distance,
                    whispering: flags & 1 != 0, category }
            }
            other => return Err(ProtocolError::UnknownPacket(other)),
        };
        r.finish()?; Ok(packet)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientDatagram { pub player_id: Uuid, pub packet: VoicePacket }

impl ClientDatagram {
    /// ClientNetworkMessage.writeClient: 0xff + player UUID + VarInt byte array.
    pub fn encode(&self, secret: &VoiceSecret) -> Result<Vec<u8>, ProtocolError> {
        let encrypted = secret.encrypt(&self.packet.encode_plain()?)?;
        let mut w = Writer::default(); w.u8(MAGIC); w.uuid(self.player_id);
        w.byte_array(&encrypted); Ok(w.data)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerDatagram { pub packet: VoicePacket }

impl ServerDatagram {
    /// ClientNetworkMessage.readPacketClient: 0xff + VarInt byte array.
    pub fn decode(bytes: &[u8], secret: &VoiceSecret) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(bytes);
        if r.u8()? != MAGIC { return Err(ProtocolError::BadMagic); }
        let encrypted = r.byte_array(MAX_VOICE_PACKET)?; r.finish()?;
        Ok(Self { packet: VoicePacket::decode_plain(&secret.decrypt(&encrypted)?)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn secret() -> VoiceSecret { VoiceSecret::from_bytes([0x33; 16]) }

    #[test]
    fn client_datagram_prefix_is_magic_and_uuid() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let d = ClientDatagram { player_id: id,
            packet: VoicePacket::Authenticate { player_id: id, secret: [0x77; 16] } };
        let bytes = d.encode(&secret()).unwrap();
        assert_eq!(bytes[0], MAGIC); assert_eq!(&bytes[1..17], id.as_bytes());
        assert!(bytes.len() > 17 + 12 + 16);
    }

    #[test]
    fn server_auth_ack_decrypts() {
        let encrypted = secret().encrypt(&[AUTHENTICATE_ACK]).unwrap();
        let mut w = Writer::default(); w.u8(MAGIC); w.byte_array(&encrypted);
        assert_eq!(ServerDatagram::decode(&w.data, &secret()).unwrap().packet, VoicePacket::AuthenticateAck);
    }

    #[test]
    fn mic_packet_round_trip() {
        let packet = VoicePacket::Mic { opus: vec![1, 2, 3], sequence: 42, whispering: true };
        let encrypted = secret().encrypt(&packet.encode_plain().unwrap()).unwrap();
        let mut w = Writer::default(); w.u8(MAGIC); w.byte_array(&encrypted);
        assert_eq!(ServerDatagram::decode(&w.data, &secret()).unwrap().packet, packet);
    }
}
