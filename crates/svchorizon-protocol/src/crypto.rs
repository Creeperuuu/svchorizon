use aes_gcm::{Aes128Gcm, Nonce, aead::{Aead, KeyInit}};
use rand::{RngCore, rngs::OsRng};
use thiserror::Error;

pub const SECRET_BYTES: usize = 16;
pub const NONCE_BYTES: usize = 12;
pub const GCM_TAG_BYTES: usize = 16;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("AES-GCM authentication failed")] Authentication,
    #[error("encrypted datagram is truncated")] Truncated,
}

#[derive(Clone)]
pub struct VoiceSecret([u8; SECRET_BYTES]);

impl VoiceSecret {
    pub fn from_bytes(bytes: [u8; SECRET_BYTES]) -> Self { Self(bytes) }
    pub fn as_bytes(&self) -> &[u8; SECRET_BYTES] { &self.0 }

    /// Mirrors upstream Secret.encrypt(): random IV prefix, AES-128-GCM payload.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let cipher = Aes128Gcm::new_from_slice(&self.0).expect("16-byte AES key");
        let mut nonce = [0u8; NONCE_BYTES]; OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher.encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| CryptoError::Authentication)?;
        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce); out.extend_from_slice(&ciphertext); Ok(out)
    }

    pub fn decrypt(&self, encrypted: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if encrypted.len() < NONCE_BYTES + GCM_TAG_BYTES { return Err(CryptoError::Truncated); }
        let cipher = Aes128Gcm::new_from_slice(&self.0).expect("16-byte AES key");
        cipher.decrypt(Nonce::from_slice(&encrypted[..NONCE_BYTES]), &encrypted[NONCE_BYTES..])
            .map_err(|_| CryptoError::Authentication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypt_decrypt_and_length() {
        let key = VoiceSecret::from_bytes([0x5a; 16]);
        let encrypted = key.encrypt(b"simple voice chat").unwrap();
        assert_eq!(encrypted.len(), 12 + 17 + 16);
        assert_eq!(key.decrypt(&encrypted).unwrap(), b"simple voice chat");
    }
    #[test]
    fn wrong_key_fails_authentication() {
        let a = VoiceSecret::from_bytes([1; 16]);
        let b = VoiceSecret::from_bytes([2; 16]);
        assert!(matches!(b.decrypt(&a.encrypt(b"test").unwrap()), Err(CryptoError::Authentication)));
    }
}
