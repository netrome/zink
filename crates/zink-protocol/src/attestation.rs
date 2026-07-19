//! Attestations: signed, advisory claims linking or naming keys (SPEC §3.2).

use borsh::{BorshDeserialize, BorshSerialize};

use crate::codec::{self, DecodeError};
use crate::keys::{self, DeviceKey, PublicKey, Signature, VerifyError};
use crate::message::BlobHash;

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

/// The claim an attester makes about a subject key. One primitive, several
/// uses: profiles, device linking, vouching, repudiation.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum Claim {
    Name(String),
    /// A profile picture: `hash` addresses the encrypted blob on relay
    /// caches; `key` decrypts it. The key may live here because claims
    /// travel only over voluntary E2E channels (QR, peer sync) — a relay
    /// holds ciphertext it cannot open, while the audience stays
    /// open-ended (who-is-this.md §8; field added in-place at v1, D1d).
    Avatar {
        hash: BlobHash,
        key: [u8; 32],
    },
    SamePersonAs(PublicKey),
    /// Active disavowal: "I do not / no longer recognise this key."
    Negative,
}

/// An attestation id: `BLAKE3(borsh(Attestation))`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AttestationId(pub [u8; 32]);

/// The evidence tier verified `same-person-as` links establish between a
/// trusted key set and a candidate key (multi-device.md §2/§7). Links are
/// asymmetric, and the tiers reflect it: the load-bearing direction is
/// trusted-key → candidate — a key you already trust vouching the new one
/// is unforgeable. The reverse direction alone (the candidate claiming a
/// trusted key) is the spoof direction and never clusters; both together
/// upgrade — consent-proof against vouching an unrelated victim's key.
/// Ordered so a client can compare evidence strength directly.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LinkTier {
    /// No verified vouch *from* trust — including the spoof direction
    /// standing alone, forged signatures, and third-party links.
    None,
    /// A trusted key vouches the candidate ("P says this is their device").
    VouchedFromTrust,
    /// Both directions verified ("mutually confirmed").
    MutuallyConfirmed,
}

/// Evaluate the link evidence for `candidate` against `trusted`, over
/// attestations aggregated from held records (the caller gathers stored +
/// learned; this stays pure). A link counts only if its signature verifies
/// and it is genuinely self-attested (`attester == subject` — "I, K, am the
/// same person as L"); anything else is inert, never a downgrade. `Negative`
/// supersession is deferred with the recovery flows (D4).
pub fn link_tier(
    trusted: &[PublicKey],
    candidate: PublicKey,
    attestations: &[SignedAttestation],
) -> LinkTier {
    let links: Vec<(PublicKey, PublicKey)> =
        attestations.iter().filter_map(verified_self_link).collect();
    let vouched = links
        .iter()
        .any(|(from, to)| trusted.contains(from) && *to == candidate);
    if !vouched {
        return LinkTier::None;
    }
    let confirmed_back = links
        .iter()
        .any(|(from, to)| *from == candidate && trusted.contains(to));
    if confirmed_back {
        LinkTier::MutuallyConfirmed
    } else {
        LinkTier::VouchedFromTrust
    }
}

/// A verified, self-attested `SamePersonAs` as `(attester, linked key)`;
/// `None` for anything else — forged, tampered, or third-party claims are
/// inert evidence, never errors (the inputs came from untrusted channels).
fn verified_self_link(signed: &SignedAttestation) -> Option<(PublicKey, PublicKey)> {
    let attestation = &signed.attestation;
    let Claim::SamePersonAs(linked) = attestation.claim else {
        return None;
    };
    (attestation.attester == attestation.subject && signed.verify().is_ok())
        .then_some((attestation.attester, linked))
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
            Claim::Avatar {
                hash: BlobHash([0; 32]),
                key: [0; 32],
            },
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

    /// "I, `attester`, am the same person as `linked`" — signed by `signer`
    /// (pass a non-attester to forge).
    fn link(attester: &DeviceKey, linked: PublicKey, signer: &DeviceKey) -> SignedAttestation {
        SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: attester.public(),
                subject: attester.public(),
                claim: Claim::SamePersonAs(linked),
                revision: 0,
            },
            signer,
        )
    }

    #[test]
    fn link_tier__should_tier_a_trusted_keys_vouch_as_vouched_from_trust() {
        // Given: trusted phone vouches the new laptop key
        let phone = device_key(1);
        let laptop = device_key(2);
        let links = [link(&phone, laptop.public(), &phone)];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], laptop.public(), &links),
            LinkTier::VouchedFromTrust
        );
    }

    #[test]
    fn link_tier__should_upgrade_to_mutually_confirmed_when_both_directions_verify() {
        // Given
        let phone = device_key(1);
        let laptop = device_key(2);
        let links = [
            link(&phone, laptop.public(), &phone),
            link(&laptop, phone.public(), &laptop),
        ];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], laptop.public(), &links),
            LinkTier::MutuallyConfirmed
        );
    }

    #[test]
    fn link_tier__should_tier_the_reverse_direction_alone_as_nothing() {
        // Given: only the spoof direction — a stranger claiming the trusted key
        let phone = device_key(1);
        let stranger = device_key(2);
        let links = [link(&stranger, phone.public(), &stranger)];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], stranger.public(), &links),
            LinkTier::None
        );
    }

    #[test]
    fn link_tier__should_ignore_a_forged_vouch() {
        // Given: a vouch naming the trusted key as attester, signed by the forger
        let phone = device_key(1);
        let forger = device_key(2);
        let links = [link(&phone, forger.public(), &forger)];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], forger.public(), &links),
            LinkTier::None
        );
    }

    #[test]
    fn link_tier__should_ignore_a_forged_confirmation_when_upgrading() {
        // Given: a real vouch, and a *forged* reverse link — must stay one-way
        let phone = device_key(1);
        let laptop = device_key(2);
        let forger = device_key(3);
        let links = [
            link(&phone, laptop.public(), &phone),
            link(&laptop, phone.public(), &forger), // signature won't verify
        ];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], laptop.public(), &links),
            LinkTier::VouchedFromTrust
        );
    }

    #[test]
    fn link_tier__should_ignore_third_party_links_and_links_to_other_keys() {
        // Given: a link *about* someone else (attester ≠ subject) and a
        // trusted vouch for an unrelated key
        let phone = device_key(1);
        let laptop = device_key(2);
        let other = device_key(3);
        let third_party = SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: phone.public(),
                subject: other.public(), // not self-attested
                claim: Claim::SamePersonAs(laptop.public()),
                revision: 0,
            },
            &phone,
        );
        let links = [third_party, link(&phone, other.public(), &phone)];

        // When / Then
        assert_eq!(
            link_tier(&[phone.public()], laptop.public(), &links),
            LinkTier::None
        );
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
