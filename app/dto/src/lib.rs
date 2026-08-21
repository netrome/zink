//! The Tauri-command wire types shared by `app/src-tauri` (serializes) and
//! `app/ui` (deserializes). Presentation-shaped on purpose: ids and keys are
//! hex strings, senders are labels — the command layer resolves petnames so
//! the webview never re-implements naming policy.

use serde::{Deserialize, Serialize};

/// The relay-spec QR prefix (R4) — the webview's routing copy of
/// `zink_protocol::RelayEntry::QR_PREFIX` (the source of truth, which the
/// UI crate deliberately doesn't depend on). Must match it byte-for-byte;
/// the protocol's tests pin the literal.
pub const RELAY_QR_PREFIX: &str = "ZINK-RELAY:";

/// Everything the UI needs on load, in one call.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppState {
    pub my_key: String,
    pub name: Option<String>,
    /// This device's self-claimed device label ("phone", "laptop") — the
    /// qualifier beside the person name (SPEC §3.2 `DeviceLabel`, S1).
    pub device_label: Option<String>,
    /// All home relays as full dial specs (`dial[#relay-url]`, U5 multi-relay)
    /// — "where your messages wait when you're offline". Round-trips back
    /// through `set_profile`; a bare dial string would drop the relay URL.
    pub relays: Vec<String>,
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

/// Add-flow triage for a scanned/pasted record (R1, relay lifecycle): a
/// key-overlapping record is an *update* of that contact and detours to a
/// confirm card; anything else flows to the plain add. A record spanning
/// two contacts errors at the command layer instead — it can't be stored.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AddPreview {
    /// The single overlapping contact's petname — `Some` routes the UI to
    /// the update-confirm card; `None` means a new person.
    pub updates: Option<String>,
    /// Render-ready change lines for the card ("name: Anna → Ann",
    /// "+ relay xx@…", "+ 1 device key"). Empty = their record matches
    /// what's stored.
    pub changes: Vec<String>,
    /// The record's verified self-claimed name — the new-person prefill.
    pub name: Option<String>,
}

/// One people-list / picker row — a **person** (project 7 S2/S3): the
/// cluster lens, one row per person, never per device key. `petname` is
/// the person label — what send-by-name resolves.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContactRow {
    /// The opaque person id — what the page fetch and every act key on.
    /// Ids identify; labels display and address (a label is a mutable
    /// lens, so nothing holds a reference by it). Round-trips unread.
    pub id: String,
    /// The person label (my lens; the addressing name).
    pub petname: String,
    /// A verified self-claimed name from the cluster, if any — a row's dim
    /// second line when it differs from the label (S5).
    pub self_name: Option<String>,
    /// The first member's first key, hex — the row's avatar handle.
    pub key: String,
    /// Every key across the person's member entries, hex — cluster-first
    /// (ui-design-system.md §1); consumers read the set, never assume
    /// `key` is the only one.
    pub keys: Vec<String>,
    /// How many device entries this person spans (the "2 devices" hint).
    pub members: usize,
    /// Whether this device vouches for any member (D4c).
    pub vouched: bool,
    /// Render-ready disavowal warnings across the cluster (D4c). Empty for
    /// the common case.
    pub disavowals: Vec<String>,
}

/// The person page (project 7 S3): one page for contacts and strangers
/// alike, keyed by the observer's cluster lens. Exactly one of `person` /
/// `stranger` is set. All read-time, local stores only — rendering never
/// queries anyone; the page's queries (the contact subject-refresh, the
/// per-friend ask, the stranger bootstrap) are separate explicit commands.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersonPage {
    /// The header: person label (contact), else the best learned
    /// self-claim, else short hex.
    pub label: String,
    /// Set for a contact person — the my-lens acts apply (rename, merge).
    pub person: Option<PersonInfo>,
    /// Set for a non-contact key — the stranger variant.
    pub stranger: Option<StrangerInfo>,
    /// The key avatar lookup uses (first member's first key).
    pub avatar_key: String,
    /// Whether I've set a local photo (U6) — drives the photo affordances.
    pub has_local_avatar: bool,
    /// The per-device layer: one card per member entry (a stranger is a
    /// one-card degenerate cluster).
    pub devices: Vec<DeviceCard>,
    /// The through-friends lens: what each friend *tells* you — vouched
    /// names and held records; never their private petnames
    /// (web-of-trust.md §6). Display-only; addressing stays mine.
    pub friends: Vec<FriendLens>,
}

/// The contact-person half of the page.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersonInfo {
    /// The opaque person id — the handle for rename / merge /
    /// `person_conversations` (ids identify; labels display).
    pub id: String,
    /// Other persons — the "same person as…" merge picker's options.
    pub merge_candidates: Vec<PersonRef>,
}

/// A person handle for pickers: the id acts key on, the label humans see.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersonRef {
    pub id: String,
    pub label: String,
}

/// The stranger half of the page (absorbs the original identity-preview
/// proposal): everything believed about an unknown key, plus the acts.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StrangerInfo {
    /// The key, hex — the handle for who-is / dismiss / add.
    pub key: String,
    /// Ranked name candidates from the learned store (no query fired).
    pub candidates: Vec<WhoIsCandidate>,
    pub dismissed: bool,
    /// ZINK payload when this key's record verifiably claims to be one of
    /// MY devices (the one-way pairing case) — feeds the pair-confirm
    /// fingerprint flow; recognizing is never one tap.
    pub pair_back: Option<String>,
}

/// Outcome of the deliberate subject ask (project 7 — the stranger
/// bootstrap's direct rung): three states the edge words distinctly. An
/// answer landed; they were reached but served nothing (declining and
/// not-holding look the same on the wire); or no route reached them.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SubjectAsk {
    Answered,
    Nothing,
    Unreachable,
}

