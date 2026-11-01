use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use thiserror::Error;

pub const SECRET_BYTES: usize = 16;
pub const NONCE_BYTES: usize = 12;
pub const GCM_TAG_BYTES: usize = 16;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("AES-128-GCM authentication failed")]
    Authentication,
    #[error("system random number generator failed")]
    Randomness,
    #[error("encrypted datagram is truncated")]
    Truncated,
}

#[derive(Clone, PartialEq)]
pub struct VoiceSecret([u8; SECRET_BYTES]);

impl std::fmt::Debug for VoiceSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VoiceSecret([REDACTED])")
    }
}

impl VoiceSecret {
    pub fn from_bytes(bytes: [u8; SECRET_BYTES]) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8; SECRET_BYTES] {
        &self.0
    }

    fn key(&self) -> LessSafeKey {
        let key =
            UnboundKey::new(&aead::AES_128_GCM, &self.0).expect("AES-128 key has exactly 16 bytes");
        LessSafeKey::new(key)
    }

    /// Mirrors upstream Secret.encrypt(): random IV prefix and AES-128-GCM.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut nonce = [0u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| CryptoError::Randomness)?;
        let mut ciphertext = plaintext.to_vec();
        self.key()
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut ciphertext,
            )
            .map_err(|_| CryptoError::Authentication)?;
        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn decrypt(&self, encrypted: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if encrypted.len() < NONCE_BYTES + GCM_TAG_BYTES {
            return Err(CryptoError::Truncated);
        }
        let nonce: [u8; NONCE_BYTES] = encrypted[..NONCE_BYTES].try_into().expect("length checked");
        let mut plaintext = encrypted[NONCE_BYTES..].to_vec();
        let opened = self
            .key()
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut plaintext,
            )
            .map_err(|_| CryptoError::Authentication)?;
        Ok(opened.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_and_length() {
        let key = VoiceSecret::from_bytes([0x5a; 16]);
        let encrypted = key.encrypt(b"simple voice chat").unwrap();
        assert_eq!(encrypted.len(), NONCE_BYTES + 17 + GCM_TAG_BYTES);
        assert_eq!(key.decrypt(&encrypted).unwrap(), b"simple voice chat");
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let a = VoiceSecret::from_bytes([1; 16]);
        let b = VoiceSecret::from_bytes([2; 16]);
        assert!(matches!(
            b.decrypt(&a.encrypt(b"test").unwrap()),
            Err(CryptoError::Authentication)
        ));
    }

    #[test]
    fn tampering_fails_authentication() {
        let key = VoiceSecret::from_bytes([7; 16]);
        let mut encrypted = key.encrypt(b"voice frame").unwrap();
        *encrypted.last_mut().unwrap() ^= 0x80;
        assert!(matches!(
            key.decrypt(&encrypted),
            Err(CryptoError::Authentication)
        ));
    }
}
