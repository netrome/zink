//! The Tauri-command wire types shared by `app/src-tauri` (serializes) and
//! `app/ui` (deserializes). Presentation-shaped on purpose: ids and keys are
//! hex strings, senders are labels — the command layer resolves petnames so
//! the webview never re-implements naming policy.

use serde::{Deserialize, Serialize};

/// Everything the UI needs on load, in one call.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppState {
    pub my_key: String,
    pub name: Option<String>,
    pub relay: Option<String>,
    pub contacts: Vec<ContactRow>,
    pub record: Option<QrPayload>,
    /// Recognized own devices (D3e) — the me-view's device list, and what
    /// gates the chat view's "introduce my devices" button.
    pub devices: Vec<DeviceRow>,
}

/// One recognized own device (multi-device.md §3).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceRow {
    /// The device's self-claimed name ("mårten laptop"), or short hex.
    pub name: String,
    /// The vouched device key, hex.
    pub key: String,
}

/// A decoded-but-not-yet-trusted record (D3e): what the pair-mode confirm
/// shows before anything is signed — scanning a wrong QR must never
/// silently vouch (multi-device.md §3).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RecordPreview {
    /// The record's verified self-claimed name, if any.
    pub name: Option<String>,
    /// The device key (the record's first key), full hex — the fingerprint
    /// the user confirms against the other device's me-view.
    pub key: String,
}

/// One contact-list row. The key rides along so the contact view can run
/// identity actions (`who_is` refresh, D1c) without re-deriving it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContactRow {
    pub petname: String,
    /// The record's first key, hex — the row's avatar / `who_is` handle.
    pub key: String,
    /// The full cluster of keys grouped under this person, hex — cluster-first
    /// (U4, ui-facelift.md §4); consumers read the set, never assume `key` is
    /// the only one.
    pub keys: Vec<String>,
    /// Whether this device currently vouches for them (D4c toggle).
    pub vouched: bool,
    /// Render-ready disavowal warnings, e.g. "disavowed by mårten —
    /// excluded from your replies" (D4c). Empty for the common case.
    pub disavowals: Vec<String>,
}

/// The person-detail screen (U4, ui-facelift.md §4): the three separated
/// belief layers, all read-time (no network pull). Fetched by petname when a
/// People row is tapped.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersonDetail {
    /// My petname for them (my lens).
    pub petname: String,
    /// The keys I've grouped under this person, hex — cluster-first, never
    /// one-key-per-person.
    pub keys: Vec<String>,
    /// The key avatar lookup uses (the cluster's first).
    pub avatar_key: String,
    /// Whether I currently vouch for them.
    pub vouched: bool,
    /// Their own verified self-claimed name, if any (their self-claim layer).
    pub self_name: Option<String>,
    /// How mutual friends label them — vouched names only, never a friend's
    /// private petname (the friends' lens; who-is-this.md §6).
    pub friends: Vec<FriendLabel>,
    /// Render-ready disavowal warnings (D4c) — context for a trust decision.
    pub disavowals: Vec<String>,
}

/// One vouched name from the friends' lens: a name, and the petnames of the
/// friends who vouch it for this person.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FriendLabel {
    pub name: String,
    pub vouched_by: Vec<String>,
}

/// A displayable ContactRecord: SVG for the screen, text for copy/paste.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct QrPayload {
    pub svg: String,
    pub text: String,
}

/// One conversation-list row.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Conversation {
    /// Conversation id, hex — the handle for `messages` / `send_message`.
    pub id: String,
    /// The other participants, petname-resolved ("only me" when alone).
    pub label: String,
    pub message_count: usize,
    /// Wall-clock hint of the newest message — display ordering only.
    pub last_timestamp_ms: u64,
}

