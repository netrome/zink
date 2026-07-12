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
    pub contacts: Vec<String>,
    pub record: Option<QrPayload>,
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
    pub mine: bool,
    /// Lossy-decoded body; `None` when this device cannot open it.
    pub text: Option<String>,
    /// Sender's wall-clock hint (ms) — display only.
    pub timestamp_ms: u64,
    /// Referenced blobs, in envelope order (thumbnails first by our send
    /// convention, but don't rely on it — filter by `kind`).
    pub blobs: Vec<BlobInfo>,
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
