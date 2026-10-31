//! Protocol primitives based on the official Simple Voice Chat client.
pub mod crypto;
pub mod protocol;
pub mod wire;

pub use crypto::VoiceSecret;
pub use protocol::{ClientDatagram, ServerDatagram, VoicePacket};
pub mod tcp;
