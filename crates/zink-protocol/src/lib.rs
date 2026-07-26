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

pub use attestation::{
    Attestation, AttestationId, Claim, LinkTier, SignedAttestation, link_tier, verified_negative,
};
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
    open_avatar, seal_avatar,
};
pub use sync::{
    MAX_GET_KEYS_IDS, MAX_SYNC_REQUEST_BYTES, MAX_SYNC_RESPONSE_BYTES, SYNC_ALPN, SyncErrorCode,
    SyncOp, SyncRequest, SyncResponse, SyncResult,
};
#[cfg(test)]
mod version_tests;

/// Per-type format versioning (SPEC §10). Every hashed/wire object starts
/// with a `u16` version tag, and **each type owns its own number**.
///
/// This is deliberately not one global constant. With a single
/// `FORMAT_VERSION` compared for equality, bumping *any* object forked the
/// whole protocol at once: peers on the old build silently skip every object
/// of every type (§10 says unknown versions are ignored, not errored), and a
/// bumped build cannot even decode its own on-disk state — a stored envelope
/// at v1 fails `decode_versioned`, and the loader drops it as damaged. Fine
/// while every install was ours and dev data could be wiped; a silent,
/// unannounced break the moment two builds coexist.
///
/// Per type, the wire cost of an addition is confined to the type that
/// changed: `ContactRecord` can move to 2 and accept `{1, 2}` while
/// `MessageCore` stays at 1 — which matters most for `MessageCore`, whose
/// version sits *inside the hashed core*, so bumping it would move every
/// message id in existence.
pub trait Versioned {
    /// The version this build stamps when it encodes.
    const CURRENT: u16;
    /// Every version this build can decode. Must contain [`Self::CURRENT`];
    /// `versioned__should_accept_its_own_current` pins that for each type.
    ///
    /// Widening this is how a format change ships: add the new number,
    /// keep the old one for as long as peers and stored data may carry it.
    const ACCEPTED: &'static [u16];

    /// Can this build decode an object of this type stamped `version`?
    fn supports(version: u16) -> bool {
        Self::ACCEPTED.contains(&version)
    }
}
