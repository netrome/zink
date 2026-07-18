//! Client-side persistence (slice B5): conversations on disk, so the CLI
//! can thread messages instead of sending standalone geneses.
//!
//! Layout under `<key-file>.state/`:
//! - `conversations/<conv-id-hex>/<message-id-hex>.env` — one file per
//!   envelope, content = canonical wire bytes. The DAG is rebuilt from
//!   these on demand (out-of-order insert is the store's normal mode).
//! - `participants/<fingerprint-hex>` — maps a participant set to its
//!   conversation id. "One conversation per participant set" is *client
//!   policy* (SPEC tenet 4), not protocol: sender and recipients land in
//!   the same conversation because both sides fingerprint the same set.
//! - `profile.name` / `profile.relays` — this device's display name and
//!   home relays (what goes into its ContactRecord).
//! - `contacts/<key-hex>.record` (wire bytes) + `.name` (the local petname
//!   — client policy, defaulting to the contact's self-claimed name).
//! - `blobs/<hash-hex>` — cached *encrypted* blobs (ciphertext at rest, like
//!   the envelopes), so images outlive the relay cache's TTL and the
//!   sender's own images need no relay at all (C3a).
//! - `outbox/<message-hex>.<relay-fp>` — the delivery ledger (C4a,
//!   live-delivery.md §2): one entry per (message, relay) still owed a
//!   deposit (and its blob pushes). Written before any network work,
//!   removed on per-relay success; three text lines (relay dial string,
//!   conversation hex, created-ms).

use std::collections::BTreeSet;
use std::path::PathBuf;

use zink_protocol::{
    BlobHash, ContactRecord, ConversationDag, MessageEnvelope, MessageId, PublicKey, RelayEntry,
};

#[derive(Clone, Debug)]
pub struct ClientState {
    root: PathBuf,
}

impl ClientState {
    /// State lives next to the key file: `<key-file>.state/`.
    pub fn open(key_path: &str) -> Self {
        Self {
            root: PathBuf::from(format!("{key_path}.state")),
        }
    }

    /// The conversation this participant set maps to, if any.
    pub fn conversation_for(&self, participants: &BTreeSet<PublicKey>) -> Option<MessageId> {
        let bytes = std::fs::read(self.participants_file(participants)).ok()?;
        Some(MessageId(bytes.try_into().ok()?))
    }

