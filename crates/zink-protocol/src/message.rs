//! Messages: the signed, hashed core and its transport envelope (SPEC §4.1).

use borsh::{BorshDeserialize, BorshSerialize};

use crate::FORMAT_VERSION;
use crate::codec::{self, DecodeError};
use crate::keys::{self, DeviceKey, PublicKey, Signature, VerifyError};

/// The signed, hashed core — identical bytes for every recipient, so everyone
/// derives the same id and the DAG holds across recipients.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct MessageCore {
    pub version: u16,
    /// Genesis id of the conversation; `None` in the genesis itself.
    pub conversation: Option<MessageId>,
    /// The sender's known heads at send time; empty in the genesis.
    pub parents: Vec<MessageId>,
    /// Who this was fanned out to. Advisory, but signed.
    pub recipients: Vec<PublicKey>,
    pub sender: PublicKey,
    /// Per `(sender, conversation)`, 0-based (sender's first message = 0).
    pub seq: u64,
    /// Lamport clock: `1 + max(parents.logical)`; 0 in the genesis.
    pub logical: u64,
    /// Wall-clock hint, display only — never trusted for ordering.
    pub timestamp_ms: u64,
    /// Body ciphertext, encrypted once with a random content-key.
    pub body: Vec<u8>,
    pub key_commit: KeyCommitment,
    pub blob_refs: Vec<BlobRef>,
}

impl MessageCore {
    pub fn id(&self) -> MessageId {
        MessageId(codec::content_hash(self))
    }
}

/// The unit of delivery. Per-recipient parts live here, *outside* the hashed
/// core, so all recipients share one message id.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct MessageEnvelope {
    pub version: u16,
    pub core: MessageCore,
    /// Ed25519 by `core.sender` over the id.
    pub sig: Signature,
    pub key_wraps: Vec<KeyWrap>,
}

impl MessageEnvelope {
    /// Sign `core` and wrap it for transport. Key-wraps are filled by the
    /// encryption layer (slice A3).
    pub fn new(core: MessageCore, sender_key: &DeviceKey) -> Self {
        let sig = sender_key.sign_hash(&core.id().0);
        Self {
            version: FORMAT_VERSION,
            core,
            sig,
            key_wraps: Vec::new(),
        }
    }

    pub fn id(&self) -> MessageId {
        self.core.id()
    }

    /// Check the sender's signature over the recomputed id.
    pub fn verify(&self) -> Result<(), VerifyError> {
        keys::verify_hash(&self.core.sender, &self.core.id().0, &self.sig)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        codec::canonical_bytes(self)
    }

    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let envelope: Self = codec::decode_versioned(bytes)?;
        // The envelope and core versions evolve independently (SPEC §4.1);
        // check the inner tag too so a future core is surfaced, not misparsed.
        if envelope.core.version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion {
                found: envelope.core.version,
            });
        }
        Ok(envelope)
    }
}

/// A message id: `BLAKE3(borsh(MessageCore))`. Content address and DAG node id.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MessageId(pub [u8; 32]);

/// Commitment to a content-key, carried inside the hashed core so "same id ⇒
/// same content" holds even though AEADs are not key-committing (SPEC §6).
/// Computed and verified in the encryption layer (slice A3).
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyCommitment(pub [u8; 32]);

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlobRef {
    pub hash: BlobHash,
    pub kind: BlobKind,
    pub key_commit: KeyCommitment,
}

/// BLAKE3 hash of an encrypted blob (SPEC §7).
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlobHash(pub [u8; 32]);

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlobKind {
    Thumbnail,
    Full,
}

/// All sealed content-keys for one recipient.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct KeyWrap {
    pub recipient: PublicKey,
    pub sealed: Vec<SealedKey>,
}

/// One wrapped content-key for one encrypted object.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct SealedKey {
    pub object: SealedRef,
    pub sealed_key: Vec<u8>,
}

