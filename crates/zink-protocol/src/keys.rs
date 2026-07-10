//! Ed25519 device keys — the only identifiers in the protocol (tenet 1).

use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::Signer;

/// A device's public key. One key = one device (SPEC §2).
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PublicKey(pub [u8; 32]);

/// An Ed25519 signature over a 32-byte content id.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signature(pub [u8; 64]);

/// A device's signing key.
///
/// Constructed from caller-supplied entropy: the core is pure, so randomness is
/// the caller's job (I/O at the edges).
pub struct DeviceKey(ed25519_dalek::SigningKey);

impl DeviceKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    pub(crate) fn sign_hash(&self, hash: &[u8; 32]) -> Signature {
        Signature(self.0.sign(hash).to_bytes())
    }

    /// The X25519 secret for this device: the clamped scalar half of the
    /// expanded Ed25519 secret (standard conversion, one key / two uses —
    /// SPEC §6). Vetted dalek code path, never hand-rolled.
    pub(crate) fn x25519_secret_bytes(&self) -> [u8; 32] {
        self.0.to_scalar_bytes()
    }
}

/// Signature verification failure. Never a panic, even on hostile bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The public key bytes are not a valid Ed25519 point.
    InvalidKey,
    /// The signature does not verify against the key and content.
    BadSignature,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey => write!(f, "invalid public key"),
            Self::BadSignature => write!(f, "signature verification failed"),
        }
    }
}

impl std::error::Error for VerifyError {}

pub(crate) fn verify_hash(
    key: &PublicKey,
    hash: &[u8; 32],
    sig: &Signature,
) -> Result<(), VerifyError> {
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&key.0).map_err(|_| VerifyError::InvalidKey)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
    // verify_strict rejects malleable / mixed-order-point signatures that plain
    // `verify` accepts; ids must have exactly one valid signature encoding.
    key.verify_strict(hash, &sig)
        .map_err(|_| VerifyError::BadSignature)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    #[test]
    fn from_seed__should_derive_the_same_public_key_for_the_same_seed() {
        assert_eq!(device_key(7).public(), device_key(7).public());
    }

    #[test]
    fn from_seed__should_derive_different_public_keys_for_different_seeds() {
        assert_ne!(device_key(1).public(), device_key(2).public());
    }

    #[test]
    fn verify_hash__should_accept_a_valid_signature() {
        // Given
        let key = device_key(1);
        let hash = [42u8; 32];

        // When
        let sig = key.sign_hash(&hash);

        // Then
        assert_eq!(verify_hash(&key.public(), &hash, &sig), Ok(()));
    }

    #[test]
    fn verify_hash__should_reject_a_signature_by_another_key() {
        // Given
        let hash = [42u8; 32];
        let sig = device_key(1).sign_hash(&hash);

        // When
        let result = verify_hash(&device_key(2).public(), &hash, &sig);

        // Then
        assert_eq!(result, Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_hash__should_reject_a_signature_over_different_content() {
        // Given
        let key = device_key(1);
        let sig = key.sign_hash(&[42u8; 32]);

        // When
        let result = verify_hash(&key.public(), &[43u8; 32], &sig);

        // Then
        assert_eq!(result, Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_hash__should_error_on_invalid_public_key_bytes() {
        // Given: 0x02.. is not a valid compressed Edwards point (its x² is
        // non-square, so decompression fails).
        let bad_key = PublicKey([0x02; 32]);
        let sig = device_key(1).sign_hash(&[42u8; 32]);

        // When
        let result = verify_hash(&bad_key, &[42u8; 32], &sig);

        // Then
        assert_eq!(result, Err(VerifyError::InvalidKey));
    }
}
