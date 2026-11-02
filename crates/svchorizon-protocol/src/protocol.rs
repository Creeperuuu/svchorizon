use crate::{
    crypto::{CryptoError, VoiceSecret},
    wire::{Reader, WireError, Writer},
};
use thiserror::Error;
use uuid::Uuid;

pub const MAGIC: u8 = 0xff;
/// Upstream Utils.MAX_VOICE_CHAT_PACKET_SIZE.
pub const MAX_VOICE_PACKET: usize = 2048;
/// Upstream AudioUtils.MAX_OPUS_PAYLOAD_SIZE.
pub const MAX_OPUS_PAYLOAD: usize = 1275;
const MIC: u8 = 0x01;
const PLAYER_SOUND: u8 = 0x02;
const GROUP_SOUND: u8 = 0x03;
const LOCATION_SOUND: u8 = 0x04;
const AUTHENTICATE: u8 = 0x05;
const AUTHENTICATE_ACK: u8 = 0x06;
const PING: u8 = 0x07;
const KEEP_ALIVE: u8 = 0x08;
const CONNECTION_CHECK: u8 = 0x09;
const CONNECTION_CHECK_ACK: u8 = 0x0a;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("invalid Simple Voice Chat magic byte")]
    BadMagic,
    #[error("unknown packet id {0:#04x}")]
    UnknownPacket(u8),
    #[error("invalid packet payload")]
    InvalidPacket,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VoicePacket {
    Mic {
        opus: Vec<u8>,
        sequence: i64,
        whispering: bool,
    },
    /// SVC 2.6.27 authentication: the player UUID plus the 16-byte secret
    /// issued in the SecretPacket (verified against voicechat-bukkit-2.6.27:
    /// AuthenticatePacket.toBytes writes playerUUID, then Secret.toBytes).
    Authenticate {
        player_uuid: Uuid,
        secret: VoiceSecret,
    },
    AuthenticateAck,
    ConnectionCheck,
    KeepAlive,
    ConnectionCheckAck,
    Ping {
        id: Uuid,
        timestamp: i64,
    },
    PlayerSound {
        channel_id: Uuid,
        sender: Uuid,
        opus: Vec<u8>,
        sequence: i64,
        distance: f32,
        whispering: bool,
        category: Option<String>,
    },
    GroupSound {
        channel_id: Uuid,
        sender: Uuid,
        opus: Vec<u8>,
        sequence: i64,
        category: Option<String>,
    },
    LocationSound {
        channel_id: Uuid,
        sender: Uuid,
        location: [f64; 3],
        opus: Vec<u8>,
        sequence: i64,
        distance: f32,
        category: Option<String>,
    },
}

fn write_category_flags(
    w: &mut Writer,
    category: &Option<String>,
    whispering: bool,
) -> Result<(), ProtocolError> {
    if category.as_ref().is_some_and(|s| s.chars().count() > 16) {
        return Err(ProtocolError::InvalidPacket);
    }
    let mut flags = 0u8;
    if whispering {
        flags |= 1;
    }
    if category.is_some() {
        flags |= 2;
    }
    w.u8(flags);
    if let Some(value) = category {
        w.utf(value);
    }
    Ok(())
}

