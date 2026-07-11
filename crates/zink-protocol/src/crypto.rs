//! Envelope encryption (SPEC §6): encrypt the body once with a random
//! content-key, seal that key per recipient, verify the key commitment
//! before trusting anything.

use std::fmt;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand_core::CryptoRngCore;
use zeroize::Zeroize;

use crate::keys::{DeviceKey, PublicKey};
use crate::message::KeyCommitment;

/// Domain-separation context for the key commitment (SPEC §6/§11).
const KEY_COMMIT_CONTEXT: &str = "zink v1 key-commit";

const NONCE_SIZE: usize = 24;

/// Random per-message symmetric key. Encrypts one object (body or blob)
/// exactly once; sealed per recipient in the envelope's key-wraps.
pub struct ContentKey([u8; 32]);

impl ContentKey {
    pub fn generate(rng: &mut impl CryptoRngCore) -> Self {
        let mut key = [0u8; 32];
        rng.fill_bytes(&mut key);
        Self(key)
    }

    /// The commitment carried inside the hashed core. Binding the key into
    /// the id is what makes "same id ⇒ same content" hold: an AEAD alone is
    /// not key-committing, so without this a malicious sender could seal
    /// different keys to different recipients over one ciphertext.
    pub fn commitment(&self) -> KeyCommitment {
        KeyCommitment(blake3::derive_key(KEY_COMMIT_CONTEXT, &self.0))
    }

    /// Encrypt with a fresh random nonce; returns `nonce || ciphertext`.
    pub fn encrypt(&self, plaintext: &[u8], rng: &mut impl CryptoRngCore) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_SIZE];
        rng.fill_bytes(&mut nonce);
        let ciphertext = XChaCha20Poly1305::new((&self.0).into())
            .encrypt(&XNonce::from(nonce), plaintext)
            // Encrypting our own plaintext is not input parsing; XChaCha20-
            // Poly1305 only errors past a length no real message reaches.
            .expect("encryption of an in-memory plaintext cannot fail");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        out
    }

    /// Decrypt `nonce || ciphertext`. Errors on truncated, tampered, or
    /// wrong-key input — never panics.
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (nonce, ciphertext) = data
            .split_at_checked(NONCE_SIZE)
            .ok_or(CryptoError::Malformed)?;
        let nonce = XNonce::try_from(nonce).map_err(|_| CryptoError::Malformed)?;
        XChaCha20Poly1305::new((&self.0).into())
            .decrypt(&nonce, ciphertext)
            .map_err(|_| CryptoError::DecryptFailed)
    }

    /// Seal this key to a recipient device key (sealed-box over X25519,
    /// derived from the recipient's Ed25519 key).
    pub fn seal_for(
        &self,
        recipient: &PublicKey,
        rng: &mut impl CryptoRngCore,
    ) -> Result<Vec<u8>, CryptoError> {
        x25519_public(recipient)?
            .seal(rng, &self.0)
            .map_err(|_| CryptoError::SealFailed)
    }

    /// Unseal a content-key and verify it against the commitment from the
    /// hashed core. A key that doesn't match the commitment is rejected
    /// before anything is decrypted with it.
    pub fn open(
        sealed: &[u8],
        device: &DeviceKey,
        expected: &KeyCommitment,
    ) -> Result<Self, CryptoError> {
        let mut bytes = x25519_secret(device)
            .unseal(sealed)
            .map_err(|_| CryptoError::OpenFailed)?;
        // Zeroize the unsealed buffer on every path, including the
        // wrong-length error return.
        let array: Result<[u8; 32], _> = bytes[..].try_into();
        bytes.zeroize();
        let key = Self(array.map_err(|_| CryptoError::Malformed)?);
        if key.commitment() != *expected {
            return Err(CryptoError::CommitmentMismatch);
        }
        Ok(key)
    }
}

impl Drop for ContentKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for ContentKey {
    /// Never prints the key material.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentKey(..)")
    }
}

/// Encryption-layer failure. Never a panic, even on hostile input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// The recipient's public key bytes are not a valid Ed25519 point.
    InvalidRecipientKey,
    /// Sealing to the recipient failed.
    SealFailed,
    /// The sealed key could not be opened with this device key.
    OpenFailed,
    /// The unsealed key does not match the core's `key-commit` (SPEC §6):
    /// the sender committed to a different key. Do not trust the message.
    CommitmentMismatch,
    /// AEAD decryption failed: tampered ciphertext or wrong key.
    DecryptFailed,
    /// Input too short / wrong shape to be processed at all.
    Malformed,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipientKey => write!(f, "invalid recipient public key"),
            Self::SealFailed => write!(f, "sealing the content-key failed"),
            Self::OpenFailed => write!(f, "could not open the sealed content-key"),
            Self::CommitmentMismatch => write!(f, "content-key does not match its commitment"),
            Self::DecryptFailed => write!(f, "decryption failed"),
            Self::Malformed => write!(f, "malformed input"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Ed25519 → X25519 for the recipient side (Edwards → Montgomery point).
fn x25519_public(key: &PublicKey) -> Result<crypto_box::PublicKey, CryptoError> {
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&key.0)
        .map_err(|_| CryptoError::InvalidRecipientKey)?;
    Ok(crypto_box::PublicKey::from_bytes(
        verifying.to_montgomery().to_bytes(),
    ))
}

