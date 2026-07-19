//! The client error type (De1): one crate-wide enum. Variants an edge or
//! test *branches on* get their own precise type (`NoRelayUrl`,
//! `NotAContact`, …); failures only ever shown to a human are grouped by
//! kind with the detail as payload (`Storage`, `Transport`, …). Protocol
//! errors that are already typed pass through via `#[from]`.
//!
//! Edges that speak `Result<_, String>` (CLI commands, Tauri commands) keep
//! working unchanged: `From<Error> for String` renders via `Display`, so
//! `?` converts at the boundary — presentation stays at the edge.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    // ---- local ----
    /// Device-key file could not be read, parsed, or created.
    #[error("keystore: {0}")]
    Keystore(String),
    /// Client state on disk failed (read/write/decode).
    #[error("storage: {0}")]
    Storage(String),
    /// A stored conversation cannot produce a DAG (no genesis on disk,
    /// or the stored genesis is invalid) — it can't be threaded into.
    #[error("conversation not threadable: {0}")]
    Conversation(String),

    // ---- input ----
    /// Malformed user-supplied value (hex id/key, relay spec, contact spec).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A scanned/pasted record that can't be used as a contact.
    #[error("unusable record: {0}")]
    InvalidRecord(String),
    /// The chosen petname already names a different contact.
    #[error("a different contact is already named {0:?}")]
    PetnameCollision(String),
    /// The record shares a key with an existing contact stored under a
    /// different petname — updating that entry must be explicit, never a
    /// side effect of an add that said "new contact" (multi-device.md §4).
    #[error(
        "record shares a key with your contact {existing:?} — add it under that petname to update their entry"
    )]
    ContactOverlap { existing: String },
    /// The record shares keys with two or more contacts; merging is never
    /// silent — an ambiguous record is refused (multi-device.md §4).
    #[error("record shares keys with multiple contacts ({0}) — not stored")]
    AmbiguousOverlap(String),
    /// A send needs someone to send to.
    #[error("at least one recipient required")]
    NoRecipients,
    /// The profile is missing what this operation needs.
    #[error("{0}")]
    ProfileIncomplete(&'static str),

    // ---- contacts / addressing ----
    /// No stored contact matches the petname / key, or none of a
    /// conversation's participants resolve to a record.
    #[error("no stored contact: {0}")]
    NotAContact(String),
    /// The stored record carries no relay URL — dial-by-key impossible.
    #[error("contact record has no relay url — re-exchange records to enable dial-by-key")]
    NoRelayUrl,

    // ---- network ----
    /// Could not reach the other side (connect failed or timed out).
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// A connection or stream failed mid-operation.
    #[error("transport: {0}")]
    Transport(String),
    /// The relay/peer answered outside the protocol.
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
    /// No relay accepted the deposit; the message is queued in the outbox
    /// and will retry — "queued", not "lost" (live-delivery.md §2).
    #[error("no relay took the deposit — message queued for retry ({0})")]
    AllRelaysPending(String),
    /// The blob is in no reachable cache.
    #[error("blob fetch failed: {0}")]
    BlobUnavailable(String),

    // ---- typed protocol errors, passed through ----
    #[error("crypto: {0}")]
    Crypto(#[from] zink_protocol::CryptoError),
    #[error("open: {0}")]
    Open(#[from] zink_protocol::OpenError),
    #[error("decode: {0}")]
    Decode(#[from] zink_protocol::DecodeError),
}

/// The edge shim: CLI and Tauri commands return `Result<_, String>`
/// (presentation), and `?` converts through this.
impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.to_string()
    }
}
