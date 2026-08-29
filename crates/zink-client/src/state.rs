//! Client-side persistence (slice B5): conversations on disk, so the CLI
//! can thread messages instead of sending standalone geneses.
//!
//! Layout under `<key-file>.state/`:
//! - `conversations/<conv-id-hex>/<message-id-hex>.env` — one file per
//!   envelope, content = canonical wire bytes. The DAG is rebuilt from
//!   these on demand (out-of-order insert is the store's normal mode).
//! - `conversations/<conv-id-hex>/<message-id-hex>.acks` — delivery
//!   confirmations (De7): the recipient device keys that returned D5's
//!   `Stored` ack, concatenated raw. The C4a ledger's third rung, scoped to
//!   the conversation so it dies with it. Positive-only — absence means "no
//!   confirmation", never "undelivered".
//! - `participants/<fingerprint-hex>` — maps a participant set to its
//!   conversation id. "One conversation per participant set" is *client
//!   policy* (SPEC tenet 4), not protocol: sender and recipients land in
//!   the same conversation because both sides fingerprint the same set.
//! - `profile.name` / `profile.relays` — this device's display name and
//!   home relays (what goes into its ContactRecord).
//! - `contacts/<key-hex>.record` (wire bytes) + `.name` (the local petname
//!   — client policy, defaulting to the contact's self-claimed name).
//! - `devices/<key-hex>.record` + `.vouch` — the own-devices store (D3b,
//!   multi-device.md §3): recognized siblings' records and the signed
//!   `same-person-as` vouches this device issued. Written only by the
//!   recognize act; the serving gate and `my_record` read it.
//! - `blobs/<hash-hex>` — cached *encrypted* blobs (ciphertext at rest, like
//!   the envelopes), so images outlive the relay cache's TTL and the
//!   sender's own images need no relay at all (C3a).
//! - `outbox/<message-hex>.<relay-fp>` — the delivery ledger (C4a,
//!   live-delivery.md §2): one entry per (message, relay) still owed a
//!   deposit (and its blob pushes). Written before any network work,
//!   removed on per-relay success; three text lines (relay dial string,
//!   conversation hex, created-ms).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::error::Error;
use zink_protocol::{
    BlobHash, Claim, ContactRecord, ConversationDag, MessageCore, MessageEnvelope, MessageId,
    PublicKey, RelayEntry, SignedAttestation,
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
    ) -> Result<(), Error> {
        let path = self.participants_file(participants);
        create_parent(&path)?;
        write_atomic(&path, &conversation.0)
            .map_err(|e| Error::Storage(format!("write {path:?}: {e}")))
    }

    /// Persist an envelope under its conversation. Idempotent (the file
    /// name is the message id).
    pub fn store_envelope(
        &self,
        conversation: MessageId,
        envelope: &MessageEnvelope,
    ) -> Result<(), Error> {
        let path = self
            .conversation_dir(conversation)
            .join(format!("{}.env", hex(&envelope.id().0)));
        create_parent(&path)?;
        write_atomic(&path, &envelope.to_bytes())
            .map_err(|e| Error::Storage(format!("write {path:?}: {e}")))?;
        self.note_first_seen(conversation);
        Ok(())
    }

    /// When this device first stored anything for a conversation — **our**
    /// clock, written once. 0 when unrecorded (a conversation from before
    /// this marker existed), which sorts as oldest.
    ///
    /// The senders' `timestamp_ms` cannot do this job: it is a display hint
    /// a sender chooses freely (SPEC §4.3), so a stranger could pin their
    /// message to the top of the requests queue forever by dating it in the
    /// future, and push real requests off the cap. Ordering the spam view by
    /// something attacker-controlled would hand them the eviction policy.
    pub fn first_seen_ms(&self, conversation: MessageId) -> u64 {
        std::fs::read_to_string(self.first_seen_path(conversation))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Write the marker if absent. Best-effort: losing it costs ordering
    /// precision in one view, never a message.
    fn note_first_seen(&self, conversation: MessageId) {
        let path = self.first_seen_path(conversation);
        if path.exists() {
            return;
        }
        let now = crate::adapters::system_clock::now_ms();
        if let Err(error) = write_atomic(&path, now.to_string().as_bytes()) {
            tracing::debug!(%error, "could not record a conversation's first-seen time");
        }
    }

    /// Inside the conversation dir, with no `.env` suffix, so
    /// `load_envelopes` ignores it like the `.acks` sidecars.
    fn first_seen_path(&self, conversation: MessageId) -> PathBuf {
        self.conversation_dir(conversation).join("first-seen")
    }

    /// My local name for a conversation (project 6 S6) — the petname's
    /// conversation-shaped sibling: pure presentation policy, never
    /// transmitted. `None` clears. Stored as **my lens** (`my-name`),
    /// deliberately distinct from any future peer-*suggestion* store —
    /// the anchor-vs-learned split the contact store uses.
    pub fn set_conversation_name(
        &self,
        conversation: MessageId,
        name: Option<&str>,
    ) -> Result<(), Error> {
        let path = self.conversation_name_path(conversation);
        match name {
            None => {
                let _ = std::fs::remove_file(&path);
                Ok(())
            }
            Some(name) => {
                create_parent(&path)?;
                write_atomic(&path, name.as_bytes())
                    .map_err(|e| Error::Storage(format!("write conversation name: {e}")))
            }
        }
    }

    pub fn conversation_name(&self, conversation: MessageId) -> Option<String> {
        std::fs::read_to_string(self.conversation_name_path(conversation))
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// A sidecar beside `first-seen`, ignored by `load_envelopes` alike.
    fn conversation_name_path(&self, conversation: MessageId) -> PathBuf {
        self.conversation_dir(conversation).join("my-name")
    }

    /// How many of a conversation's stored messages had been rendered at
    /// the last read (project 6 S7) — the unread marker's baseline. Local
    /// presentation state, never transmitted, like `my-name` and
    /// `dismissed.keys`. A **count**, not a timestamp: the store is
    /// append-only, so the count is monotone — a sender's freely chosen
    /// `timestamp_ms` can't mark their message pre-read or pin it unread
    /// (the `first_seen_ms` lesson).
    pub fn set_read_count(&self, conversation: MessageId, count: usize) -> Result<(), Error> {
        let path = self.read_count_path(conversation);
        create_parent(&path)?;
        write_atomic(&path, count.to_string().as_bytes())
            .map_err(|e| Error::Storage(format!("write read count: {e}")))
    }

    pub fn read_count(&self, conversation: MessageId) -> usize {
        std::fs::read_to_string(self.read_count_path(conversation))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    fn read_count_path(&self, conversation: MessageId) -> PathBuf {
        self.conversation_dir(conversation).join("read-count")
    }

    /// All decodable envelopes stored under a conversation, unordered. A
    /// damaged file is skipped with a warning, never fatal: the DAG then
    /// honestly reports the hole as a missing parent / seq gap.
    pub fn load_envelopes(&self, conversation: MessageId) -> Result<Vec<MessageEnvelope>, Error> {
        let dir = self.conversation_dir(conversation);
        let entries =
            std::fs::read_dir(&dir).map_err(|e| Error::Storage(format!("read {dir:?}: {e}")))?;
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
    ) -> Result<MessageEnvelope, Error> {
        let path = self
            .conversation_dir(conversation)
            .join(format!("{}.env", hex(&message.0)));
        let bytes =
            std::fs::read(&path).map_err(|e| Error::Storage(format!("read {path:?}: {e}")))?;
        MessageEnvelope::try_from_bytes(&bytes)
            .map_err(|e| Error::Storage(format!("decode {path:?}: {e}")))
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
    pub fn load_dag(&self, conversation: MessageId) -> Result<ConversationDag, Error> {
        let cores = self
            .load_envelopes(conversation)?
            .into_iter()
            .map(|envelope| envelope.core)
            .collect();
        build_dag(cores, conversation)
    }

    /// `load_dag` over envelopes the caller already holds. Same rebuild,
    /// without reading and BORSH-decoding the whole conversation a second
    /// time: `history` and `conversations` load the envelopes anyway, so
    /// going back to disk for the DAG doubled the cost of every render —
    /// and both run on each `new-messages` event. Clones the cores, which
    /// is the allocation a re-read would have made regardless, minus the
    /// syscalls and the decode.
    pub fn dag_of(
        envelopes: &[MessageEnvelope],
        conversation: MessageId,
    ) -> Result<ConversationDag, Error> {
        let cores = envelopes
            .iter()
            .map(|envelope| envelope.core.clone())
            .collect();
        build_dag(cores, conversation)
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
    pub fn save_blob(&self, hash: &BlobHash, bytes: &[u8]) -> Result<(), Error> {
        let path = self.blob_path(hash);
        create_parent(&path)?;
        write_atomic(&path, bytes).map_err(|e| Error::Storage(format!("write {path:?}: {e}")))
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
    ) -> Result<(), Error> {
        let path = self.outbox_path(message, relay);
        create_parent(&path)?;
        let content = format!("{relay}\n{}\n{created_ms}\n", hex(&conversation.0));
        write_atomic(&path, content.as_bytes())
            .map_err(|e| Error::Storage(format!("write {path:?}: {e}")))
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

    /// Messages with at least one outstanding delivery, each with its
    /// oldest entry's `created_ms` — *since when* it has been owed is what
    /// lets an edge tell in-flight from stuck (R3, relay-lifecycle.md).
    pub fn pending_since(&self) -> BTreeMap<MessageId, u64> {
        let mut since = BTreeMap::new();
        for entry in self.outbox() {
            since
                .entry(entry.message)
                .and_modify(|created: &mut u64| *created = (*created).min(entry.created_ms))
                .or_insert(entry.created_ms);
        }
        since
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

    /// Record recipient devices that confirmed a **durable store** of
    /// `message` — D5's `Stored` ack, surfaced by De7. The outbox's third
    /// rung: `pending` says we still owe a relay, a cleared entry says the
    /// relay took it, and this says the *recipient's own device* has it.
    /// The ack is transient and cannot be recovered afterwards (a message
    /// with no outbox entries is equally direct-acked or deposited fine),
    /// so it is written when it happens or never.
    ///
    /// **Unioned, never replaced:** `deliver` runs again on a flush or a
    /// retry and may reach a device the first pass missed; a later pass
    /// that reaches nobody must not erase what an earlier one earned.
    pub fn add_acks(
        &self,
        conversation: MessageId,
        message: MessageId,
        keys: &BTreeSet<PublicKey>,
    ) -> Result<(), Error> {
        if keys.is_empty() {
            return Ok(());
        }
        let path = self.acks_path(conversation, message);
        let mut all = read_keys(&path);
        all.extend(keys.iter().copied());
        let bytes: Vec<u8> = all.iter().flat_map(|key| key.0).collect();
        create_parent(&path)?;
        write_atomic(&path, &bytes).map_err(|e| Error::Storage(format!("write {path:?}: {e}")))
    }

    /// Every confirmation sidecar in a conversation, by message id — one
    /// directory scan per history render rather than a read per message.
    /// Absent entries are simply absent: no confirmation held is the
    /// default, and never means "not delivered" (De7, tenet 7).
    pub fn acks_in(&self, conversation: MessageId) -> BTreeMap<MessageId, Vec<PublicKey>> {
        let dir = self.conversation_dir(conversation);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return BTreeMap::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "acks") {
                    return None;
                }
                let id = MessageId(crate::hex::parse32(path.file_stem()?.to_str()?).ok()?);
                let keys = read_keys(&path);
                (!keys.is_empty()).then(|| (id, keys.into_iter().collect()))
            })
            .collect()
    }

    /// Beside the envelope, not under `outbox/`: scoped to the conversation
    /// so it dies with it, and `load_envelopes` already ignores anything
    /// that isn't `.env`, so the loader needs no change.
    fn acks_path(&self, conversation: MessageId, message: MessageId) -> PathBuf {
        self.conversation_dir(conversation)
            .join(format!("{}.acks", hex(&message.0)))
    }

    pub fn save_profile(&self, name: &str, relays: &[RelayEntry]) -> Result<(), Error> {
        let name_path = self.root.join("profile.name");
        create_parent(&name_path)?;
        write_atomic(&name_path, name.as_bytes())
            .map_err(|e| Error::Storage(format!("write profile: {e}")))?;
        let specs: Vec<String> = relays.iter().map(RelayEntry::to_spec).collect();
        write_atomic(
            &self.root.join("profile.relays"),
            specs.join("\n").as_bytes(),
        )
        .map_err(|e| Error::Storage(format!("write relays: {e}")))
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

    /// The profile name-attestation's supersession counter (SPEC §3.2):
    /// 0 until the first rename, bumped by `Client::set_profile` on every
    /// name change so receivers holding two claims have a winner (D1b).
    pub fn profile_revision(&self) -> u64 {
        std::fs::read_to_string(self.root.join("profile.revision"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn save_profile_revision(&self, revision: u64) -> Result<(), Error> {
        let path = self.root.join("profile.revision");
        create_parent(&path)?;
        write_atomic(&path, revision.to_string().as_bytes())
            .map_err(|e| Error::Storage(format!("write profile revision: {e}")))
    }

    /// This device's avatar claim materials (D1d): ciphertext hash +
    /// content key + supersession revision, as `profile.avatar`
    /// (`hash-hex\nkey-hex\nrevision`). The ciphertext itself lives in the
    /// blob cache under its hash — like every blob, encrypted at rest.
    pub fn save_avatar_meta(
        &self,
        hash: &BlobHash,
        key: &[u8; 32],
        revision: u64,
    ) -> Result<(), Error> {
        let path = self.root.join("profile.avatar");
        create_parent(&path)?;
        let content = format!("{}\n{}\n{revision}\n", hex(&hash.0), hex(key));
        write_atomic(&path, content.as_bytes())
            .map_err(|e| Error::Storage(format!("write avatar meta: {e}")))
    }

    pub fn avatar_meta(&self) -> Option<(BlobHash, [u8; 32], u64)> {
        let content = std::fs::read_to_string(self.root.join("profile.avatar")).ok()?;
        let mut lines = content.lines();
        let hash = BlobHash(crate::hex::parse32(lines.next()?.trim()).ok()?);
        let key = crate::hex::parse32(lines.next()?.trim()).ok()?;
        let revision = lines.next()?.trim().parse().ok()?;
        Some((hash, key, revision))
    }

    /// This device's self-claimed device label ("phone", "laptop") plus its
    /// own supersession revision (per-claim-kind scope, SPEC §3.2), as
    /// `profile.device` (`label\nrevision`).
    pub fn save_device_label(&self, label: &str, revision: u64) -> Result<(), Error> {
        let path = self.root.join("profile.device");
        create_parent(&path)?;
        write_atomic(&path, format!("{label}\n{revision}\n").as_bytes())
            .map_err(|e| Error::Storage(format!("write device label: {e}")))
    }

    pub fn device_label_meta(&self) -> Option<(String, u64)> {
        let content = std::fs::read_to_string(self.root.join("profile.device")).ok()?;
        let mut lines = content.lines();
        let label = lines.next()?.trim();
        let revision = lines.next()?.trim().parse().ok()?;
        (!label.is_empty()).then(|| (label.to_string(), revision))
    }

    /// Store a contact under a petname. The record is kept in wire form;
    /// the petname is a sibling file (local convention, never protocol).
    pub fn save_contact(&self, petname: &str, record: &ContactRecord) -> Result<(), Error> {
        let stem = self.contact_stem(
            record
                .keys
                .first()
                .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?,
        );
        create_parent(&stem.with_extension("record"))?;
        write_atomic(&stem.with_extension("record"), &record.to_bytes())
            .map_err(|e| Error::Storage(format!("write contact: {e}")))?;
        write_atomic(&stem.with_extension("name"), petname.as_bytes())
            .map_err(|e| Error::Storage(format!("write petname: {e}")))
    }

    /// A local avatar override for a key (U6, my lens): a photo *I* chose for
    /// a contact, stored plaintext on this device only — never sent, never a
    /// claim. `Client::avatar` prefers it over the resolved self-claim.
    pub fn save_local_avatar(&self, key: &PublicKey, bytes: &[u8]) -> Result<(), Error> {
        let path = self.root.join("local-avatars").join(hex(&key.0));
        create_parent(&path)?;
        write_atomic(&path, bytes).map_err(|e| Error::Storage(format!("write local avatar: {e}")))
    }

    /// The local avatar override for a key, if one is set.
    pub fn local_avatar(&self, key: &PublicKey) -> Option<Vec<u8>> {
        std::fs::read(self.root.join("local-avatars").join(hex(&key.0))).ok()
    }

    /// Drop a local avatar override — `avatar` falls back to the self-claim.
    pub fn remove_local_avatar(&self, key: &PublicKey) {
        let _ = std::fs::remove_file(self.root.join("local-avatars").join(hex(&key.0)));
    }

    /// Store a recognized own device (multi-device.md §3): its record plus
    /// the link vouch this device signed over it. Written only by the
    /// recognize act — serving decisions read this store, never the wire.
    pub fn save_recognized_device(
        &self,
        record: &ContactRecord,
        vouch: &SignedAttestation,
    ) -> Result<(), Error> {
        let key = record
            .keys
            .first()
            .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?;
        let stem = self.root.join("devices").join(hex(&key.0));
        create_parent(&stem.with_extension("record"))?;
        write_atomic(&stem.with_extension("record"), &record.to_bytes())
            .map_err(|e| Error::Storage(format!("write device record: {e}")))?;
        write_atomic(&stem.with_extension("vouch"), &vouch.to_bytes())
            .map_err(|e| Error::Storage(format!("write device vouch: {e}")))
    }

    /// Recognized own devices as `(device key, record)`. The key is the one
    /// the recognize act confirmed and vouched — the only key serving
    /// trusts; extra keys a record lists stay advisory (multi-device.md §4).
    /// Damaged entries are skipped with a warning, like `contacts`.
    pub fn recognized_devices(&self) -> Vec<(PublicKey, ContactRecord)> {
        let Ok(entries) = std::fs::read_dir(self.root.join("devices")) else {
            return Vec::new();
        };
        let mut devices = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let record = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).map_err(|e| e.to_string()));
            match record {
                Ok(record) => match record.keys.first() {
                    Some(&key) => devices.push((key, record)),
                    None => tracing::warn!(?path, "skipping keyless device record"),
                },
                Err(err) => tracing::warn!(?path, %err, "skipping damaged device record"),
            }
        }
        devices
    }

    /// The link vouches this device has signed — what `my_record` carries
    /// (SPEC §3.6: links live in the record's attestations).
    pub fn device_vouches(&self) -> Vec<SignedAttestation> {
        let Ok(entries) = std::fs::read_dir(self.root.join("devices")) else {
            return Vec::new();
        };
        let mut vouches = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "vouch") {
                continue;
            }
            let vouch = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    SignedAttestation::try_from_bytes(&bytes).map_err(|e| e.to_string())
                });
            match vouch {
                Ok(vouch) => vouches.push(vouch),
                Err(err) => tracing::warn!(?path, %err, "skipping damaged device vouch"),
            }
        }
        vouches
    }

    /// Store an issued vouch (D4a, web-of-trust.md §2): this device's
    /// signed claim about a contact's key, served as an endorsement with
    /// every `WhoIs` answer about that subject. One per subject; a
    /// re-vouch replaces it (the caller bumps the revision).
    pub fn save_vouch(&self, subject: &PublicKey, vouch: &SignedAttestation) -> Result<(), Error> {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("attestation");
        create_parent(&path)?;
        write_atomic(&path, &vouch.to_bytes())
            .map_err(|e| Error::Storage(format!("write vouch: {e}")))
    }

    /// The issued vouch about a subject, if any.
    pub fn vouch_for(&self, subject: &PublicKey) -> Option<SignedAttestation> {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("attestation");
        std::fs::read(path)
            .ok()
            .and_then(|bytes| SignedAttestation::try_from_bytes(&bytes).ok())
    }

    /// Withdraw a vouch locally: it stops being served, and observers'
    /// per-responder entries replace it away on their next pull. The
    /// *active* disavowal (`Negative`) is D4b.
    pub fn remove_vouch(&self, subject: &PublicKey) {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("attestation");
        let _ = std::fs::remove_file(path);
    }

    /// Store an issued avatar share (project 7 S5): this device's signed
    /// `Avatar` claim about a contact — "the photo I chose for them",
    /// served as an endorsement beside the name vouch. A sibling slot, not
    /// a replacement: name and avatar supersede independently, like the
    /// self-claim kinds (SPEC §3.2).
    pub fn save_avatar_share(
        &self,
        subject: &PublicKey,
        share: &SignedAttestation,
    ) -> Result<(), Error> {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("avatar");
        create_parent(&path)?;
        write_atomic(&path, &share.to_bytes())
            .map_err(|e| Error::Storage(format!("write avatar share: {e}")))
    }

    /// The issued avatar share about a subject, if any.
    pub fn avatar_share_for(&self, subject: &PublicKey) -> Option<SignedAttestation> {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("avatar");
        std::fs::read(path)
            .ok()
            .and_then(|bytes| SignedAttestation::try_from_bytes(&bytes).ok())
    }

    /// Withdraw an avatar share locally — same semantics as `remove_vouch`.
    pub fn remove_avatar_share(&self, subject: &PublicKey) {
        let path = self
            .root
            .join("vouches")
            .join(hex(&subject.0))
            .with_extension("avatar");
        let _ = std::fs::remove_file(path);
    }

    /// Every issued avatar share (S5) — what the startup re-push keeps
    /// alive on the home relay caches.
    pub fn issued_avatar_shares(&self) -> Vec<SignedAttestation> {
        let Ok(entries) = std::fs::read_dir(self.root.join("vouches")) else {
            return Vec::new();
        };
        let mut shares = Vec::new();
        for entry in entries.flatten() {
            if entry.path().extension().is_none_or(|ext| ext != "avatar") {
                continue;
            }
            let Some(signed) = std::fs::read(entry.path())
                .ok()
                .and_then(|bytes| SignedAttestation::try_from_bytes(&bytes).ok())
            else {
                continue;
            };
            shares.push(signed);
        }
        shares
    }

    /// Every issued `Negative` stance (D4b) — what `my_record` publishes
    /// so contacts learn a repudiation from any freshness pull on *us*.
    pub fn issued_negatives(&self) -> Vec<SignedAttestation> {
        let Ok(entries) = std::fs::read_dir(self.root.join("vouches")) else {
            return Vec::new();
        };
        let mut negatives = Vec::new();
        for entry in entries.flatten() {
            let Some(signed) = std::fs::read(entry.path())
                .ok()
                .and_then(|bytes| SignedAttestation::try_from_bytes(&bytes).ok())
            else {
                continue;
            };
            if matches!(signed.attestation.claim, Claim::Negative) {
                negatives.push(signed);
            }
        }
        negatives
    }

    /// Drop a device from the own-devices store (D4b: repudiating a
    /// sibling un-recognizes it — serving, send-to-self, and re-wrap all
    /// stop reading it from here on).
    pub fn remove_recognized_device(&self, key: &PublicKey) {
        let stem = self.root.join("devices").join(hex(&key.0));
        let _ = std::fs::remove_file(stem.with_extension("record"));
        let _ = std::fs::remove_file(stem.with_extension("vouch"));
    }

    /// Replace a stored contact's record — the explicit key-overlap update
    /// (multi-device.md §4). The stem derives from the record's first key,
    /// so a reordered/re-keyed record lands under a new stem: write the new
    /// entry first, then drop the old files — a crash in between leaves a
    /// duplicate to clean up, never a lost contact.
    pub fn replace_contact(
        &self,
        old: &ContactRecord,
        petname: &str,
        new: &ContactRecord,
    ) -> Result<(), Error> {
        self.save_contact(petname, new)?;
        let (Some(old_key), Some(new_key)) = (old.keys.first(), new.keys.first()) else {
            return Ok(());
        };
        // An explicit record update supersedes any manual relay patch (R5):
        // the override was set against the *old* anchor, and keeping it
        // would silently shadow the truth the user just adopted.
        self.clear_relay_override(old_key);
        self.clear_relay_override(new_key);
        if old_key != new_key {
            let stem = self.contact_stem(old_key);
            let _ = std::fs::remove_file(stem.with_extension("record"));
            let _ = std::fs::remove_file(stem.with_extension("name"));
            // A re-keyed entry keeps its clustering (S2): person members
            // reference the stem, so the stem move must follow.
            self.move_person_member(old_key, new_key);
        }
        Ok(())
    }

    /// The manual relay override for a contact, if set (R5,
    /// relay-lifecycle.md): specs stored *beside* the record — never
    /// inside it, the scanned record stays immutable evidence. Keyed by
    /// the record's first key, like the record itself.
    pub fn relay_override(&self, record: Option<&ContactRecord>) -> Option<Vec<RelayEntry>> {
        let key = record?.keys.first()?;
        let content =
            std::fs::read_to_string(self.contact_stem(key).with_extension("relays")).ok()?;
        let relays: Vec<RelayEntry> = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(RelayEntry::from_spec)
            .collect();
        (!relays.is_empty()).then_some(relays)
    }

    pub fn save_relay_override(&self, key: &PublicKey, relays: &[RelayEntry]) -> Result<(), Error> {
        let specs: Vec<String> = relays.iter().map(RelayEntry::to_spec).collect();
        std::fs::write(
            self.contact_stem(key).with_extension("relays"),
            specs.join("\n"),
        )
        .map_err(|e| Error::Storage(format!("write relay override: {e}")))
    }

    pub fn clear_relay_override(&self, key: &PublicKey) {
        let _ = std::fs::remove_file(self.contact_stem(key).with_extension("relays"));
    }

    /// Persist a person entry (project 7 S2): the local lens grouping
    /// contact entries under one label — `persons/<id>` holds the label
    /// line, then one member stem key (hex) per line. The id is the
    /// client's drawn `PersonId` as its raw `u128` — taking the number,
    /// not a string, keeps filenames well-formed by construction. Ids are
    /// opaque local tokens, never derived from member keys or content
    /// (clusters merge, split, and rename). Never on the wire.
    pub fn save_person(&self, id: u128, label: &str, members: &[PublicKey]) -> Result<(), Error> {
        let path = self.root.join("persons").join(format!("{id:032x}"));
        create_parent(&path)?;
        let mut content = String::from(label);
        for member in members {
            content.push('\n');
            content.push_str(&hex(&member.0));
        }
        write_atomic(&path, content.as_bytes())
            .map_err(|e| Error::Storage(format!("write person: {e}")))
    }

    pub fn remove_person(&self, id: u128) {
        let _ = std::fs::remove_file(self.root.join("persons").join(format!("{id:032x}")));
    }

    /// Persisted person entries as `(id, label, member stem keys)`. Damaged
    /// entries are skipped with a warning, like `contacts`; membership
    /// against the live contact store is the client's read-time concern.
    pub fn persons(&self) -> Vec<(u128, String, Vec<PublicKey>)> {
        let Ok(entries) = std::fs::read_dir(self.root.join("persons")) else {
            return Vec::new();
        };
        let mut persons = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_person_id(&name) else {
                if !name.starts_with('.') {
                    tracing::warn!(?name, "skipping person entry with a malformed id");
                }
                continue;
            };
            if let Some((label, members)) = read_person(&entry.path()) {
                persons.push((id, label, members));
            }
        }
        persons.sort_by_key(|&(id, ..)| id);
        persons
    }

    /// Drain person files from before drawn ids (the brief counter era of
    /// project 7 — `p1`-style names, plus the `.next` counter file) so the
    /// client can re-mint them under real ids, labels and clustering
    /// intact. Empty on current stores.
    pub fn take_legacy_persons(&self) -> Vec<(String, Vec<PublicKey>)> {
        let dir = self.root.join("persons");
        let _ = std::fs::remove_file(dir.join(".next"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut legacy = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || parse_person_id(&name).is_some() {
                continue;
            }
            if let Some(person) = read_person(&entry.path()) {
                legacy.push(person);
            }
            let _ = std::fs::remove_file(entry.path());
        }
        legacy
    }

    /// Re-point person memberships from one member stem to another — the
    /// record-update companion (`replace_contact`): a re-keyed entry must
    /// stay exactly as clustered as it was (S2's no-dangle rule).
    fn move_person_member(&self, old: &PublicKey, new: &PublicKey) {
        for (id, label, mut members) in self.persons() {
            if let Some(slot) = members.iter_mut().find(|member| *member == old) {
                *slot = *new;
                if let Err(error) = self.save_person(id, &label, &members) {
                    tracing::warn!(%error, id, "could not re-point a person membership");
                }
            }
        }
    }

    /// All stored contacts as `(petname, record)`, petname-sorted.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, Error> {
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

    /// Store a `who-is` answer (D1b, who-is-this.md §5):
    /// `learned/<subject>/<responder>.record` + a receipt-time sibling +
    /// the responder's caller-validated endorsements (D4a). Latest answer
    /// per responder wins — the whole entry replaces, endorsements
    /// included, so a withdrawn vouch disappears with the next freshness
    /// pull; nothing else is ever overwritten, and the contact store is
    /// never touched by this path. Learned records don't get petnames,
    /// aren't served onward, and don't open the D0c serving gate —
    /// advisory input with provenance, not contacts.
    pub fn save_learned(
        &self,
        subject: &PublicKey,
        responder: &PublicKey,
        record: &ContactRecord,
        endorsements: &[SignedAttestation],
        received_ms: u64,
    ) -> Result<(), Error> {
        let stem = self
            .root
            .join("learned")
            .join(hex(&subject.0))
            .join(hex(&responder.0));
        create_parent(&stem.with_extension("record"))?;
        write_atomic(&stem.with_extension("record"), &record.to_bytes())
            .map_err(|e| Error::Storage(format!("write learned record: {e}")))?;
        let path = stem.with_extension("endorsements");
        if endorsements.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            write_atomic(&path, &encode_attestations(endorsements))
                .map_err(|e| Error::Storage(format!("write endorsements: {e}")))?;
        }
        write_atomic(
            &stem.with_extension("time"),
            received_ms.to_string().as_bytes(),
        )
        .map_err(|e| Error::Storage(format!("write learned time: {e}")))
    }

    /// Every learned record for a subject, unordered. Damaged entries are
    /// skipped with a warning — advisory data, never fatal.
    pub fn learned(&self, subject: &PublicKey) -> Vec<LearnedRecord> {
        let dir = self.root.join("learned").join(hex(&subject.0));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut learned = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "record") {
                continue;
            }
            let responder = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| crate::hex::parse32(stem).ok())
                .map(PublicKey);
            let record = std::fs::read(&path)
                .ok()
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).ok());
            let (Some(responder), Some(record)) = (responder, record) else {
                tracing::warn!(?path, "skipping a damaged learned entry");
                continue;
            };
            let received_ms = std::fs::read_to_string(path.with_extension("time"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let endorsements = std::fs::read(path.with_extension("endorsements"))
                .map(|bytes| decode_attestations(&bytes))
                .unwrap_or_default();
            learned.push(LearnedRecord {
                responder,
                record,
                endorsements,
                received_ms,
            });
        }
        learned
    }

    /// Dismiss an unknown key (D2c, groups.md §5): the "ignore" side of
    /// the wild-key popup — pure presentation policy, persisted so the
    /// popup doesn't nag every open. One hex key per line; idempotent.
    pub fn dismiss_key(&self, key: &PublicKey) -> Result<(), Error> {
        let mut dismissed = self.dismissed_keys();
        if !dismissed.insert(*key) {
            return Ok(());
        }
        let path = self.root.join("dismissed.keys");
        create_parent(&path)?;
        let content: String = dismissed
            .iter()
            .map(|key| format!("{}\n", hex(&key.0)))
            .collect();
        write_atomic(&path, content.as_bytes())
            .map_err(|e| Error::Storage(format!("write dismissed keys: {e}")))
    }

    pub fn dismissed_keys(&self) -> BTreeSet<PublicKey> {
        std::fs::read_to_string(self.root.join("dismissed.keys"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| crate::hex::parse32(line.trim()).ok())
            .map(PublicKey)
            .collect()
    }

    /// Peers whose last direct dial got nowhere, and when (De6b):
    /// `<hex key> <wall-clock ms>` per line, replacing the file wholesale.
    ///
    /// Dumb storage on purpose — *which* entries are worth keeping is the
    /// caller's policy (the dial cooldown lives in `client`), so this writes
    /// exactly what it is given and reads back exactly what is there.
    /// Wall-clock, like every other persisted timestamp (the B5 lesson:
    /// `Instant` doesn't serialize).
    pub fn save_unreachable(&self, entries: &[([u8; 32], u64)]) -> Result<(), Error> {
        let path = self.root.join("unreachable.keys");
        create_parent(&path)?;
        let content: String = entries
            .iter()
            .map(|(key, at_ms)| format!("{} {at_ms}\n", hex(key)))
            .collect();
        write_atomic(&path, content.as_bytes())
            .map_err(|e| Error::Storage(format!("write unreachable keys: {e}")))
    }

    /// Reads back `save_unreachable`. Unparseable lines are skipped rather
    /// than fatal: this is a cache of negative evidence, and the worst a lost
    /// entry costs is one extra dial.
    pub fn unreachable(&self) -> Vec<([u8; 32], u64)> {
        std::fs::read_to_string(self.root.join("unreachable.keys"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (key, at_ms) = line.trim().split_once(' ')?;
                Some((crate::hex::parse32(key).ok()?, at_ms.parse().ok()?))
            })
            .collect()
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

/// One learned record (D1b): an answer `responder` served for a subject
/// via `who-is`. Multiple per subject is the data model — advisory inputs
/// ranked at read time (who-is-this.md §7), never merged or promoted
/// implicitly.
pub struct LearnedRecord {
    pub responder: PublicKey,
    pub record: ContactRecord,
    /// The responder's own claims about the subject (D4a) — validated at
    /// receipt (attester == responder, subject matches, signature).
    pub endorsements: Vec<SignedAttestation>,
    /// Local receipt time (ms) — orders answers *within* a provenance
    /// class; trusted because we wrote it, unlike anything in the record.
    pub received_ms: u64,
}

/// Local file framing for attestation lists (u32-LE length + wire bytes
/// per entry) — a storage convention, never on the wire; damaged tails
/// are skipped like every advisory store.
fn encode_attestations(list: &[SignedAttestation]) -> Vec<u8> {
    let mut out = Vec::new();
    for signed in list {
        let bytes = signed.to_bytes();
        out.extend((bytes.len() as u32).to_le_bytes());
        out.extend(bytes);
    }
    out
}

fn decode_attestations(bytes: &[u8]) -> Vec<SignedAttestation> {
    let mut list = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 4 {
        let len = u32::from_le_bytes(rest[..4].try_into().expect("4 bytes")) as usize;
        rest = &rest[4..];
        if rest.len() < len {
            break;
        }
        match SignedAttestation::try_from_bytes(&rest[..len]) {
            Ok(signed) => list.push(signed),
            Err(_) => tracing::warn!("skipping a damaged stored attestation"),
        }
        rest = &rest[len..];
    }
    list
}

/// The shared rebuild behind `load_dag` / `dag_of`. A missing genesis is
/// the only unrecoverable case; individual invalid messages are skipped so
/// one bad file can't hide a whole conversation.
fn build_dag(
    mut cores: Vec<MessageCore>,
    conversation: MessageId,
) -> Result<ConversationDag, Error> {
    let genesis_at = cores
        .iter()
        .position(|core| core.conversation.is_none())
        .ok_or_else(|| {
            Error::Conversation(format!(
                "conversation {} has no genesis on disk",
                hex(&conversation.0)
            ))
        })?;
    let mut dag = ConversationDag::new(cores.swap_remove(genesis_at))
        .map_err(|e| Error::Conversation(format!("stored genesis invalid: {e}")))?;
    for core in cores {
        if let Err(e) = dag.insert(core) {
            tracing::warn!(error = %e, "skipping invalid stored message");
        }
    }
    Ok(dag)
}

/// Keys from an `.acks` sidecar (concatenated raw 32-byte keys). A damaged
/// tail is dropped like every advisory store — a lost confirmation renders
/// as *no* confirmation, which is already the honest default (De7).
fn read_keys(path: &std::path::Path) -> BTreeSet<PublicKey> {
    std::fs::read(path)
        .map(|bytes| {
            bytes
                .chunks_exact(32)
                .map(|chunk| PublicKey(chunk.try_into().expect("32 bytes")))
                .collect()
        })
        .unwrap_or_default()
}

fn create_parent(path: &std::path::Path) -> Result<(), Error> {
    let parent = path.parent().expect("state paths always have a parent");
    std::fs::create_dir_all(parent).map_err(|e| Error::Storage(format!("create {parent:?}: {e}")))
}

/// A person filename is exactly the raw id as 32 hex chars (`{:032x}`).
fn parse_person_id(name: &str) -> Option<u128> {
    (name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(name, 16).ok())
        .flatten()
}

/// One person file's content: the label line, then member stem keys.
fn read_person(path: &std::path::Path) -> Option<(String, Vec<PublicKey>)> {
    let Ok(content) = std::fs::read_to_string(path) else {
        tracing::warn!(?path, "skipping unreadable person entry");
        return None;
    };
    let mut lines = content.lines();
    let Some(label) = lines
        .next()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    else {
        tracing::warn!(?path, "skipping person entry with no label");
        return None;
    };
    let members: Vec<PublicKey> = lines
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| crate::hex::parse32(line.trim()).ok().map(PublicKey))
        .collect();
    Some((label.to_string(), members))
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