impl VoicePacket {
    fn encode_plain(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut w = Writer::default();
        match self {
            Self::Mic {
                opus,
                sequence,
                whispering,
            } => {
                if opus.len() > MAX_OPUS_PAYLOAD {
                    return Err(ProtocolError::InvalidPacket);
                }
                w.u8(MIC);
                w.byte_array(opus);
                w.i64(*sequence);
                w.bool(*whispering);
            }
            Self::Authenticate {
                player_uuid,
                secret,
            } => {
                w.u8(AUTHENTICATE);
                w.uuid(*player_uuid);
                w.raw(secret.as_bytes());
            }
            Self::ConnectionCheck => w.u8(CONNECTION_CHECK),
            Self::KeepAlive => w.u8(KEEP_ALIVE),
            Self::AuthenticateAck => w.u8(AUTHENTICATE_ACK),
            Self::ConnectionCheckAck => w.u8(CONNECTION_CHECK_ACK),
            Self::Ping { id, timestamp } => {
                w.u8(PING);
                w.uuid(*id);
                w.i64(*timestamp);
            }
            Self::PlayerSound {
                channel_id,
                sender,
                opus,
                sequence,
                distance,
                whispering,
                category,
            } => {
                if opus.len() > MAX_OPUS_PAYLOAD {
                    return Err(ProtocolError::InvalidPacket);
                }
                w.u8(PLAYER_SOUND);
                w.uuid(*channel_id);
                w.uuid(*sender);
                w.byte_array(opus);
                w.i64(*sequence);
                w.f32(*distance);
                write_category_flags(&mut w, category, *whispering)?;
            }
            Self::GroupSound {
                channel_id,
                sender,
                opus,
                sequence,
                category,
            } => {
                if opus.len() > MAX_OPUS_PAYLOAD {
                    return Err(ProtocolError::InvalidPacket);
                }
                w.u8(GROUP_SOUND);
                w.uuid(*channel_id);
                w.uuid(*sender);
                w.byte_array(opus);
                w.i64(*sequence);
                write_category_flags(&mut w, category, false)?;
            }
            Self::LocationSound {
                channel_id,
                sender,
                location,
                opus,
                sequence,
                distance,
                category,
            } => {
                if opus.len() > MAX_OPUS_PAYLOAD {
                    return Err(ProtocolError::InvalidPacket);
                }
                w.u8(LOCATION_SOUND);
                w.uuid(*channel_id);
                w.uuid(*sender);
                for coordinate in location {
                    w.f64(*coordinate);
                }
                w.byte_array(opus);
                w.i64(*sequence);
                w.f32(*distance);
                write_category_flags(&mut w, category, false)?;
            }
        }
        Ok(w.data)
    }