    pub fn record_conversation(
        &self,
        participants: &BTreeSet<PublicKey>,
        conversation: MessageId,
    ) -> Result<(), String> {
        let path = self.participants_file(participants);
        create_parent(&path)?;
        write_atomic(&path, &conversation.0).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// Persist an envelope under its conversation. Idempotent (the file
    /// name is the message id).
    pub fn store_envelope(
        &self,
        conversation: MessageId,
        envelope: &MessageEnvelope,
    ) -> Result<(), String> {
        let path = self
            .conversation_dir(conversation)
            .join(format!("{}.env", hex(&envelope.id().0)));
        create_parent(&path)?;
        write_atomic(&path, &envelope.to_bytes()).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// All decodable envelopes stored under a conversation, unordered. A
    /// damaged file is skipped with a warning, never fatal: the DAG then
    /// honestly reports the hole as a missing parent / seq gap.
    pub fn load_envelopes(&self, conversation: MessageId) -> Result<Vec<MessageEnvelope>, String> {
        let dir = self.conversation_dir(conversation);
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))?;
        let mut envelopes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "env") {
                continue; // e.g. an orphaned write_atomic temp file
            }
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    MessageEnvelope::try_from_bytes(&bytes).map_err(|e| e.to_string())
                }) {
                Ok(envelope) => envelopes.push(envelope),
                Err(e) => tracing::warn!(?path, error = %e, "skipping damaged file"),
            }
        }
        Ok(envelopes)
    }

    /// One stored envelope by conversation + message id.
    pub fn load_envelope(
        &self,
        conversation: MessageId,
        message: MessageId,
    ) -> Result<MessageEnvelope, String> {
        let path = self
            .conversation_dir(conversation)
            .join(format!("{}.env", hex(&message.0)));
        let bytes = std::fs::read(&path).map_err(|e| format!("read {path:?}: {e}"))?;
        MessageEnvelope::try_from_bytes(&bytes).map_err(|e| format!("decode {path:?}: {e}"))
    }

    /// Every conversation with stored envelopes, sorted by id (the caller
    /// orders for display — id order is just deterministic).
    pub fn conversations(&self) -> Vec<MessageId> {
        let dir = self.root.join("conversations");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut ids: Vec<MessageId> = entries
            .flatten()
            .filter_map(|entry| {
                crate::hex::parse32(&entry.file_name().to_string_lossy())
                    .ok()
                    .map(MessageId)
            })
            .collect();
        ids.sort();
        ids
    }

    /// Rebuild the DAG from the stored envelopes. Order on disk is
    /// irrelevant — the store accepts children before parents. Only a
    /// missing genesis is unrecoverable (there is no root to build on).
    pub fn load_dag(&self, conversation: MessageId) -> Result<ConversationDag, String> {
        let mut cores: Vec<_> = self
            .load_envelopes(conversation)?
            .into_iter()
            .map(|envelope| envelope.core)
            .collect();
        let genesis_at = cores
            .iter()
            .position(|core| core.conversation.is_none())
            .ok_or_else(|| {
                format!(
                    "conversation {} has no genesis on disk",
                    hex(&conversation.0)
                )
            })?;
        let mut dag = ConversationDag::new(cores.swap_remove(genesis_at))
            .map_err(|e| format!("stored genesis invalid: {e}"))?;
        for core in cores {
            if let Err(e) = dag.insert(core) {
                tracing::warn!(error = %e, "skipping invalid stored message");
            }
        }
        Ok(dag)
    }

    /// One stored envelope by message id, wherever it lives. Content is
    /// addressed by id alone (SPEC §5.2 `get`), but on disk it's filed under a
    /// conversation, so this scans conversations — fine at friend/family scale
    /// (few conversations); an id→conversation index is the optimization if a
    /// large store ever makes the scan bite.
    pub fn find_envelope(&self, id: MessageId) -> Option<MessageEnvelope> {
        self.conversations()
            .into_iter()
            .find_map(|conversation| self.load_envelope(conversation, id).ok())
    }

    /// Ids of held messages whose `parents` include `parent` (SPEC §5.2
    /// `get-successors`) — known children, for pulling a conversation forward.
    pub fn successors(&self, parent: MessageId) -> Vec<MessageId> {
        let mut ids: Vec<MessageId> = self
            .conversations()
            .into_iter()
            .flat_map(|conversation| self.load_envelopes(conversation).unwrap_or_default())
            .filter(|envelope| envelope.core.parents.contains(&parent))
            .map(|envelope| envelope.id())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Referenced parents we don't hold for `conversation` — the frontier a
    /// backfill fetches to walk back toward the genesis. Empty when the stored
    /// slice is already ancestor-closed (genesis reached, or nothing stored).
    pub fn missing_ancestors(&self, conversation: MessageId) -> Vec<MessageId> {
        let envelopes = self.load_envelopes(conversation).unwrap_or_default();
        let present: BTreeSet<MessageId> = envelopes.iter().map(|e| e.id()).collect();
        let mut missing: BTreeSet<MessageId> = BTreeSet::new();
        for envelope in &envelopes {
            for parent in &envelope.core.parents {
                if !present.contains(parent) {
                    missing.insert(*parent);
                }
            }
        }
        missing.into_iter().collect()
    }

    /// Cache a blob as fetched/produced — encrypted, keyed by its hash.
    /// Idempotent (content-addressed: same hash ⇒ same bytes).
    pub fn save_blob(&self, hash: &BlobHash, bytes: &[u8]) -> Result<(), String> {
        let path = self.blob_path(hash);
        create_parent(&path)?;
        write_atomic(&path, bytes).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// A cached encrypted blob, if present. The caller verifies + decrypts
    /// against the referencing envelope — the cache is trusted no more than
    /// a relay would be.
    pub fn load_blob(&self, hash: &BlobHash) -> Option<Vec<u8>> {
        std::fs::read(self.blob_path(hash)).ok()
    }

    fn blob_path(&self, hash: &BlobHash) -> PathBuf {
        self.root.join("blobs").join(hex(&hash.0))
    }

    /// Record that `message` still owes `relay` a deposit (and blob pushes).
    /// Written *before* any network work, so a crash mid-send leaves the
    /// ledger honest. Idempotent (same name, same content).
    pub fn add_outbox(
        &self,
        message: MessageId,
        relay: &str,
        conversation: MessageId,
        created_ms: u64,
    ) -> Result<(), String> {
        let path = self.outbox_path(message, relay);
        create_parent(&path)?;
        let content = format!("{relay}\n{}\n{created_ms}\n", hex(&conversation.0));
        write_atomic(&path, content.as_bytes()).map_err(|e| format!("write {path:?}: {e}"))
    }

    /// Delivery to `relay` succeeded — drop the entry. Missing is fine
    /// (already cleared).
    pub fn clear_outbox(&self, message: MessageId, relay: &str) {
        let _ = std::fs::remove_file(self.outbox_path(message, relay));
    }

    /// Every outstanding delivery, oldest first. Damaged entries are
    /// removed with a warning — an unparseable ledger line can't be
    /// retried anyway.
    pub fn outbox(&self) -> Vec<OutboxEntry> {
        let dir = self.root.join("outbox");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut outbox = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match parse_outbox_entry(&path) {
                Some(entry) => outbox.push(entry),
                None => {
                    tracing::warn!(?path, "dropping damaged outbox entry");
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        outbox.sort_by_key(|entry| entry.created_ms);
        outbox
    }

    /// Message ids with at least one outstanding delivery — the `pending`
    /// flag of history.
    pub fn pending_messages(&self) -> BTreeSet<MessageId> {
        self.outbox()
            .into_iter()
            .map(|entry| entry.message)
            .collect()
    }

    /// One entry per (message, relay): the relay part of the name is a
    /// fingerprint (dial strings hold `@`/`:`), the full string lives in
    /// the file.
    fn outbox_path(&self, message: MessageId, relay: &str) -> PathBuf {
        let fingerprint = blake3::hash(relay.as_bytes()).to_hex();
        self.root.join("outbox").join(format!(
            "{}.{}",
            hex(&message.0),
            &fingerprint.as_str()[..16]
        ))
    }

    pub fn save_profile(&self, name: &str, relays: &[RelayEntry]) -> Result<(), String> {
        let name_path = self.root.join("profile.name");
        create_parent(&name_path)?;
        write_atomic(&name_path, name.as_bytes()).map_err(|e| format!("write profile: {e}"))?;
        let specs: Vec<String> = relays.iter().map(RelayEntry::to_spec).collect();
        write_atomic(
            &self.root.join("profile.relays"),
            specs.join("\n").as_bytes(),
        )
        .map_err(|e| format!("write relays: {e}"))
    }

    pub fn profile_name(&self) -> Option<String> {
        let name = std::fs::read_to_string(self.root.join("profile.name")).ok()?;
        (!name.trim().is_empty()).then(|| name.trim().to_string())
    }

    /// The home relay services, one spec line per entry (`dial[#relay-url]`).
    pub fn home_relay_entries(&self) -> Vec<RelayEntry> {
        std::fs::read_to_string(self.root.join("profile.relays"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(RelayEntry::from_spec)
            .collect()
    }

    /// The home relays' mailbox dial strings — what every mailbox path
    /// (deposit fan-out, recv, subscribe, outbox keys) runs on.
    pub fn home_relays(&self) -> Vec<String> {
        self.home_relay_entries()
            .into_iter()
            .map(|entry| entry.mailbox)
            .collect()
    }

    /// Store a contact under a petname. The record is kept in wire form;
    /// the petname is a sibling file (local convention, never protocol).
    pub fn save_contact(&self, petname: &str, record: &ContactRecord) -> Result<(), String> {
        let stem = self.contact_stem(record.keys.first().ok_or("record has no keys")?);
        create_parent(&stem.with_extension("record"))?;
        write_atomic(&stem.with_extension("record"), &record.to_bytes())
            .map_err(|e| format!("write contact: {e}"))?;
        write_atomic(&stem.with_extension("name"), petname.as_bytes())
            .map_err(|e| format!("write petname: {e}"))
    }

    /// All stored contacts as `(petname, record)`, petname-sorted.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, String> {
        let dir = self.root.join("contacts");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut contacts = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let record = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).map_err(|e| e.to_string()));
            let petname = std::fs::read_to_string(path.with_extension("name"));
            match (record, petname) {
                (Ok(record), Ok(petname)) => contacts.push((petname.trim().to_string(), record)),
                (record, petname) => tracing::warn!(
                    ?path,
                    record_err = ?record.err(),
                    petname_err = ?petname.err(),
                    "skipping damaged contact"
                ),
            }
        }
        contacts.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(contacts)
    }

    fn contact_stem(&self, key: &PublicKey) -> PathBuf {
        self.root.join("contacts").join(hex(&key.0))
    }

    fn conversation_dir(&self, conversation: MessageId) -> PathBuf {
        self.root.join("conversations").join(hex(&conversation.0))
    }

    fn participants_file(&self, participants: &BTreeSet<PublicKey>) -> PathBuf {
        // Fingerprint = BLAKE3 over the sorted keys (BTreeSet iterates
        // sorted), so any member computes the same name.
        let mut hasher = blake3::Hasher::new();
        for key in participants {
            hasher.update(&key.0);
        }
        self.root
            .join("participants")
            .join(hasher.finalize().to_hex().as_str())
    }
}

/// One outstanding delivery: `message` (of `conversation`) still owes
/// `relay` a deposit and any blob pushes.
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub message: MessageId,
    pub relay: String,
    pub conversation: MessageId,
    pub created_ms: u64,
}

