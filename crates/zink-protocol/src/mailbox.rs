//! Mailbox wire protocol: the request/response objects a client exchanges
//! with a relay (see `docs/design/mailbox-wire-protocol.md`). Pure data —
//! transport lives in the relay and client edges.

use borsh::{BorshDeserialize, BorshSerialize};

use crate::FORMAT_VERSION;
use crate::codec::{self, DecodeError};
use crate::message::{MessageEnvelope, MessageId};

/// ALPN for the mailbox protocol. The protocol generation lives here, so
/// incompatible speakers never exchange frames.
pub const MAILBOX_ALPN: &[u8] = b"zink-mailbox/1";

/// One request per bi-stream; caps enforced via `read_to_end` limits.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;
pub const MAX_RESPONSE_BYTES: usize = 16 << 20;

/// One mailbox operation. `register`/`fetch`/`ack` act on the mailbox of the
/// key that authenticated the connection — no other mailbox can be named.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct MailboxRequest {
    pub version: u16,
    pub op: MailboxOp,
}

impl MailboxRequest {
    pub fn new(op: MailboxOp) -> Self {
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
pub enum MailboxOp {
    /// Create or refresh the caller's mailbox.
    Register,
    /// Store an envelope for its recipients' mailboxes on this relay.
    /// (Boxed for variant-size balance; BORSH encodes `Box<T>` exactly as `T`.)
    Deposit { envelope: Box<MessageEnvelope> },
    /// Return envelopes deposited after `after` (0 = from the beginning).
    Fetch { after: u64 },
    /// Drop delivered envelopes with cursor ≤ `up_to`.
    Ack { up_to: u64 },
}

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct MailboxResponse {
    pub version: u16,
    pub result: MailboxResult,
}

impl MailboxResponse {
    pub fn new(result: MailboxResult) -> Self {
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
pub enum MailboxResult {
    Registered,
    /// Idempotency receipt: returned whether or not the deposit was new.
    Deposited {
        id: MessageId,
    },
    Envelopes {
        items: Vec<MailboxItem>,
    },
    Acked,
    Error {
        code: MailboxErrorCode,
    },
}

/// A fetched envelope with its relay-local cursor, so the client can ack
/// precisely. Real ordering is the DAG — the cursor is only a drain marker.
#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Eq, Debug)]
pub struct MailboxItem {
    pub cursor: u64,
    pub envelope: MessageEnvelope,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MailboxErrorCode {
    /// The request could not be decoded.
    Malformed,
    /// The relay failed internally; retrying is reasonable.
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

    #[test]
    fn request_roundtrip__should_decode_every_op_to_the_original() {
        // Given
        let ops = [
            MailboxOp::Register,
            MailboxOp::Deposit {
                envelope: Box::new(sample_envelope()),
            },
            MailboxOp::Fetch { after: 7 },
            MailboxOp::Ack { up_to: 9 },
        ];

        for op in ops {
            // When
            let request = MailboxRequest::new(op);
            let decoded = MailboxRequest::try_from_bytes(&request.to_bytes()).unwrap();

            // Then
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn response_roundtrip__should_decode_every_result_to_the_original() {
        // Given
        let results = [
            MailboxResult::Registered,
            MailboxResult::Deposited {
                id: sample_envelope().id(),
            },
            MailboxResult::Envelopes {
                items: vec![MailboxItem {
                    cursor: 3,
                    envelope: sample_envelope(),
                }],
            },
            MailboxResult::Acked,
            MailboxResult::Error {
                code: MailboxErrorCode::Malformed,
            },
        ];

        for result in results {
            // When
            let response = MailboxResponse::new(result);
            let decoded = MailboxResponse::try_from_bytes(&response.to_bytes()).unwrap();

            // Then
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn try_from_bytes__should_reject_an_unsupported_version() {
        // Given
        let mut bytes = MailboxRequest::new(MailboxOp::Register).to_bytes();
        bytes[0..2].copy_from_slice(&9u16.to_le_bytes());

        // When / Then
        assert_eq!(
            MailboxRequest::try_from_bytes(&bytes),
            Err(DecodeError::UnsupportedVersion { found: 9 })
        );
    }

    #[test]
    fn try_from_bytes__should_error_on_hostile_input_without_panicking() {
        let valid = MailboxRequest::new(MailboxOp::Fetch { after: 1 }).to_bytes();
        for len in [0, 1, 2, valid.len() - 1] {
            assert!(MailboxRequest::try_from_bytes(&valid[..len]).is_err());
        }
        let mut garbage = vec![1u8, 0u8];
        garbage.extend([0xFF; 32]);
        assert!(MailboxRequest::try_from_bytes(&garbage).is_err());
        assert!(MailboxResponse::try_from_bytes(&garbage).is_err());
    }
}
