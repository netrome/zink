//! Messages: the signed, hashed core and its transport envelope (SPEC §4.1).

use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use rand_core::CryptoRngCore;

use crate::FORMAT_VERSION;
use crate::codec::{self, DecodeError};
use crate::crypto::{ContentKey, CryptoError};
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

/// The plaintext precursor of a message: everything the sender chooses,
/// before encryption. `seal` turns it into a deliverable envelope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MessageDraft {
    pub conversation: Option<MessageId>,
    pub parents: Vec<MessageId>,
    pub recipients: Vec<PublicKey>,
    pub seq: u64,
    pub logical: u64,
    pub timestamp_ms: u64,
    pub plaintext: Vec<u8>,
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
    /// Encrypt and package a draft: fresh content-key, body encrypted once,
    /// key committed in the core, key sealed per recipient, core signed.
    pub fn seal(
        draft: MessageDraft,
        sender: &DeviceKey,
        rng: &mut impl CryptoRngCore,
    ) -> Result<Self, CryptoError> {
        let content_key = ContentKey::generate(rng);
        let key_wraps = draft
            .recipients
            .iter()
            .map(|recipient| {
                Ok(KeyWrap {
                    recipient: *recipient,
                    sealed: vec![SealedKey {
                        object: SealedRef::Body,
                        sealed_key: content_key.seal_for(recipient, rng)?,
                    }],
                })
            })
            .collect::<Result<Vec<_>, CryptoError>>()?;
        let core = MessageCore {
            version: FORMAT_VERSION,
            conversation: draft.conversation,
            parents: draft.parents,
            recipients: draft.recipients,
            sender: sender.public(),
            seq: draft.seq,
            logical: draft.logical,
            timestamp_ms: draft.timestamp_ms,
            body: content_key.encrypt(&draft.plaintext, rng),
            key_commit: content_key.commitment(),
            blob_refs: vec![],
        };
        let mut envelope = Self::new(core, sender);
        envelope.key_wraps = key_wraps;
        Ok(envelope)
    }

    /// Verify the signature, unseal this device's content-key (checked
    /// against the core's commitment), decrypt the body.
    pub fn open(&self, device: &DeviceKey) -> Result<Vec<u8>, OpenError> {
        self.verify().map_err(OpenError::Signature)?;
        let wrap = self
            .key_wraps
            .iter()
            .find(|wrap| wrap.recipient == device.public())
            .ok_or(OpenError::NotARecipient)?;
        let sealed = wrap
            .sealed
            .iter()
            .find(|sealed| sealed.object == SealedRef::Body)
            .ok_or(OpenError::MissingBodyKey)?;
        let content_key = ContentKey::open(&sealed.sealed_key, device, &self.core.key_commit)
            .map_err(OpenError::Crypto)?;
        content_key
            .decrypt(&self.core.body)
            .map_err(OpenError::Crypto)
    }

    /// Sign `core` and wrap it for transport. `seal` is the high-level
    /// entry; this is the building block beneath it.
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

/// Why an envelope could not be opened. Never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// The sender's signature over the id does not verify.
    Signature(VerifyError),
    /// No key-wrap addressed to this device.
    NotARecipient,
    /// This device's wrap has no sealed key for the body.
    MissingBodyKey,
    /// Unsealing, the commitment check, or decryption failed.
    Crypto(CryptoError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signature(e) => write!(f, "signature check failed: {e}"),
            Self::NotARecipient => write!(f, "no key-wrap for this device"),
            Self::MissingBodyKey => write!(f, "no sealed content-key for the body"),
            Self::Crypto(e) => write!(f, "could not decrypt: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// A message id: `BLAKE3(borsh(MessageCore))`. Content address and DAG node id.
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug,
)]
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
    use crate::testutil::rng;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn draft_to(recipients: Vec<PublicKey>, plaintext: &[u8]) -> MessageDraft {
        MessageDraft {
            conversation: None,
            parents: vec![],
            recipients,
            seq: 0,
            logical: 0,
            timestamp_ms: 1_700_000_000_000,
            plaintext: plaintext.to_vec(),
        }
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
    fn seal_open__should_roundtrip_for_every_recipient() {
        // Given
        let mut rng = rng();
        let sender = device_key(1);
        let recipients: Vec<DeviceKey> = (2..=4).map(device_key).collect();
        let keys: Vec<PublicKey> = recipients.iter().map(|r| r.public()).collect();

        // When
        let envelope =
            MessageEnvelope::seal(draft_to(keys, b"hello, spine"), &sender, &mut rng).unwrap();

        // Then
        for recipient in &recipients {
            assert_eq!(envelope.open(recipient).unwrap(), b"hello, spine");
        }
    }

    #[test]
    fn open__should_fail_for_a_device_that_was_not_a_recipient() {
        // Given
        let mut rng = rng();
        let sender = device_key(1);
        let draft = draft_to(vec![device_key(2).public()], b"private");

        // When
        let envelope = MessageEnvelope::seal(draft, &sender, &mut rng).unwrap();

        // Then
        assert_eq!(
            envelope.open(&device_key(3)).unwrap_err(),
            OpenError::NotARecipient
        );
    }

    #[test]
    fn open__should_reject_a_tampered_body_via_the_signature() {
        // Given
        let mut rng = rng();
        let sender = device_key(1);
        let recipient = device_key(2);
        let draft = draft_to(vec![recipient.public()], b"original");
        let mut envelope = MessageEnvelope::seal(draft, &sender, &mut rng).unwrap();

        // When
        let last = envelope.core.body.len() - 1;
        envelope.core.body[last] ^= 0x01;

        // Then
        assert!(matches!(
            envelope.open(&recipient).unwrap_err(),
            OpenError::Signature(_)
        ));
    }

    #[test]
    fn open__should_reject_a_key_wrap_swapped_in_from_another_message() {
        // Given: two messages to the same recipient — wraps live outside the
        // signed core, so a swap passes the signature check…
        let mut rng = rng();
        let sender = device_key(1);
        let recipient = device_key(2);
        let mut first = MessageEnvelope::seal(
            draft_to(vec![recipient.public()], b"first"),
            &sender,
            &mut rng,
        )
        .unwrap();
        let second = MessageEnvelope::seal(
            draft_to(vec![recipient.public()], b"second"),
            &sender,
            &mut rng,
        )
        .unwrap();

        // When
        first.key_wraps = second.key_wraps.clone();

        // Then: …and is caught by the key commitment instead.
        assert_eq!(
            first.open(&recipient).unwrap_err(),
            OpenError::Crypto(CryptoError::CommitmentMismatch)
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