/// One member device of the page's cluster — my belief about one key.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceCard {
    /// My per-device petname (the entry underneath the person label), or
    /// short hex for a stranger.
    pub petname: String,
    /// Their self-claimed device label ("phone", "laptop" — SPEC §3.2).
    pub device_label: Option<String>,
    /// Their self-claimed person name.
    pub self_name: Option<String>,
    /// Full key hex — the fingerprint, shown at trust moments.
    pub key: String,
    /// Link evidence with direction, render-ready ("mårten-phone says this
    /// is their device", "…mutually confirmed"); empty = clustered by you
    /// alone, no cryptographic link.
    pub link: Vec<String>,
    /// Render-ready disavowal warnings for this key (D4c).
    pub disavowals: Vec<String>,
    /// Render-ready provenance for `relays` (R5) — per device, from that
    /// device's own records (relays bind to the publishing device).
    pub relay_source: String,
    pub relays: Vec<RelayRow>,
    /// Whether a manual relay override is in effect for this entry.
    pub relay_override: bool,
    /// Whether I vouch for this entry (sharing its petname — D4a).
    pub vouched: bool,
    /// Whether the split act applies (the person has other members).
    pub can_split: bool,
}

/// One friend's lens on this person: what they chose to tell you.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FriendLens {
    /// The friend, by my petname for them — also the `ask_friend` handle.
    pub petname: String,
    /// The name they vouch for this person (their published claim), if any.
    pub vouched_name: Option<String>,
    /// Render-ready held-record lines ("holds their record — 'Mårten ·
    /// laptop', from 2 d ago").
    pub held: Vec<String>,
}

/// One effective relay for a person (R5).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RelayRow {
    /// Full spec (`dial[#relay-url]`).
    pub spec: String,
    /// Render-ready debt line ("⚠ 2 message(s) queued for this relay ·
    /// oldest 3 d ago") — `None` = nothing queued. Per *relay*, so on a
    /// relay shared across contacts it can include other people's messages.
    pub owed: Option<String>,
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
    /// One-line preview of the newest message, preformatted ("you: hi",
    /// "alice: 📎 image", "🔒 can't read this yet"); empty when there is
    /// nothing to preview. Local plaintext, client-side policy (S5).
    pub snippet: String,
    /// Stored messages this device hasn't rendered yet (S7) — the row
    /// badge. Local presentation state, never a receipt to anyone.
    pub unread: usize,
    /// Nobody you know has *written* here yet (groups.md §6), so this sits
    /// in the requests queue rather than the main list. Not a verdict: a
    /// contact's first message promotes it with nothing lost.
    pub request: bool,
    /// For a request row: an unknown sender's key (hex) — the "who is
    /// this?" preview handle, opening the person page (S3). `None` on
    /// ordinary conversations.
    pub stranger_key: Option<String>,
}

/// The chats screen's two lists (groups.md §6, unknown-sender quarantine).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Inbox {
    /// Conversations a contact has contributed to.
    pub conversations: Vec<Conversation>,
    /// Requests from unknown senders — bounded; newest first.
    pub requests: Vec<Conversation>,
    /// Requests past the cap. Shown as a count rather than hidden.
    pub dropped: usize,
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
    /// Pending *and* owed long enough (command-layer policy: 10 minutes)
    /// that a relay is likely unreachable (R3) — render an actionable
    /// "can't reach their relay" cue instead of "sending…". A fact about
    /// our deposits, never a claim about their receipt: the message may
    /// well have arrived by another path.
    pub stuck: bool,
    /// The debt passed the 30-day give-up window: retries stopped
    /// (relay-lifecycle.md §2) — render "undelivered".
    pub undelivered: bool,
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
    /// Labels of recipient devices that confirmed a durable store of this
    /// message (De7) — vouched by the **recipient's own device key**, not
    /// by a relay. **Positive-only** (tenet 7): render "confirmed by …"
    /// when non-empty and render *nothing* when empty. Empty means no
    /// confirmation was received, NOT that the message is undelivered —
    /// most mailbox-path messages arrive fine and never say so. No greyed
    /// ticks, no "undelivered" state; `pending` is the only negative cue.
    pub confirmed: Vec<String>,
}

/// One unknown member of a conversation — the "a wild key appeared"
/// surface (D2c, groups.md §5). Since S4 the row is a link: the person
/// page owns the acts and evidence (candidates, who-is, ignore, add);
/// the row only surfaces the key and navigates. `dismissed` dims it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UnknownMember {
    /// The key, hex — the person-page target.
    pub key: String,
    pub dismissed: bool,
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

/// The members panel (project 6 S2): the conversation's current membership,
/// presentation-shaped, plus the re-derived header label — so a membership
/// change updates the title instead of leaving the open-time label stale.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConversationMembers {
    /// The chat-header label — my local name when set (S6), else other
    /// participants petname-resolved, "only me" when alone.
    pub label: String,
    /// My local name for this conversation, if set — prefills the panel's
    /// rename field. Local lens, never transmitted.
    pub local_name: Option<String>,
    /// Every current member: "you", then one row per person / device /
    /// unknown key. Rows navigate to the person page (S4).
    pub members: Vec<MemberRow>,
    /// The contact petnames among the members — what the add-picker
    /// excludes.
    pub petnames: Vec<String>,
}

/// One members-panel row: the label humans see and the key the row
/// navigates by — `None` for the merged "you" row (an own *cluster*, not
/// one identifier; Me is its page).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MemberRow {
    pub label: String,
    pub key: Option<String>,
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