/// Filename carries the message id; the file body is three lines:
/// relay dial string, conversation hex, created-ms.
fn parse_outbox_entry(path: &std::path::Path) -> Option<OutboxEntry> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let message = MessageId(crate::hex::parse32(name.split('.').next()?).ok()?);
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let relay = lines.next()?.to_string();
    let conversation = MessageId(crate::hex::parse32(lines.next()?).ok()?);
    let created_ms = lines.next()?.parse().ok()?;
    Some(OutboxEntry {
        message,
        relay,
        conversation,
        created_ms,
    })
}

fn create_parent(path: &std::path::Path) -> Result<(), String> {
    let parent = path.parent().expect("state paths always have a parent");
    std::fs::create_dir_all(parent).map_err(|e| format!("create {parent:?}: {e}"))
}

/// Monotonic per-process counter so each `write_atomic` gets its own temp
/// file (see below).
static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Temp file + rename: a crash mid-write never leaves a truncated file.
/// The temp name is unique per *write*, not just per process: C4 made
/// `ClientState` concurrently accessible (subscription loops + command
/// handlers in one process), so two tasks can write the same target path at
/// once. A pid-only suffix made them collide on one temp file — the first
/// rename removed it and the second got ENOENT, surfacing as a spurious
/// drain failure and reconnect. The atomic counter gives each write its own
/// temp file; whichever renames last wins (the bytes are identical —
/// content-addressed — so the winner is immaterial).
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.to_path_buf();
    tmp.set_extension(format!("tmp{}.{seq}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
