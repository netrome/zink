//! Pure protocol core: types, canonical encoding, hashing, DAG, crypto.
//!
//! No I/O, no network, no async runtime — data in, data out.
//! See `docs/SPEC.md` and `docs/STYLE.md`.

mod attestation;
mod codec;
mod contact_record;
mod crypto;
mod dag;
mod fanout;
mod keys;
mod mailbox;
mod message;
mod sync;
#[cfg(test)]
mod testutil;

pub use attestation::{Attestation, AttestationId, Claim, SignedAttestation};
pub use codec::DecodeError;
pub use contact_record::{ContactRecord, RelayEntry};
pub use crypto::{ContentKey, CryptoError};
pub use dag::{ConversationDag, DagError, InsertOutcome};
pub use fanout::distinct_relays;
pub use keys::{DeviceKey, PublicKey, Signature, VerifyError};
pub use mailbox::{
    MAILBOX_ALPN, MAX_FETCH_PAGE_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, MailboxErrorCode,
    MailboxItem, MailboxOp, MailboxRequest, MailboxResponse, MailboxResult,
};
pub use message::{
    BlobDraft, BlobHash, BlobKind, BlobRef, EncryptedBlob, KeyCommitment, KeyWrap, MessageCore,
    MessageDraft, MessageEnvelope, MessageId, OpenError, SealedKey, SealedMessage, SealedRef,
};
pub use sync::{
    MAX_SYNC_REQUEST_BYTES, MAX_SYNC_RESPONSE_BYTES, SYNC_ALPN, SyncErrorCode, SyncOp, SyncRequest,
    SyncResponse, SyncResult,
};

/// Format tag every hashed/wire object starts with (SPEC §10).
pub const FORMAT_VERSION: u16 = 1;
