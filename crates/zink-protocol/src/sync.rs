//! Sync wire protocol (D0): the request/response objects a client exchanges
//! with a *peer* to pull history — `get` / `get-successors` (SPEC §5.2, and
//! `docs/design/sync-primitives.md`). Pure data — transport lives in the
//! client edge. Distinct ALPN from the mailbox: peers speak this to each
//! other, never to a relay.

use borsh::{BorshDeserialize, BorshSerialize};

use crate::FORMAT_VERSION;
use crate::codec::{self, DecodeError};
use crate::contact_record::ContactRecord;
use crate::keys::PublicKey;
use crate::message::{MessageEnvelope, MessageId};

/// ALPN for the peer sync protocol. The generation lives here, so
/// incompatible speakers never exchange frames.
pub const SYNC_ALPN: &[u8] = b"zink-sync/1";

/// One request per bi-stream; caps enforced via `read_to_end` limits. A `Get`
/// returns a single envelope, so the response cap mirrors the mailbox's
/// per-envelope headroom rather than a full-mailbox page.
pub const MAX_SYNC_REQUEST_BYTES: usize = 1 << 10;
pub const MAX_SYNC_RESPONSE_BYTES: usize = 16 << 20;

/// One sync operation, addressed by content id. Served at the peer's
/// discretion (SPEC §5.2): a peer answers what it holds and chooses to share.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct SyncRequest {
    pub version: u16,
    pub op: SyncOp,
}

impl SyncRequest {
    pub fn new(op: SyncOp) -> Self {
        Self {
            version: FORMAT_VERSION,
            op,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        codec::canonical_bytes(self)
    }

    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        codec::decode_versioned(bytes)
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum SyncOp {
    /// Fetch a message by id (the DAG skeleton + ciphertext; the requester
    /// verifies authorship and content-addressing, and decrypts only what it
    /// holds a key-wrap for).
    Get { id: MessageId },
    /// Ids of held messages whose `parents` include `id` — pull forward.
    GetSuccessors { id: MessageId },
    /// Identity discovery (SPEC §3.5, D1): "who is this key?" Answered with
    /// the responder's stored record for it — or `NotHeld`, at discretion.
    /// (Appended so existing BORSH variant tags stay stable.)
    WhoIs { key: PublicKey },
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct SyncResponse {
    pub version: u16,
    pub result: SyncResult,
}

impl SyncResponse {
    pub fn new(result: SyncResult) -> Self {
        Self {
            version: FORMAT_VERSION,
            result,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        codec::canonical_bytes(self)
    }

    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        codec::decode_versioned(bytes)
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub enum SyncResult {
    /// `Get` hit. (Boxed for variant-size balance; BORSH encodes `Box<T>` as `T`.)
    Envelope {
        envelope: Box<MessageEnvelope>,
    },
    /// `Get` miss, or the peer declined to serve this id. Also the `WhoIs`
    /// miss: not-knowing and declining are indistinguishable (SPEC §5.2).
    NotHeld,
    /// `GetSuccessors` — known children (possibly empty).
    Successors {
        ids: Vec<MessageId>,
    },
    Error {
        code: SyncErrorCode,
    },
    /// `WhoIs` hit: the responder's stored record — the subject's signed
    /// self-claims relayed verbatim, which the requester verifies like a
    /// scanned QR. (Appended so existing BORSH variant tags stay stable.)
    Known {
        record: Box<ContactRecord>,
    },
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncErrorCode {
    /// The request could not be decoded.
    Malformed,
    /// The peer failed internally; retrying is reasonable.
    Internal,
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::keys::DeviceKey;
    use crate::message::{KeyCommitment, MessageCore};

    fn sample_envelope() -> MessageEnvelope {
        let sender = DeviceKey::from_seed([1; 32]);
        let core = MessageCore {
            version: FORMAT_VERSION,
            conversation: None,
            parents: vec![],
            recipients: vec![DeviceKey::from_seed([2; 32]).public()],
            sender: sender.public(),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: vec![1, 2, 3],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        };
        MessageEnvelope::new(core, &sender)
    }

    /// A record with every field populated, so the `Known` round-trip
    /// exercises the nested attestation + relay-entry decode.
    fn sample_record() -> ContactRecord {
        use crate::attestation::{Attestation, Claim, SignedAttestation};
        use crate::contact_record::RelayEntry;
        let subject = DeviceKey::from_seed([4; 32]);
        let attestation = SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: subject.public(),
                subject: subject.public(),
                claim: Claim::Name("Carol".to_string()),
                revision: 1,
            },
            &subject,
        );
        ContactRecord::new(
            vec![subject.public()],
            vec![attestation],
            vec![RelayEntry {
                mailbox: "aa@203.0.113.1:1".to_string(),
                relay_url: Some("http://203.0.113.1:2".to_string()),
            }],
        )
    }

    #[test]
    fn request_roundtrip__should_decode_every_op_to_the_original() {
        // Given
        let ops = [
            SyncOp::Get {
                id: sample_envelope().id(),
            },
            SyncOp::GetSuccessors {
                id: sample_envelope().id(),
            },
            SyncOp::WhoIs {
                key: DeviceKey::from_seed([3; 32]).public(),
            },
        ];

        for op in ops {
            // When
            let request = SyncRequest::new(op);
            let decoded = SyncRequest::try_from_bytes(&request.to_bytes()).unwrap();

            // Then
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn response_roundtrip__should_decode_every_result_to_the_original() {
        // Given
        let results = [
            SyncResult::Envelope {
                envelope: Box::new(sample_envelope()),
            },
            SyncResult::NotHeld,
            SyncResult::Successors {
                ids: vec![sample_envelope().id()],
            },
            SyncResult::Error {
                code: SyncErrorCode::Malformed,
            },
            SyncResult::Known {
                record: Box::new(sample_record()),
            },
        ];

        for result in results {
            // When
            let response = SyncResponse::new(result);
            let decoded = SyncResponse::try_from_bytes(&response.to_bytes()).unwrap();

            // Then
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn try_from_bytes__should_reject_an_unsupported_version() {
        // Given
        let mut bytes = SyncRequest::new(SyncOp::Get {
            id: sample_envelope().id(),
        })
        .to_bytes();
        bytes[0..2].copy_from_slice(&9u16.to_le_bytes());

        // When / Then
        assert_eq!(
            SyncRequest::try_from_bytes(&bytes),
            Err(DecodeError::UnsupportedVersion { found: 9 })
        );
    }

    #[test]
    fn try_from_bytes__should_error_on_hostile_input_without_panicking() {
        let valid = SyncRequest::new(SyncOp::Get {
            id: sample_envelope().id(),
        })
        .to_bytes();
        for len in [0, 1, 2, valid.len() - 1] {
            assert!(SyncRequest::try_from_bytes(&valid[..len]).is_err());
        }
        let mut garbage = vec![1u8, 0u8];
        garbage.extend([0xFF; 32]);
        assert!(SyncRequest::try_from_bytes(&garbage).is_err());
        assert!(SyncResponse::try_from_bytes(&garbage).is_err());
    }
}