/// Ed25519 → X25519 for the device side (the clamped scalar half of the
/// expanded secret — the standard libsodium-compatible conversion).
fn x25519_secret(device: &DeviceKey) -> crypto_box::SecretKey {
    crypto_box::SecretKey::from(device.x25519_secret_bytes())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::testutil::rng;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    #[test]
    fn encrypt_seal_open_decrypt__should_roundtrip_for_n_recipients() {
        // Given
        let mut rng = rng();
        let recipients: Vec<DeviceKey> = (1..=3).map(device_key).collect();
        let plaintext = b"hello zink";
        let key = ContentKey::generate(&mut rng);
        let commit = key.commitment();

        // When: encrypted once, sealed per recipient
        let body = key.encrypt(plaintext, &mut rng);
        let sealed: Vec<Vec<u8>> = recipients
            .iter()
            .map(|r| key.seal_for(&r.public(), &mut rng).unwrap())
            .collect();

        // Then: every recipient opens its wrap and decrypts the one body
        for (recipient, sealed_key) in recipients.iter().zip(&sealed) {
            let opened = ContentKey::open(sealed_key, recipient, &commit).unwrap();
            assert_eq!(opened.decrypt(&body).unwrap(), plaintext);
        }
    }

    #[test]
    fn open__should_fail_with_a_device_key_that_was_not_sealed_to() {
        // Given
        let mut rng = rng();
        let key = ContentKey::generate(&mut rng);
        let sealed = key.seal_for(&device_key(1).public(), &mut rng).unwrap();

        // When
        let result = ContentKey::open(&sealed, &device_key(2), &key.commitment());

        // Then
        assert_eq!(result.unwrap_err(), CryptoError::OpenFailed);
    }

    #[test]
    fn open__should_reject_a_key_that_does_not_match_the_commitment() {
        // Given: sender seals key A but committed to key B in the core
        let mut rng = rng();
        let sealed_key = ContentKey::generate(&mut rng);
        let committed_key = ContentKey::generate(&mut rng);
        let recipient = device_key(1);
        let sealed = sealed_key.seal_for(&recipient.public(), &mut rng).unwrap();

        // When
        let result = ContentKey::open(&sealed, &recipient, &committed_key.commitment());

        // Then
        assert_eq!(result.unwrap_err(), CryptoError::CommitmentMismatch);
    }

    #[test]
    fn open__should_error_on_malformed_sealed_bytes_without_panicking() {
        let device = device_key(1);
        let commit = ContentKey::generate(&mut rng()).commitment();
        for bad in [&[][..], &[0u8; 5], &[0xFF; 80]] {
            assert!(ContentKey::open(bad, &device, &commit).is_err());
        }
    }

    #[test]
    fn decrypt__should_reject_tampered_ciphertext() {
        // Given
        let mut rng = rng();
        let key = ContentKey::generate(&mut rng);
        let mut body = key.encrypt(b"payload", &mut rng);

        // When
        let last = body.len() - 1;
        body[last] ^= 0x01;

        // Then
        assert_eq!(key.decrypt(&body).unwrap_err(), CryptoError::DecryptFailed);
    }

    #[test]
    fn decrypt__should_reject_a_wrong_key() {
        // Given
        let mut rng = rng();
        let body = ContentKey::generate(&mut rng).encrypt(b"payload", &mut rng);

        // When
        let result = ContentKey::generate(&mut rng).decrypt(&body);

        // Then
        assert_eq!(result.unwrap_err(), CryptoError::DecryptFailed);
    }

    #[test]
    fn decrypt__should_error_on_truncated_input_without_panicking() {
        let key = ContentKey::generate(&mut rng());
        for bad in [&[][..], &[0u8; 10], &[0u8; NONCE_SIZE]] {
            assert!(key.decrypt(bad).is_err());
        }
    }

    #[test]
    fn commitment__should_be_deterministic_per_key_and_differ_between_keys() {
        // Given
        let mut rng = rng();
        let (a, b) = (
            ContentKey::generate(&mut rng),
            ContentKey::generate(&mut rng),
        );

        // Then
        assert_eq!(a.commitment(), a.commitment());
        assert_ne!(a.commitment(), b.commitment());
    }

    #[test]
    fn encrypt__should_use_a_fresh_nonce_per_call() {
        // Given
        let mut rng = rng();
        let key = ContentKey::generate(&mut rng);

        // When: the same plaintext encrypted twice
        let (a, b) = (
            key.encrypt(b"same", &mut rng),
            key.encrypt(b"same", &mut rng),
        );

        // Then
        assert_ne!(a, b);
    }

    #[test]
    fn seal_for__should_error_on_an_invalid_recipient_key() {
        // Given: 0x02.. is not a valid compressed Edwards point.
        let mut rng = rng();
        let key = ContentKey::generate(&mut rng);

        // When
        let result = key.seal_for(&PublicKey([0x02; 32]), &mut rng);

        // Then
        assert_eq!(result.unwrap_err(), CryptoError::InvalidRecipientKey);
    }
}