/// One message-view row, in linearized DAG order.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    /// Message id, hex — the handle for `fetch_blob`.
    pub id: String,
    /// Conversation id, hex (carried so a blob fetch needs no extra state).
    pub conversation: String,
    /// Sender label ("me", a petname, or short hex).
    pub sender: String,
    /// Sender key, hex — the `avatar` lookup handle (D1d).
    pub sender_key: String,
    pub mine: bool,
    /// Lossy-decoded body; `None` when this device cannot open it.
    pub text: Option<String>,
    /// Sender's wall-clock hint (ms) — display only.
    pub timestamp_ms: u64,
    /// Referenced blobs, in envelope order (thumbnails first by our send
    /// convention, but don't rely on it — filter by `kind`).
    pub blobs: Vec<BlobInfo>,
    /// True while ≥1 relay is still owed this message (outbox, C4a) —
    /// delivery will be retried; render a "not yet delivered" cue.
    pub pending: bool,
    /// Causally incomparable with the message above it — they crossed in
    /// flight (D4d, tenet 7). The rendered order is unchanged.
    pub crossed: bool,
    /// Merges concurrent branches (more than one parent).
    pub merged: bool,
    /// The sender's key (hex) when it belongs to no stored contact — the
    /// "who is this?" handle (D1c). `None` for own and contacts' messages.
    pub unknown_sender: Option<String>,
    /// Membership deltas vs this message's parents (D2c, groups.md §2) —
    /// labels of keys this message added to / dropped from the addressed
    /// set. Derived from signed cores; empty for genesis / partial views.
    pub joined: Vec<String>,
    pub left: Vec<String>,
}

/// One unknown member of a conversation — the "a wild key appeared"
/// surface (D2c, groups.md §5). Candidates come from the learned store
/// (the scoped auto-query fills it); `dismissed` collapses the popup to
/// the compact manual row.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UnknownMember {
    /// The key, hex — handle for `who_is` / `dismiss`.
    pub key: String,
    pub candidates: Vec<WhoIsCandidate>,
    pub dismissed: bool,
    /// Verified link evidence, strongest first (D3c, multi-device.md §7):
    /// render-ready lines like "mårten says this is their device" /
    /// "…mutually confirmed". Evidence for an offer, never automation.
    pub device_evidence: Vec<String>,
    /// Render-ready disavowal warnings (D4c) — evidence at the moment of
    /// decision, never a block.
    pub disavowals: Vec<String>,
}

/// What a `who_is` query brought back, render-ready (D1c).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WhoIsReport {
    /// How many contacts served a record just now.
    pub answers: usize,
    /// The honest denominator (De3): dialable contacts queried, and how
    /// many of those couldn't be reached — "nobody answered" and "nobody
    /// was reachable" are different verdicts.
    pub asked: usize,
    pub unreachable: usize,
    /// The petname, when the key already belongs to a contact (the
    /// refresh flow — fresh answers sharpen relay resolution by
    /// themselves; there is nothing to promote).
    pub contact: Option<String>,
    /// Ranked name candidates for an unknown key, best first.
    pub candidates: Vec<WhoIsCandidate>,
    /// Render-ready disavowal warnings for the key (D4c).
    pub disavowals: Vec<String>,
}

/// One believable name for an unknown key, with provenance.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WhoIsCandidate {
    pub name: String,
    /// Preformatted: "confirmed by themself" / "records held by Bob, Dana".
    pub provenance: String,
    /// Feed to `add_contact` to promote — the freshest served record
    /// claiming this name; `None` when no responder is serving one right
    /// now (the claim came from an earlier query).
    pub payload: Option<String>,
}

/// One blob reference of a message.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BlobInfo {
    /// Blob hash, hex — the handle for `fetch_blob`.
    pub hash: String,
    /// "thumbnail" | "full".
    pub kind: String,
}

/// An image to attach to an outgoing message, prepared by the webview
/// (canvas-downscaled): base64 of the encoded image bytes, no data-URL
/// prefix. Base64 because Tauri's IPC is JSON — raw bytes don't survive it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutgoingImage {
    pub thumb_b64: String,
    pub full_b64: String,
}
