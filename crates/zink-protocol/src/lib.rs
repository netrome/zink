//! Pure protocol core: types, canonical encoding, hashing, DAG, crypto.
//!
//! No I/O, no network, no async runtime — data in, data out.
//! See `docs/SPEC.md` and `docs/STYLE.md`.

mod attestation;
mod codec;
mod crypto;
mod keys;
mod message;

pub use attestation::{Attestation, AttestationId, Claim, SignedAttestation};
pub use codec::DecodeError;
pub use crypto::{ContentKey, CryptoError};
pub use keys::{DeviceKey, PublicKey, Signature, VerifyError};
pub use message::{
    BlobHash, BlobKind, BlobRef, KeyCommitment, KeyWrap, MessageCore, MessageEnvelope, MessageId,
    SealedKey, SealedRef,
};

/// Format tag every hashed/wire object starts with (SPEC §10).
pub const FORMAT_VERSION: u16 = 1;
