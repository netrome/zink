//! Attestations: signed, advisory claims linking or naming keys (SPEC §3.2).

use borsh::{BorshDeserialize, BorshSerialize};

use crate::codec::{self, DecodeError};
use crate::keys::{self, DeviceKey, PublicKey, Signature, VerifyError};
use crate::message::BlobHash;

/// An attestation id: `BLAKE3(borsh(Attestation))`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AttestationId(pub [u8; 32]);

/// The claim an attester makes about a subject key. One primitive, several
/// uses: profiles, device linking, vouching, repudiation.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum Claim {
    Name(String),
    Avatar(BlobHash),
    SamePersonAs(PublicKey),
    /// Active disavowal: "I do not / no longer recognise this key."
    Negative,
}

/// The hashed, signed content of an attestation. Advisory input, never a
/// global fact — clients weigh it by their trust in the attester.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct Attestation {
    pub version: u16,
    pub attester: PublicKey,
    pub subject: PublicKey,
    pub claim: Claim,
    /// Supersession counter: highest revision wins, scoped per
    /// `(attester, subject, claim-kind, linked key)` (SPEC §3.2).
    pub revision: u64,
}

impl Attestation {
    pub fn id(&self) -> AttestationId {
        AttestationId(codec::content_hash(self))
    }
}

/// An attestation plus its attester's signature — the wire object.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct SignedAttestation {
    pub attestation: Attestation,
    /// Ed25519 by `attestation.attester` over the id.
    pub sig: Signature,
}

impl SignedAttestation {
    pub fn new(attestation: Attestation, attester_key: &DeviceKey) -> Self {
        let sig = attester_key.sign_hash(&attestation.id().0);
        Self { attestation, sig }
    }

    /// Check the attester's signature over the recomputed id.
    pub fn verify(&self) -> Result<(), VerifyError> {
        keys::verify_hash(
            &self.attestation.attester,
            &self.attestation.id().0,
            &self.sig,
        )
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        codec::canonical_bytes(self)
    }

    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        // `attestation.version` is the first encoded field, so the leading
        // version peek covers the wrapper too.
        codec::decode_versioned(bytes)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::FORMAT_VERSION;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn sample_attestation(attester: &DeviceKey) -> Attestation {
        Attestation {
            version: FORMAT_VERSION,
            attester: attester.public(),
            subject: device_key(9).public(),
            claim: Claim::Name("Alice".to_string()),
            revision: 3,
        }
    }

    #[test]
    fn attestation_id__should_be_deterministic_for_equal_values() {
        // Given
        let attester = device_key(1);
        let (a, b) = (sample_attestation(&attester), sample_attestation(&attester));

        // Then
        assert_eq!(codec::canonical_bytes(&a), codec::canonical_bytes(&b));
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn attestation_id__should_differ_between_claim_kinds() {
        // Given: identical attestations except for the claim
        let attester = device_key(1);
        let base = sample_attestation(&attester);
        let claims = [
            Claim::Name("Alice".to_string()),
            Claim::Avatar(BlobHash([0; 32])),
            Claim::SamePersonAs(device_key(9).public()),
            Claim::Negative,
        ];

        // When
        let ids: Vec<_> = claims
            .into_iter()
            .map(|claim| {
                Attestation {
                    claim,
                    ..base.clone()
                }
                .id()
            })
            .collect();

        // Then: all distinct
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn signed_attestation_roundtrip__should_decode_to_the_original() {
        // Given
        let attester = device_key(1);
        let signed = SignedAttestation::new(sample_attestation(&attester), &attester);

        // When
        let decoded = SignedAttestation::try_from_bytes(&signed.to_bytes()).unwrap();

        // Then
        assert_eq!(decoded, signed);
    }

    #[test]
    fn signed_attestation_verify__should_accept_a_valid_signature() {
        let attester = device_key(1);
        let signed = SignedAttestation::new(sample_attestation(&attester), &attester);
        assert_eq!(signed.verify(), Ok(()));
    }

    #[test]
    fn signed_attestation_verify__should_reject_a_signature_by_a_non_attester_key() {
        // Given: attestation names device 1 as attester, device 2 signs
        let claimed_attester = device_key(1);
        let signed = SignedAttestation::new(sample_attestation(&claimed_attester), &device_key(2));

        // Then
        assert_eq!(signed.verify(), Err(VerifyError::BadSignature));
    }

    #[test]
    fn signed_attestation_verify__should_reject_a_tampered_claim() {
        // Given
        let attester = device_key(1);
        let mut signed = SignedAttestation::new(sample_attestation(&attester), &attester);

        // When
        signed.attestation.revision += 1;

        // Then
        assert_eq!(signed.verify(), Err(VerifyError::BadSignature));
    }

    #[test]
    fn try_from_bytes__should_reject_an_unsupported_version() {
        // Given
        let attester = device_key(1);
        let mut bytes = SignedAttestation::new(sample_attestation(&attester), &attester).to_bytes();
        bytes[0..2].copy_from_slice(&7u16.to_le_bytes());

        // When / Then
        assert_eq!(
            SignedAttestation::try_from_bytes(&bytes),
            Err(DecodeError::UnsupportedVersion { found: 7 })
        );
    }

    #[test]
    fn try_from_bytes__should_error_on_truncated_input_without_panicking() {
        let attester = device_key(1);
        let bytes = SignedAttestation::new(sample_attestation(&attester), &attester).to_bytes();
        for len in [0, 1, 2, bytes.len() / 2, bytes.len() - 1] {
            assert!(SignedAttestation::try_from_bytes(&bytes[..len]).is_err());
        }
    }
}