    fn decode_plain(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(bytes);
        let packet = match r.u8()? {
            MIC => Self::Mic {
                opus: r.byte_array(MAX_OPUS_PAYLOAD)?,
                sequence: r.i64()?,
                whispering: r.bool()?,
            },
            AUTHENTICATE => Self::Authenticate {
                player_uuid: r.uuid()?,
                secret: VoiceSecret::from_bytes(r.take(16)?.try_into().expect("length checked")),
            },
            CONNECTION_CHECK => Self::ConnectionCheck,
            KEEP_ALIVE => Self::KeepAlive,
            AUTHENTICATE_ACK => Self::AuthenticateAck,
            CONNECTION_CHECK_ACK => Self::ConnectionCheckAck,
            PING => Self::Ping {
                id: r.uuid()?,
                timestamp: r.i64()?,
            },
            PLAYER_SOUND => {
                let channel_id = r.uuid()?;
                let sender = r.uuid()?;
                let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
                let sequence = r.i64()?;
                let distance = r.f32()?;
                let flags = r.u8()?;
                let category = if flags & 2 != 0 {
                    Some(r.utf(16)?)
                } else {
                    None
                };
                Self::PlayerSound {
                    channel_id,
                    sender,
                    opus,
                    sequence,
                    distance,
                    whispering: flags & 1 != 0,
                    category,
                }
            }
            GROUP_SOUND => {
                let channel_id = r.uuid()?;
                let sender = r.uuid()?;
                let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
                let sequence = r.i64()?;
                let flags = r.u8()?;
                let category = if flags & 2 != 0 {
                    Some(r.utf(16)?)
                } else {
                    None
                };
                Self::GroupSound {
                    channel_id,
                    sender,
                    opus,
                    sequence,
                    category,
                }
            }
            LOCATION_SOUND => {
                let channel_id = r.uuid()?;
                let sender = r.uuid()?;
                let location = [r.f64()?, r.f64()?, r.f64()?];
                let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
                let sequence = r.i64()?;
                let distance = r.f32()?;
                let flags = r.u8()?;
                let category = if flags & 2 != 0 {
                    Some(r.utf(16)?)
                } else {
                    None
                };
                Self::LocationSound {
                    channel_id,
                    sender,
                    location,
                    opus,
                    sequence,
                    distance,
                    category,
                }
            }
            other => return Err(ProtocolError::UnknownPacket(other)),
        };
        r.finish()?;
        Ok(packet)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientDatagram {
    pub connection_id: Uuid,
    pub packet: VoicePacket,
}
impl ClientDatagram {
    /// Client datagram framing (NetworkMessage.readPacketServer): magic byte,
    /// a UUID the server resolves the per-player AES key with, then the
    /// varint-prefixed encrypted payload. In the SVC 2.6.27 wire the UUID is
    /// the player's own profile UUID; upstream 2.7.0-pre1 switches to a
    /// server-assigned connection UUID.
    pub fn encode(&self, secret: &VoiceSecret) -> Result<Vec<u8>, ProtocolError> {
        let encrypted = secret.encrypt(&self.packet.encode_plain()?)?;
        let mut w = Writer::default();
        w.u8(MAGIC);
        w.uuid(self.connection_id);
        w.byte_array(&encrypted);
        Ok(w.data)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerDatagram {
    pub packet: VoicePacket,
}
impl ServerDatagram {
    /// ClientNetworkMessage.readPacketClient: magic + encrypted byte array (no UUID prefix).
    pub fn decode(bytes: &[u8], secret: &VoiceSecret) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(bytes);
        if r.u8()? != MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let encrypted = r.byte_array(MAX_VOICE_PACKET)?;
        r.finish()?;
        Ok(Self {
            packet: VoicePacket::decode_plain(&secret.decrypt(&encrypted)?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn secret() -> VoiceSecret {
        VoiceSecret::from_bytes([0x33; 16])
    }

    fn server_wrap(packet: &VoicePacket) -> Vec<u8> {
        let encrypted = secret().encrypt(&packet.encode_plain().unwrap()).unwrap();
        let mut w = Writer::default();
        w.u8(MAGIC);
        w.byte_array(&encrypted);
        w.data
    }

    #[test]
    fn client_datagram_prefix_is_magic_and_connection_id() {
        let connection = Uuid::parse_str("99aa77bb-1111-2222-3333-445566778899").unwrap();
        let d = ClientDatagram {
            connection_id: connection,
            packet: VoicePacket::Authenticate {
                player_uuid: Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
                secret: secret(),
            },
        };
        let bytes = d.encode(&secret()).unwrap();
        assert_eq!(bytes[0], MAGIC);
        assert_eq!(&bytes[1..17], connection.as_bytes());
        assert!(bytes.len() > 17 + 12 + 16);
    }

    #[test]
    fn authenticate_packet_carries_player_and_secret() {
        let player = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let packet = VoicePacket::Authenticate {
            player_uuid: player,
            secret: secret(),
        };
        assert_eq!(
            VoicePacket::decode_plain(&packet.encode_plain().unwrap()).unwrap(),
            packet
        );
        let plain = packet.encode_plain().unwrap();
        assert_eq!(plain[0], AUTHENTICATE);
        assert_eq!(&plain[1..17], player.as_bytes());
        assert_eq!(&plain[17..33], secret().as_bytes());
    }

    #[test]
    fn server_auth_ack_decrypts() {
        assert_eq!(
            ServerDatagram::decode(&server_wrap(&VoicePacket::AuthenticateAck), &secret())
                .unwrap()
                .packet,
            VoicePacket::AuthenticateAck
        );
    }

    #[test]
    fn microphone_packet_round_trip() {
        let packet = VoicePacket::Mic {
            opus: vec![1, 2, 3],
            sequence: 42,
            whispering: true,
        };
        assert_eq!(
            ServerDatagram::decode(&server_wrap(&packet), &secret())
                .unwrap()
                .packet,
            packet
        );
    }

    #[test]
    fn all_sound_variants_round_trip() {
        let id = Uuid::new_v4();
        let sender = Uuid::new_v4();
        let packets = [
            VoicePacket::PlayerSound {
                channel_id: id,
                sender,
                opus: vec![1, 2, 3],
                sequence: 10,
                distance: 32.0,
                whispering: true,
                category: Some("voice".into()),
            },
            VoicePacket::GroupSound {
                channel_id: id,
                sender,
                opus: vec![4, 5],
                sequence: 11,
                category: None,
            },
            VoicePacket::LocationSound {
                channel_id: id,
                sender,
                location: [1.25, 64.0, -8.5],
                opus: vec![6, 7],
                sequence: 12,
                distance: 20.0,
                category: Some("voice".into()),
            },
            VoicePacket::Ping {
                id,
                timestamp: 123456,
            },
        ];
        for packet in packets {
            assert_eq!(
                ServerDatagram::decode(&server_wrap(&packet), &secret())
                    .unwrap()
                    .packet,
                packet
            );
        }
    }
}