/// What a sealed content-key decrypts: the body, or one of the blobs.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum SealedRef {
    Body,
    Blob(BlobHash),
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn sample_core(sender: &DeviceKey) -> MessageCore {
        MessageCore {
            version: FORMAT_VERSION,
            conversation: Some(MessageId([1; 32])),
            parents: vec![MessageId([2; 32]), MessageId([3; 32])],
            recipients: vec![device_key(9).public()],
            sender: sender.public(),
            seq: 4,
            logical: 7,
            timestamp_ms: 1_700_000_000_000,
            body: vec![0xAA, 0xBB, 0xCC],
            key_commit: KeyCommitment([5; 32]),
            blob_refs: vec![BlobRef {
                hash: BlobHash([6; 32]),
                kind: BlobKind::Thumbnail,
                key_commit: KeyCommitment([7; 32]),
            }],
        }
    }

    #[test]
    fn message_core_id__should_be_deterministic_for_equal_values() {
        // Given
        let sender = device_key(1);
        let (a, b) = (sample_core(&sender), sample_core(&sender));

        // Then: same value → same bytes → same id
        assert_eq!(codec::canonical_bytes(&a), codec::canonical_bytes(&b));
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn message_core_id__should_change_when_any_core_field_changes() {
        // Given
        let sender = device_key(1);
        let base = sample_core(&sender);

        // When
        let mut changed = base.clone();
        changed.seq += 1;

        // Then
        assert_ne!(base.id(), changed.id());
    }

    #[test]
    fn message_core_id__should_match_the_pinned_golden_value() {
        // Regression pin for canonical encoding: a field reorder, type change, or
        // BORSH behavior change shows up as a different id (content-addressing
        // invariant — must never regress without a version bump).
        let id = sample_core(&device_key(1)).id();
        let hex: String = id.0.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "3b439775941fd9de2b5b509e4a1a886c41644d79e3b34a2491c76b261cc424e7"
        );
    }

    #[test]
    fn envelope_id__should_ignore_key_wraps() {
        // Given
        let sender = device_key(1);
        let mut a = MessageEnvelope::new(sample_core(&sender), &sender);
        let b = MessageEnvelope::new(sample_core(&sender), &sender);

        // When: a gains a wrap, b stays empty
        a.key_wraps.push(KeyWrap {
            recipient: device_key(9).public(),
            sealed: vec![SealedKey {
                object: SealedRef::Body,
                sealed_key: vec![1, 2, 3],
            }],
        });

        // Then
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn envelope_roundtrip__should_decode_to_the_original() {
        // Given
        let sender = device_key(1);
        let mut envelope = MessageEnvelope::new(sample_core(&sender), &sender);
        envelope.key_wraps.push(KeyWrap {
            recipient: device_key(9).public(),
            sealed: vec![SealedKey {
                object: SealedRef::Blob(BlobHash([6; 32])),
                sealed_key: vec![4, 5, 6],
            }],
        });

        // When
        let decoded = MessageEnvelope::try_from_bytes(&envelope.to_bytes()).unwrap();

        // Then
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn envelope_verify__should_accept_a_valid_signature() {
        let sender = device_key(1);
        let envelope = MessageEnvelope::new(sample_core(&sender), &sender);
        assert_eq!(envelope.verify(), Ok(()));
    }

    #[test]
    fn envelope_verify__should_reject_a_tampered_core() {
        // Given
        let sender = device_key(1);
        let mut envelope = MessageEnvelope::new(sample_core(&sender), &sender);

        // When
        envelope.core.body = vec![0xEE];

        // Then
        assert_eq!(envelope.verify(), Err(VerifyError::BadSignature));
    }

    #[test]
    fn envelope_verify__should_reject_a_signature_by_a_key_other_than_sender() {
        // Given: core claims device 1 as sender, but device 2 signs
        let claimed_sender = device_key(1);
        let envelope = MessageEnvelope::new(sample_core(&claimed_sender), &device_key(2));

        // Then
        assert_eq!(envelope.verify(), Err(VerifyError::BadSignature));
    }

    #[test]
    fn try_from_bytes__should_reject_an_unsupported_version() {
        // Given: valid bytes with the leading version tag bumped
        let sender = device_key(1);
        let mut bytes = MessageEnvelope::new(sample_core(&sender), &sender).to_bytes();
        bytes[0..2].copy_from_slice(&99u16.to_le_bytes());

        // When
        let result = MessageEnvelope::try_from_bytes(&bytes);

        // Then
        assert_eq!(result, Err(DecodeError::UnsupportedVersion { found: 99 }));
    }

    #[test]
    fn try_from_bytes__should_error_on_truncated_input() {
        let sender = device_key(1);
        let bytes = MessageEnvelope::new(sample_core(&sender), &sender).to_bytes();
        for len in [0, 1, 2, bytes.len() / 2, bytes.len() - 1] {
            assert!(MessageEnvelope::try_from_bytes(&bytes[..len]).is_err());
        }
    }

    #[test]
    fn try_from_bytes__should_error_on_trailing_bytes() {
        let sender = device_key(1);
        let mut bytes = MessageEnvelope::new(sample_core(&sender), &sender).to_bytes();
        bytes.push(0);
        assert_eq!(
            MessageEnvelope::try_from_bytes(&bytes),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn try_from_bytes__should_error_on_garbage_without_panicking() {
        // Version tag valid, rest hostile — must return an error, never panic.
        let mut bytes = vec![1u8, 0u8];
        bytes.extend([0xFF; 64]);
        assert!(MessageEnvelope::try_from_bytes(&bytes).is_err());
    }
}
