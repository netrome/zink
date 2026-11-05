//! Client-side persistence (slice B5): conversations on disk, so the CLI
//! can thread messages instead of sending standalone geneses.
//!
//! Since project 9 this file is the typed layer over the storage port
//! (`crate::ports::store`): layout, encoding, and cross-key invariants,
//! no I/O. The layout below is the key layout; `FsStore` roots it at
//! `<key-file>.state/`, one key segment per path component.
//!
//! Layout:
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
use std::path::Path;

use crate::adapters::fs_store::FsStore;
use crate::error::Error;
use crate::ports::store::{Key, Segment, StorageError, Store};
use zink_protocol::{
    BlobHash, Claim, ContactRecord, ConversationDag, MessageCore, MessageEnvelope, MessageId,
    PublicKey, RelayEntry, SignedAttestation,
};

/// The typed layer over a byte store: layout (which key holds what),
/// encoding, and the invariants that span keys. Cheap to clone — clones
/// share the store.
#[derive(Clone, Debug)]
pub struct ClientState<S = FsStore> {
    store: S,
}

impl ClientState {
    /// State lives next to the key file: `<key-file>.state/`.
    pub fn open(key_path: &str) -> Self {
        Self::new(FsStore::new(format!("{key_path}.state")))
    }
}

impl<S> ClientState<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The store underneath — for tests that reopen a client on the same
    /// state, or reach under the typed layer to damage it.
    #[cfg(test)]
    pub(crate) fn store(&self) -> &S {
        &self.store
    }
}

impl<S: Store> ClientState<S> {
    /// The conversation this participant set maps to, if any.
    pub async fn conversation_for(&self, participants: &BTreeSet<PublicKey>) -> Option<MessageId> {
        let bytes = self.read(&participants_path(participants)).await?;
        Some(MessageId(bytes.try_into().ok()?))
    }

    pub async fn record_conversation(
        &self,
        participants: &BTreeSet<PublicKey>,
        conversation: MessageId,
    ) -> Result<(), Error> {
        self.write(&participants_path(participants), &conversation.0)
            .await
    }

    /// Persist an envelope under its conversation. Idempotent (the key is
    /// the message id). `now_ms` is this device's wall clock, kept as the
    /// conversation's first-seen time if it has none yet.
    pub async fn store_envelope(
        &self,
        conversation: MessageId,
        envelope: &MessageEnvelope,
        now_ms: u64,
    ) -> Result<(), Error> {
        let path = format!(
            "{}/{}.env",
            conversation_dir(conversation),
            hex(&envelope.id().0)
        );
        self.write(&path, &envelope.to_bytes()).await?;
        self.note_first_seen(conversation, now_ms).await;
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
    pub async fn first_seen_ms(&self, conversation: MessageId) -> u64 {
        self.read_number(&first_seen_path(conversation))
            .await
            .unwrap_or(0)
    }

    /// Write the marker if absent. Best-effort: losing it costs ordering
    /// precision in one view, never a message.
    async fn note_first_seen(&self, conversation: MessageId, now_ms: u64) {
        let path = first_seen_path(conversation);
        if self.read(&path).await.is_some() {
            return;
        }
        if let Err(error) = self.write(&path, now_ms.to_string().as_bytes()).await {
            tracing::debug!(%error, "could not record a conversation's first-seen time");
        }
    }

    /// My local name for a conversation (project 6 S6) — the petname's
    /// conversation-shaped sibling: pure presentation policy, never
    /// transmitted. `None` clears. Stored as **my lens** (`my-name`),
    /// deliberately distinct from any future peer-*suggestion* store —
    /// the anchor-vs-learned split the contact store uses.
    pub async fn set_conversation_name(
        &self,
        conversation: MessageId,
        name: Option<&str>,
    ) -> Result<(), Error> {
        let path = conversation_name_path(conversation);
        match name {
            None => {
                self.remove(&path).await;
                Ok(())
            }
            Some(name) => self.write(&path, name.as_bytes()).await,
        }
    }

    pub async fn conversation_name(&self, conversation: MessageId) -> Option<String> {
        self.read_text(&conversation_name_path(conversation))
            .await
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// How many of a conversation's stored messages had been rendered at
    /// the last read (project 6 S7) — the unread marker's baseline. Local
    /// presentation state, never transmitted, like `my-name` and
    /// `dismissed.keys`. A **count**, not a timestamp: the store is
    /// append-only, so the count is monotone — a sender's freely chosen
    /// `timestamp_ms` can't mark their message pre-read or pin it unread
    /// (the `first_seen_ms` lesson).
    pub async fn set_read_count(&self, conversation: MessageId, count: usize) -> Result<(), Error> {
        self.write(&read_count_path(conversation), count.to_string().as_bytes())
            .await
    }

    pub async fn read_count(&self, conversation: MessageId) -> usize {
        self.read_number(&read_count_path(conversation))
            .await
            .unwrap_or(0)
    }

    /// All decodable envelopes stored under a conversation, unordered. A
    /// damaged entry is skipped with a warning, never fatal: the DAG then
    /// honestly reports the hole as a missing parent / seq gap. A
    /// conversation with nothing stored is an error, like an unknown one.
    pub async fn load_envelopes(
        &self,
        conversation: MessageId,
    ) -> Result<Vec<MessageEnvelope>, Error> {
        let dir = conversation_dir(conversation);
        let names = self.store.list(&key(&dir)).await.map_err(storage)?;
        if names.is_empty() {
            return Err(Error::Storage(format!("no stored conversation at {dir}")));
        }
        let mut envelopes = Vec::new();
        for name in names {
            if extension(&name) != Some("env") {
                continue; // a sidecar: `.acks`, `first-seen`, `my-name`, …
            }
            let path = format!("{dir}/{name}");
            let envelope = self
                .read(&path)
                .await
                .ok_or_else(|| "unreadable".to_string())
                .and_then(|bytes| {
                    MessageEnvelope::try_from_bytes(&bytes).map_err(|e| e.to_string())
                });
            match envelope {
                Ok(envelope) => envelopes.push(envelope),
                Err(e) => tracing::warn!(path, error = %e, "skipping damaged entry"),
            }
        }
        Ok(envelopes)
    }

    /// One stored envelope by conversation + message id.
    pub async fn load_envelope(
        &self,
        conversation: MessageId,
        message: MessageId,
    ) -> Result<MessageEnvelope, Error> {
        let path = format!("{}/{}.env", conversation_dir(conversation), hex(&message.0));
        let bytes = self
            .store
            .get(&key(&path))
            .await
            .map_err(storage)?
            .ok_or_else(|| Error::Storage(format!("no envelope at {path}")))?;
        MessageEnvelope::try_from_bytes(&bytes)
            .map_err(|e| Error::Storage(format!("decode {path}: {e}")))
    }

    /// Every conversation with stored envelopes, sorted by id (the caller
    /// orders for display — id order is just deterministic).
    pub async fn conversations(&self) -> Vec<MessageId> {
        self.names("conversations")
            .await
            .iter()
            .filter_map(|name| crate::hex::parse32(name.as_str()).ok().map(MessageId))
            .collect()
    }

    /// Rebuild the DAG from the stored envelopes. Order in the store is
    /// irrelevant — the DAG accepts children before parents. Only a
    /// missing genesis is unrecoverable (there is no root to build on).
    pub async fn load_dag(&self, conversation: MessageId) -> Result<ConversationDag, Error> {
        let cores = self
            .load_envelopes(conversation)
            .await?
            .into_iter()
            .map(|envelope| envelope.core)
            .collect();
        build_dag(cores, conversation)
    }

    /// One stored envelope by message id, wherever it lives. Content is
    /// addressed by id alone (SPEC §5.2 `get`), but in the store it's filed
    /// under a conversation, so this scans conversations — fine at
    /// friend/family scale (few conversations); an id→conversation index is
    /// the optimization if a large store ever makes the scan bite.
    pub async fn find_envelope(&self, id: MessageId) -> Option<MessageEnvelope> {
        for conversation in self.conversations().await {
            if let Ok(envelope) = self.load_envelope(conversation, id).await {
                return Some(envelope);
            }
        }
        None
    }

    /// Ids of held messages whose `parents` include `parent` (SPEC §5.2
    /// `get-successors`) — known children, for pulling a conversation forward.
    pub async fn successors(&self, parent: MessageId) -> Vec<MessageId> {
        let mut ids = Vec::new();
        for conversation in self.conversations().await {
            let envelopes = self.load_envelopes(conversation).await.unwrap_or_default();
            ids.extend(
                envelopes
                    .iter()
                    .filter(|envelope| envelope.core.parents.contains(&parent))
                    .map(MessageEnvelope::id),
            );
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// Referenced parents we don't hold for `conversation` — the frontier a
    /// backfill fetches to walk back toward the genesis. Empty when the stored
    /// slice is already ancestor-closed (genesis reached, or nothing stored).
    pub async fn missing_ancestors(&self, conversation: MessageId) -> Vec<MessageId> {
        let envelopes = self.load_envelopes(conversation).await.unwrap_or_default();
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
    pub async fn save_blob(&self, hash: &BlobHash, bytes: &[u8]) -> Result<(), Error> {
        self.write(&blob_path(hash), bytes).await
    }

    /// A cached encrypted blob, if present. The caller verifies + decrypts
    /// against the referencing envelope — the cache is trusted no more than
    /// a relay would be.
    pub async fn load_blob(&self, hash: &BlobHash) -> Option<Vec<u8>> {
        self.read(&blob_path(hash)).await
    }

    /// Record that `message` still owes `relay` a deposit (and blob pushes).
    /// Written *before* any network work, so a crash mid-send leaves the
    /// ledger honest. Idempotent (same key, same content).
    pub async fn add_outbox(
        &self,
        message: MessageId,
        relay: &str,
        conversation: MessageId,
        created_ms: u64,
    ) -> Result<(), Error> {
        let content = format!("{relay}\n{}\n{created_ms}\n", hex(&conversation.0));
        self.write(&outbox_path(message, relay), content.as_bytes())
            .await
    }

    /// Delivery to `relay` succeeded — drop the entry. Missing is fine
    /// (already cleared).
    pub async fn clear_outbox(&self, message: MessageId, relay: &str) {
        self.remove(&outbox_path(message, relay)).await;
    }

    /// Every outstanding delivery, oldest first. Damaged entries are
    /// removed with a warning — an unparseable ledger line can't be
    /// retried anyway.
    pub async fn outbox(&self) -> Vec<OutboxEntry> {
        let mut outbox = Vec::new();
        for name in self.names("outbox").await {
            let path = format!("outbox/{name}");
            let content = self.read_text(&path).await;
            match content.and_then(|content| parse_outbox_entry(name.as_str(), &content)) {
                Some(entry) => outbox.push(entry),
                None => {
                    tracing::warn!(path, "dropping damaged outbox entry");
                    self.remove(&path).await;
                }
            }
        }
        outbox.sort_by_key(|entry| entry.created_ms);
        outbox
    }

    /// Messages with at least one outstanding delivery, each with its
    /// oldest entry's `created_ms` — *since when* it has been owed is what
    /// lets an edge tell in-flight from stuck (R3, relay-lifecycle.md).
    pub async fn pending_since(&self) -> BTreeMap<MessageId, u64> {
        let mut since = BTreeMap::new();
        for entry in self.outbox().await {
            since
                .entry(entry.message)
                .and_modify(|created: &mut u64| *created = (*created).min(entry.created_ms))
                .or_insert(entry.created_ms);
        }
        since
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
    pub async fn add_acks(
        &self,
        conversation: MessageId,
        message: MessageId,
        keys: &BTreeSet<PublicKey>,
    ) -> Result<(), Error> {
        if keys.is_empty() {
            return Ok(());
        }
        let path = acks_path(conversation, message);
        let mut all = self.read(&path).await.map(parse_keys).unwrap_or_default();
        all.extend(keys.iter().copied());
        let bytes: Vec<u8> = all.iter().flat_map(|key| key.0).collect();
        self.write(&path, &bytes).await
    }

    /// Every confirmation sidecar in a conversation, by message id — one
    /// listing per history render rather than a read per message.
    /// Absent entries are simply absent: no confirmation held is the
    /// default, and never means "not delivered" (De7, tenet 7).
    pub async fn acks_in(&self, conversation: MessageId) -> BTreeMap<MessageId, Vec<PublicKey>> {
        let dir = conversation_dir(conversation);
        let mut acks = BTreeMap::new();
        for name in self.names(&dir).await {
            if extension(&name) != Some("acks") {
                continue;
            }
            let Some(id) = stem(&name).and_then(|stem| crate::hex::parse32(stem).ok()) else {
                continue;
            };
            let keys = self
                .read(&format!("{dir}/{name}"))
                .await
                .map(parse_keys)
                .unwrap_or_default();
            if !keys.is_empty() {
                acks.insert(MessageId(id), keys.into_iter().collect());
            }
        }
        acks
    }

    pub async fn save_profile(&self, name: &str, relays: &[RelayEntry]) -> Result<(), Error> {
        self.write("profile.name", name.as_bytes()).await?;
        let specs: Vec<String> = relays.iter().map(RelayEntry::to_spec).collect();
        self.write("profile.relays", specs.join("\n").as_bytes())
            .await
    }

    pub async fn profile_name(&self) -> Option<String> {
        let name = self.read_text("profile.name").await?;
        (!name.trim().is_empty()).then(|| name.trim().to_string())
    }

    /// The home relay services, one spec line per entry (`dial[#relay-url]`).
    pub async fn home_relay_entries(&self) -> Vec<RelayEntry> {
        parse_relay_specs(&self.read_text("profile.relays").await.unwrap_or_default())
    }

    /// The home relays' mailbox dial strings — what every mailbox path
    /// (deposit fan-out, recv, subscribe, outbox keys) runs on.
    pub async fn home_relays(&self) -> Vec<String> {
        self.home_relay_entries()
            .await
            .into_iter()
            .map(|entry| entry.mailbox)
            .collect()
    }

    /// The profile name-attestation's supersession counter (SPEC §3.2):
    /// 0 until the first rename, bumped by `Client::set_profile` on every
    /// name change so receivers holding two claims have a winner (D1b).
    pub async fn profile_revision(&self) -> u64 {
        self.read_number("profile.revision").await.unwrap_or(0)
    }

    pub async fn save_profile_revision(&self, revision: u64) -> Result<(), Error> {
        self.write("profile.revision", revision.to_string().as_bytes())
            .await
    }

    /// This device's avatar claim materials (D1d): ciphertext hash +
    /// content key + supersession revision, as `profile.avatar`
    /// (`hash-hex\nkey-hex\nrevision`). The ciphertext itself lives in the
    /// blob cache under its hash — like every blob, encrypted at rest.
    pub async fn save_avatar_meta(
        &self,
        hash: &BlobHash,
        key: &[u8; 32],
        revision: u64,
    ) -> Result<(), Error> {
        let content = format!("{}\n{}\n{revision}\n", hex(&hash.0), hex(key));
        self.write("profile.avatar", content.as_bytes()).await
    }

    pub async fn avatar_meta(&self) -> Option<(BlobHash, [u8; 32], u64)> {
        let content = self.read_text("profile.avatar").await?;
        let mut lines = content.lines();
        let hash = BlobHash(crate::hex::parse32(lines.next()?.trim()).ok()?);
        let key = crate::hex::parse32(lines.next()?.trim()).ok()?;
        let revision = lines.next()?.trim().parse().ok()?;
        Some((hash, key, revision))
    }

    /// This device's self-claimed device label ("phone", "laptop") plus its
    /// own supersession revision (per-claim-kind scope, SPEC §3.2), as
    /// `profile.device` (`label\nrevision`).
    pub async fn save_device_label(&self, label: &str, revision: u64) -> Result<(), Error> {
        self.write(
            "profile.device",
            format!("{label}\n{revision}\n").as_bytes(),
        )
        .await
    }

    pub async fn device_label_meta(&self) -> Option<(String, u64)> {
        let content = self.read_text("profile.device").await?;
        let mut lines = content.lines();
        let label = lines.next()?.trim();
        let revision = lines.next()?.trim().parse().ok()?;
        (!label.is_empty()).then(|| (label.to_string(), revision))
    }

    /// Store a contact under a petname. The record is kept in wire form;
    /// the petname is a sibling entry (local convention, never protocol).
    pub async fn save_contact(&self, petname: &str, record: &ContactRecord) -> Result<(), Error> {
        let stem = contact_stem(
            record
                .keys
                .first()
                .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?,
        );
        self.write(&format!("{stem}.record"), &record.to_bytes())
            .await?;
        self.write(&format!("{stem}.name"), petname.as_bytes())
            .await
    }

    /// A local avatar override for a key (U6, my lens): a photo *I* chose for
    /// a contact, stored plaintext on this device only — never sent, never a
    /// claim. `Client::avatar` prefers it over the resolved self-claim.
    pub async fn save_local_avatar(&self, key: &PublicKey, bytes: &[u8]) -> Result<(), Error> {
        self.write(&local_avatar_path(key), bytes).await
    }

    /// The local avatar override for a key, if one is set.
    pub async fn local_avatar(&self, key: &PublicKey) -> Option<Vec<u8>> {
        self.read(&local_avatar_path(key)).await
    }

    /// Drop a local avatar override — `avatar` falls back to the self-claim.
    pub async fn remove_local_avatar(&self, key: &PublicKey) {
        self.remove(&local_avatar_path(key)).await;
    }

    /// Store a recognized own device (multi-device.md §3): its record plus
    /// the link vouch this device signed over it. Written only by the
    /// recognize act — serving decisions read this store, never the wire.
    pub async fn save_recognized_device(
        &self,
        record: &ContactRecord,
        vouch: &SignedAttestation,
    ) -> Result<(), Error> {
        let key = record
            .keys
            .first()
            .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?;
        let stem = device_stem(key);
        self.write(&format!("{stem}.record"), &record.to_bytes())
            .await?;
        self.write(&format!("{stem}.vouch"), &vouch.to_bytes())
            .await
    }

    /// Recognized own devices as `(device key, record)`. The key is the one
    /// the recognize act confirmed and vouched — the only key serving
    /// trusts; extra keys a record lists stay advisory (multi-device.md §4).
    /// Damaged entries are skipped with a warning, like `contacts`.
    pub async fn recognized_devices(&self) -> Vec<(PublicKey, ContactRecord)> {
        let mut devices = Vec::new();
        for name in self.names("devices").await {
            if extension(&name) != Some("record") {
                continue;
            }
            let path = format!("devices/{name}");
            let record = self
                .read(&path)
                .await
                .ok_or_else(|| "unreadable".to_string())
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).map_err(|e| e.to_string()));
            match record {
                Ok(record) => match record.keys.first() {
                    Some(&key) => devices.push((key, record)),
                    None => tracing::warn!(path, "skipping keyless device record"),
                },
                Err(err) => tracing::warn!(path, %err, "skipping damaged device record"),
            }
        }
        devices
    }

    /// The link vouches this device has signed — what `my_record` carries
    /// (SPEC §3.6: links live in the record's attestations).
    pub async fn device_vouches(&self) -> Vec<SignedAttestation> {
        let mut vouches = Vec::new();
        for name in self.names("devices").await {
            if extension(&name) != Some("vouch") {
                continue;
            }
            let path = format!("devices/{name}");
            let vouch = self
                .read(&path)
                .await
                .ok_or_else(|| "unreadable".to_string())
                .and_then(|bytes| {
                    SignedAttestation::try_from_bytes(&bytes).map_err(|e| e.to_string())
                });
            match vouch {
                Ok(vouch) => vouches.push(vouch),
                Err(err) => tracing::warn!(path, %err, "skipping damaged device vouch"),
            }
        }
        vouches
    }

    /// Store an issued vouch (D4a, web-of-trust.md §2): this device's
    /// signed claim about a contact's key, served as an endorsement with
    /// every `WhoIs` answer about that subject. One per subject; a
    /// re-vouch replaces it (the caller bumps the revision).
    pub async fn save_vouch(
        &self,
        subject: &PublicKey,
        vouch: &SignedAttestation,
    ) -> Result<(), Error> {
        self.write(&vouch_path(subject, "attestation"), &vouch.to_bytes())
            .await
    }

    /// The issued vouch about a subject, if any.
    pub async fn vouch_for(&self, subject: &PublicKey) -> Option<SignedAttestation> {
        self.read_attestation(&vouch_path(subject, "attestation"))
            .await
    }

    /// Withdraw a vouch locally: it stops being served, and observers'
    /// per-responder entries replace it away on their next pull. The
    /// *active* disavowal (`Negative`) is D4b.
    pub async fn remove_vouch(&self, subject: &PublicKey) {
        self.remove(&vouch_path(subject, "attestation")).await;
    }

    /// Store an issued avatar share (project 7 S5): this device's signed
    /// `Avatar` claim about a contact — "the photo I chose for them",
    /// served as an endorsement beside the name vouch. A sibling slot, not
    /// a replacement: name and avatar supersede independently, like the
    /// self-claim kinds (SPEC §3.2).
    pub async fn save_avatar_share(
        &self,
        subject: &PublicKey,
        share: &SignedAttestation,
    ) -> Result<(), Error> {
        self.write(&vouch_path(subject, "avatar"), &share.to_bytes())
            .await
    }

    /// The issued avatar share about a subject, if any.
    pub async fn avatar_share_for(&self, subject: &PublicKey) -> Option<SignedAttestation> {
        self.read_attestation(&vouch_path(subject, "avatar")).await
    }

    /// Withdraw an avatar share locally — same semantics as `remove_vouch`.
    pub async fn remove_avatar_share(&self, subject: &PublicKey) {
        self.remove(&vouch_path(subject, "avatar")).await;
    }

    /// Every issued avatar share (S5) — what the startup re-push keeps
    /// alive on the home relay caches.
    pub async fn issued_avatar_shares(&self) -> Vec<SignedAttestation> {
        let mut shares = Vec::new();
        for name in self.names("vouches").await {
            if extension(&name) != Some("avatar") {
                continue;
            }
            if let Some(signed) = self.read_attestation(&format!("vouches/{name}")).await {
                shares.push(signed);
            }
        }
        shares
    }

    /// Every issued `Negative` stance (D4b) — what `my_record` publishes
    /// so contacts learn a repudiation from any freshness pull on *us*.
    pub async fn issued_negatives(&self) -> Vec<SignedAttestation> {
        let mut negatives = Vec::new();
        for name in self.names("vouches").await {
            let Some(signed) = self.read_attestation(&format!("vouches/{name}")).await else {
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
    pub async fn remove_recognized_device(&self, key: &PublicKey) {
        let stem = device_stem(key);
        self.remove(&format!("{stem}.record")).await;
        self.remove(&format!("{stem}.vouch")).await;
    }

    /// Replace a stored contact's record — the explicit key-overlap update
    /// (multi-device.md §4). The stem derives from the record's first key,
    /// so a reordered/re-keyed record lands under a new stem: write the new
    /// entry, re-point its person membership, and only then drop the old
    /// one — a crash in between leaves a duplicate to clean up, never a
    /// lost contact and never a member fallen out of its cluster.
    pub async fn replace_contact(
        &self,
        old: &ContactRecord,
        petname: &str,
        new: &ContactRecord,
    ) -> Result<(), Error> {
        self.save_contact(petname, new).await?;
        let (Some(old_key), Some(new_key)) = (old.keys.first(), new.keys.first()) else {
            return Ok(());
        };
        // An explicit record update supersedes any manual relay patch (R5):
        // the override was set against the *old* anchor, and keeping it
        // would silently shadow the truth the user just adopted.
        if old_key != new_key {
            // A re-keyed entry keeps its clustering (S2): person members
            // reference the stem, so the stem move must follow — before the
            // old stem goes, or a crash between strands the new entry
            // outside its person.
            self.move_person_member(old_key, new_key).await;
        }
        self.clear_relay_override(old_key).await;
        self.clear_relay_override(new_key).await;
        if old_key != new_key {
            let stem = contact_stem(old_key);
            self.remove(&format!("{stem}.record")).await;
            self.remove(&format!("{stem}.name")).await;
        }
        Ok(())
    }

    /// The manual relay override for a contact, if set (R5,
    /// relay-lifecycle.md): specs stored *beside* the record — never
    /// inside it, the scanned record stays immutable evidence. Keyed by
    /// the record's first key, like the record itself.
    pub async fn relay_override(&self, record: Option<&ContactRecord>) -> Option<Vec<RelayEntry>> {
        let key = record?.keys.first()?;
        let content = self.read_text(&relay_override_path(key)).await?;
        let relays = parse_relay_specs(&content);
        (!relays.is_empty()).then_some(relays)
    }

    pub async fn save_relay_override(
        &self,
        key: &PublicKey,
        relays: &[RelayEntry],
    ) -> Result<(), Error> {
        let specs: Vec<String> = relays.iter().map(RelayEntry::to_spec).collect();
        self.write(&relay_override_path(key), specs.join("\n").as_bytes())
            .await
    }

    pub async fn clear_relay_override(&self, key: &PublicKey) {
        self.remove(&relay_override_path(key)).await;
    }

    /// Persist a person entry (project 7 S2): the local lens grouping
    /// contact entries under one label — `persons/<id>` holds the label
    /// line, then one member stem key (hex) per line. The id is the
    /// client's drawn `PersonId` as its raw `u128` — taking the number,
    /// not a string, keeps keys well-formed by construction. Ids are
    /// opaque local tokens, never derived from member keys or content
    /// (clusters merge, split, and rename). Never on the wire.
    pub async fn save_person(
        &self,
        id: u128,
        label: &str,
        members: &[PublicKey],
    ) -> Result<(), Error> {
        let mut content = String::from(label);
        for member in members {
            content.push('\n');
            content.push_str(&hex(&member.0));
        }
        self.write(&person_path(id), content.as_bytes()).await
    }

    pub async fn remove_person(&self, id: u128) {
        self.remove(&person_path(id)).await;
    }

    /// Persisted person entries as `(id, label, member stem keys)`. Damaged
    /// entries are skipped with a warning, like `contacts`; membership
    /// against the live contact store is the client's read-time concern.
    pub async fn persons(&self) -> Vec<(u128, String, Vec<PublicKey>)> {
        let mut persons = Vec::new();
        for name in self.names("persons").await {
            let Some(id) = parse_person_id(name.as_str()) else {
                if !name.as_str().starts_with('.') {
                    tracing::warn!(%name, "skipping person entry with a malformed id");
                }
                continue;
            };
            if let Some((label, members)) = self.read_person(&name).await {
                persons.push((id, label, members));
            }
        }
        persons.sort_by_key(|&(id, ..)| id);
        persons
    }

    /// Drain person entries from before drawn ids (the brief counter era
    /// of project 7 — `p1`-style names, plus the `.next` counter) so the
    /// client can re-mint them under real ids, labels and clustering
    /// intact. Empty on current stores.
    pub async fn take_legacy_persons(&self) -> Vec<(String, Vec<PublicKey>)> {
        self.remove("persons/.next").await;
        let mut legacy = Vec::new();
        for name in self.names("persons").await {
            if name.as_str().starts_with('.') || parse_person_id(name.as_str()).is_some() {
                continue;
            }
            if let Some(person) = self.read_person(&name).await {
                legacy.push(person);
            }
            self.remove(&format!("persons/{name}")).await;
        }
        legacy
    }

    /// All stored contacts as `(petname, record)`, petname-sorted.
    pub async fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, Error> {
        let mut contacts = Vec::new();
        for name in self.names("contacts").await {
            if extension(&name) != Some("record") {
                continue;
            }
            let Some(stem) = stem(&name) else { continue };
            let path = format!("contacts/{name}");
            let record = self
                .read(&path)
                .await
                .ok_or_else(|| "unreadable".to_string())
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).map_err(|e| e.to_string()));
            let petname = self.read_text(&format!("contacts/{stem}.name")).await;
            match (record, petname) {
                (Ok(record), Some(petname)) => contacts.push((petname.trim().to_string(), record)),
                (record, petname) => tracing::warn!(
                    path,
                    record_err = ?record.err(),
                    petname_missing = petname.is_none(),
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
    pub async fn save_learned(
        &self,
        subject: &PublicKey,
        responder: &PublicKey,
        record: &ContactRecord,
        endorsements: &[SignedAttestation],
        received_ms: u64,
    ) -> Result<(), Error> {
        let stem = format!("learned/{}/{}", hex(&subject.0), hex(&responder.0));
        self.write(&format!("{stem}.record"), &record.to_bytes())
            .await?;
        let endorsements_path = format!("{stem}.endorsements");
        if endorsements.is_empty() {
            self.remove(&endorsements_path).await;
        } else {
            self.write(&endorsements_path, &encode_attestations(endorsements))
                .await?;
        }
        self.write(&format!("{stem}.time"), received_ms.to_string().as_bytes())
            .await
    }

    /// Every learned record for a subject, unordered. Damaged entries are
    /// skipped with a warning — advisory data, never fatal.
    pub async fn learned(&self, subject: &PublicKey) -> Vec<LearnedRecord> {
        let dir = format!("learned/{}", hex(&subject.0));
        let mut learned = Vec::new();
        for name in self.names(&dir).await {
            if extension(&name) != Some("record") {
                continue;
            }
            let Some(stem) = stem(&name) else { continue };
            let responder = crate::hex::parse32(stem).ok().map(PublicKey);
            let record = self
                .read(&format!("{dir}/{name}"))
                .await
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).ok());
            let (Some(responder), Some(record)) = (responder, record) else {
                tracing::warn!(dir, %name, "skipping a damaged learned entry");
                continue;
            };
            let received_ms = self
                .read_number(&format!("{dir}/{stem}.time"))
                .await
                .unwrap_or(0);
            let endorsements = self
                .read(&format!("{dir}/{stem}.endorsements"))
                .await
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
    pub async fn dismiss_key(&self, key: &PublicKey) -> Result<(), Error> {
        let mut dismissed = self.dismissed_keys().await;
        if !dismissed.insert(*key) {
            return Ok(());
        }
        let content: String = dismissed
            .iter()
            .map(|key| format!("{}\n", hex(&key.0)))
            .collect();
        self.write("dismissed.keys", content.as_bytes()).await
    }

    pub async fn dismissed_keys(&self) -> BTreeSet<PublicKey> {
        self.read_hex_lines("dismissed.keys")
            .await
            .into_iter()
            .map(PublicKey)
            .collect()
    }

    // ===== the lens-sync store (project 7 S6, lens-sync.md) =====

    /// The channel this device emits lens ops into (lens-sync.md §3).
    pub async fn lens_conversation(&self) -> Option<MessageId> {
        self.read_text("lens/emit")
            .await
            .and_then(|content| crate::hex::parse32(content.trim()).ok())
            .map(MessageId)
    }

    pub async fn set_lens_conversation(&self, id: MessageId) -> Result<(), Error> {
        self.write("lens/emit", format!("{}\n", hex(&id.0)).as_bytes())
            .await
    }

    /// Every conversation classified as a lens channel (lens-sync.md §3)
    /// — what `conversations()` filters out of the inbox. One hex id per
    /// line; grows as replay classifies, never shrinks.
    pub async fn lens_channels(&self) -> BTreeSet<MessageId> {
        self.read_hex_lines("lens/channels")
            .await
            .into_iter()
            .map(MessageId)
            .collect()
    }

    pub async fn add_lens_channel(&self, id: MessageId) -> Result<(), Error> {
        let mut channels = self.lens_channels().await;
        if !channels.insert(id) {
            return Ok(());
        }
        self.write("lens/channels", &hex_lines(channels.iter().map(|id| &id.0)))
            .await
    }

    /// The applied-ops ledger (lens-sync.md §5): op message ids the replay
    /// may skip. Marked only *after* an op's effect lands, so a crash
    /// between the two re-applies idempotently rather than losing the op.
    pub async fn lens_applied(&self) -> BTreeSet<MessageId> {
        self.read_hex_lines("lens/applied")
            .await
            .into_iter()
            .map(MessageId)
            .collect()
    }

    pub async fn add_lens_applied(&self, id: MessageId) -> Result<(), Error> {
        let mut applied = self.lens_applied().await;
        if !applied.insert(id) {
            return Ok(());
        }
        self.write("lens/applied", &hex_lines(applied.iter().map(|id| &id.0)))
            .await
    }

    /// Store a contact-add offer (lens-sync.md §6), latest per subject —
    /// the sibling multi-entry pattern the learned store uses.
    pub async fn save_offer(
        &self,
        subject: &PublicKey,
        author: &PublicKey,
        petname: &str,
        record: &ContactRecord,
    ) -> Result<(), Error> {
        let stem = offer_stem(subject);
        self.write(&format!("{stem}.record"), &record.to_bytes())
            .await?;
        self.write(&format!("{stem}.author"), hex(&author.0).as_bytes())
            .await?;
        self.write(&format!("{stem}.petname"), petname.as_bytes())
            .await
    }

    /// Every stored offer: (subject, author, petname, record). Damaged
    /// entries are skipped with a warning — advisory data, never fatal.
    pub async fn offers(&self) -> Vec<(PublicKey, PublicKey, String, ContactRecord)> {
        let mut offers = Vec::new();
        for name in self.names("lens/offers").await {
            if extension(&name) != Some("record") {
                continue;
            }
            let Some(stem) = stem(&name) else { continue };
            let subject = crate::hex::parse32(stem).ok().map(PublicKey);
            let record = self
                .read(&format!("lens/offers/{name}"))
                .await
                .and_then(|bytes| ContactRecord::try_from_bytes(&bytes).ok());
            let author = self
                .read_text(&format!("lens/offers/{stem}.author"))
                .await
                .and_then(|content| crate::hex::parse32(content.trim()).ok())
                .map(PublicKey);
            let petname = self.read_text(&format!("lens/offers/{stem}.petname")).await;
            let (Some(subject), Some(author), Some(petname), Some(record)) =
                (subject, author, petname, record)
            else {
                tracing::warn!(%name, "skipping a damaged offer entry");
                continue;
            };
            offers.push((subject, author, petname, record));
        }
        offers
    }

    pub async fn remove_offer(&self, subject: &PublicKey) {
        let stem = offer_stem(subject);
        for ext in ["record", "author", "petname"] {
            self.remove(&format!("{stem}.{ext}")).await;
        }
    }

    /// Void every offer a device authored (lens-sync.md §6): the
    /// repudiation hook.
    pub async fn remove_offers_by(&self, author: &PublicKey) {
        for (subject, offer_author, _, _) in self.offers().await {
            if offer_author == *author {
                self.remove_offer(&subject).await;
            }
        }
    }

    /// Surface a label conflict (lens-sync.md §5), latest per person.
    pub async fn save_conflict(
        &self,
        person: u128,
        theirs: &str,
        author: &PublicKey,
    ) -> Result<(), Error> {
        let stem = conflict_stem(person);
        self.write(&format!("{stem}.label"), theirs.as_bytes())
            .await?;
        self.write(&format!("{stem}.author"), hex(&author.0).as_bytes())
            .await
    }

    /// Every surfaced conflict: (person storage id, their label, author).
    pub async fn conflicts(&self) -> Vec<(u128, String, PublicKey)> {
        let mut conflicts = Vec::new();
        for name in self.names("lens/conflicts").await {
            if extension(&name) != Some("label") {
                continue;
            }
            let Some(stem) = stem(&name) else { continue };
            let person = u128::from_str_radix(stem, 16).ok();
            let theirs = self.read_text(&format!("lens/conflicts/{name}")).await;
            let author = self
                .read_text(&format!("lens/conflicts/{stem}.author"))
                .await
                .and_then(|content| crate::hex::parse32(content.trim()).ok())
                .map(PublicKey);
            let (Some(person), Some(theirs), Some(author)) = (person, theirs, author) else {
                tracing::warn!(%name, "skipping a damaged conflict entry");
                continue;
            };
            conflicts.push((person, theirs, author));
        }
        conflicts
    }

    pub async fn remove_conflict(&self, person: u128) {
        let stem = conflict_stem(person);
        for ext in ["label", "author"] {
            self.remove(&format!("{stem}.{ext}")).await;
        }
    }

    /// Peers whose last direct dial got nowhere, and when (De6b):
    /// `<hex key> <wall-clock ms>` per line, replacing the entry wholesale.
    ///
    /// Dumb storage on purpose — *which* entries are worth keeping is the
    /// caller's policy (the dial cooldown lives in `client`), so this writes
    /// exactly what it is given and reads back exactly what is there.
    /// Wall-clock, like every other persisted timestamp (the B5 lesson:
    /// `Instant` doesn't serialize).
    pub async fn save_unreachable(&self, entries: &[([u8; 32], u64)]) -> Result<(), Error> {
        let content: String = entries
            .iter()
            .map(|(key, at_ms)| format!("{} {at_ms}\n", hex(key)))
            .collect();
        self.write("unreachable.keys", content.as_bytes()).await
    }

    /// Reads back `save_unreachable`. Unparseable lines are skipped rather
    /// than fatal: this is a cache of negative evidence, and the worst a lost
    /// entry costs is one extra dial.
    pub async fn unreachable(&self) -> Vec<([u8; 32], u64)> {
        self.read_text("unreachable.keys")
            .await
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (key, at_ms) = line.trim().split_once(' ')?;
                Some((crate::hex::parse32(key).ok()?, at_ms.parse().ok()?))
            })
            .collect()
    }

    /// Re-point person memberships from one member stem to another — the
    /// record-update companion (`replace_contact`): a re-keyed entry must
    /// stay exactly as clustered as it was (S2's no-dangle rule).
    async fn move_person_member(&self, old: &PublicKey, new: &PublicKey) {
        for (id, label, mut members) in self.persons().await {
            if let Some(slot) = members.iter_mut().find(|member| *member == old) {
                *slot = *new;
                if let Err(error) = self.save_person(id, &label, &members).await {
                    tracing::warn!(%error, id, "could not re-point a person membership");
                }
            }
        }
    }

    /// One person entry's content: the label line, then member stem keys.
    async fn read_person(&self, name: &Segment) -> Option<(String, Vec<PublicKey>)> {
        let Some(content) = self.read_text(&format!("persons/{name}")).await else {
            tracing::warn!(%name, "skipping unreadable person entry");
            return None;
        };
        let mut lines = content.lines();
        let Some(label) = lines
            .next()
            .map(str::trim)
            .filter(|label| !label.is_empty())
        else {
            tracing::warn!(%name, "skipping person entry with no label");
            return None;
        };
        let members: Vec<PublicKey> = lines
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| crate::hex::parse32(line.trim()).ok().map(PublicKey))
            .collect();
        Some((label.to_string(), members))
    }

    async fn read_attestation(&self, path: &str) -> Option<SignedAttestation> {
        self.read(path)
            .await
            .and_then(|bytes| SignedAttestation::try_from_bytes(&bytes).ok())
    }

    /// A one-number entry (`first-seen`, `read-count`, `.time`, …).
    async fn read_number<N: std::str::FromStr>(&self, path: &str) -> Option<N> {
        self.read_text(path).await?.trim().parse().ok()
    }

    /// A one-hex-value-per-line set entry; unparseable lines are skipped.
    async fn read_hex_lines(&self, path: &str) -> Vec<[u8; 32]> {
        self.read_text(path)
            .await
            .unwrap_or_default()
            .lines()
            .filter_map(|line| crate::hex::parse32(line.trim()).ok())
            .collect()
    }

    // ===== the store, in this layer's terms =====
    //
    // Reads collapse a failure to absence: every read here is advisory or
    // re-derivable, and a damaged or unreadable entry is skipped exactly as
    // a missing one is. Writes surface failures; removes are best-effort.

    async fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.store.get(&key(path)).await.ok().flatten()
    }

    async fn read_text(&self, path: &str) -> Option<String> {
        String::from_utf8(self.read(path).await?).ok()
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Error> {
        self.store.put(&key(path), bytes).await.map_err(storage)
    }

    async fn remove(&self, path: &str) {
        if let Err(error) = self.store.delete(&key(path)).await {
            tracing::debug!(%error, "could not remove a state entry");
        }
    }

    /// A directory's children; a failed listing reads as empty.
    async fn names(&self, dir: &str) -> Vec<Segment> {
        self.store.list(&key(dir)).await.unwrap_or_default()
    }
}

/// `load_dag` over envelopes the caller already holds. Same rebuild,
/// without reading and BORSH-decoding the whole conversation a second
/// time: `history` and `conversations` load the envelopes anyway, so
/// going back to the store for the DAG doubled the cost of every render —
/// and both run on each `new-messages` event. Clones the cores, which
/// is the allocation a re-read would have made regardless, minus the
/// I/O and the decode.
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

/// One outstanding delivery: `message` (of `conversation`) still owes
/// `relay` a deposit and any blob pushes.
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub message: MessageId,
    pub relay: String,
    pub conversation: MessageId,
    pub created_ms: u64,
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

// ===== layout: which key holds what =====

/// Every key this layer builds is a literal or hex — well-formed by
/// construction, so a parse failure is a bug in this file, not input.
fn key(path: &str) -> Key {
    Key::parse(path).expect("state keys are built from literals and hex")
}

fn conversation_dir(conversation: MessageId) -> String {
    format!("conversations/{}", hex(&conversation.0))
}

/// Inside the conversation dir, with no `.env` suffix, so
/// `load_envelopes` ignores it like the `.acks` sidecars.
fn first_seen_path(conversation: MessageId) -> String {
    format!("{}/first-seen", conversation_dir(conversation))
}

/// A sidecar beside `first-seen`, ignored by `load_envelopes` alike.
fn conversation_name_path(conversation: MessageId) -> String {
    format!("{}/my-name", conversation_dir(conversation))
}

fn read_count_path(conversation: MessageId) -> String {
    format!("{}/read-count", conversation_dir(conversation))
}

/// Beside the envelope, not under `outbox/`: scoped to the conversation
/// so it dies with it, and `load_envelopes` already ignores anything
/// that isn't `.env`, so the loader needs no change.
fn acks_path(conversation: MessageId, message: MessageId) -> String {
    format!(
        "{}/{}.acks",
        conversation_dir(conversation),
        hex(&message.0)
    )
}

fn participants_path(participants: &BTreeSet<PublicKey>) -> String {
    // Fingerprint = BLAKE3 over the sorted keys (BTreeSet iterates
    // sorted), so any member computes the same name.
    let mut hasher = blake3::Hasher::new();
    for key in participants {
        hasher.update(&key.0);
    }
    format!("participants/{}", hasher.finalize().to_hex())
}

fn blob_path(hash: &BlobHash) -> String {
    format!("blobs/{}", hex(&hash.0))
}

/// One entry per (message, relay): the relay part of the name is a
/// fingerprint (dial strings hold `@`/`:`), the full string lives in
/// the entry.
fn outbox_path(message: MessageId, relay: &str) -> String {
    let fingerprint = blake3::hash(relay.as_bytes()).to_hex();
    format!("outbox/{}.{}", hex(&message.0), &fingerprint.as_str()[..16])
}

fn contact_stem(key: &PublicKey) -> String {
    format!("contacts/{}", hex(&key.0))
}

fn relay_override_path(key: &PublicKey) -> String {
    format!("{}.relays", contact_stem(key))
}

fn local_avatar_path(key: &PublicKey) -> String {
    format!("local-avatars/{}", hex(&key.0))
}

fn device_stem(key: &PublicKey) -> String {
    format!("devices/{}", hex(&key.0))
}

fn vouch_path(subject: &PublicKey, kind: &str) -> String {
    format!("vouches/{}.{kind}", hex(&subject.0))
}

fn person_path(id: u128) -> String {
    format!("persons/{id:032x}")
}

fn offer_stem(subject: &PublicKey) -> String {
    format!("lens/offers/{}", hex(&subject.0))
}

fn conflict_stem(person: u128) -> String {
    format!("lens/conflicts/{person:032x}")
}

// ===== encoding =====

/// The entry name carries the message id; the body is three lines:
/// relay dial string, conversation hex, created-ms.
fn parse_outbox_entry(name: &str, content: &str) -> Option<OutboxEntry> {
    let message = MessageId(crate::hex::parse32(name.split('.').next()?).ok()?);
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

fn parse_relay_specs(content: &str) -> Vec<RelayEntry> {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(RelayEntry::from_spec)
        .collect()
}

fn hex_lines<'a>(values: impl Iterator<Item = &'a [u8; 32]>) -> Vec<u8> {
    values
        .map(|value| format!("{}\n", hex(value)))
        .collect::<String>()
        .into_bytes()
}

/// Local framing for attestation lists (u32-LE length + wire bytes per
/// entry) — a storage convention, never on the wire; damaged tails are
/// skipped like every advisory store.
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
/// one bad entry can't hide a whole conversation.
fn build_dag(
    mut cores: Vec<MessageCore>,
    conversation: MessageId,
) -> Result<ConversationDag, Error> {
    let genesis_at = cores
        .iter()
        .position(|core| core.conversation.is_none())
        .ok_or_else(|| {
            Error::Conversation(format!(
                "conversation {} has no genesis stored",
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
fn parse_keys(bytes: Vec<u8>) -> BTreeSet<PublicKey> {
    bytes
        .chunks_exact(32)
        .map(|chunk| PublicKey(chunk.try_into().expect("32 bytes")))
        .collect()
}

/// A person entry name is exactly the raw id as 32 hex chars (`{:032x}`).
fn parse_person_id(name: &str) -> Option<u128> {
    (name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(name, 16).ok())
        .flatten()
}

/// An entry name's extension and stem, with `Path`'s semantics (so a
/// leading-dot name has no extension) — the store's names are plain file
/// names, separators excluded by construction.
fn extension(name: &Segment) -> Option<&str> {
    Path::new(name.as_str()).extension()?.to_str()
}

fn stem(name: &Segment) -> Option<&str> {
    Path::new(name.as_str()).file_stem()?.to_str()
}

fn storage(error: StorageError) -> Error {
    Error::Storage(error.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use zink_protocol::{Attestation, DeviceKey, KeyCommitment, Versioned};

    #[tokio::test]
    async fn fs_layout__should_stay_byte_identical_to_the_pre_port_store() {
        // Given
        let root = std::env::temp_dir().join(format!("zink-state-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = ClientState::new(FsStore::new(&root));

        // When
        write_every_kind(&state).await;

        // Then
        let files: Vec<(String, String)> = tree(&root)
            .into_iter()
            .map(|(path, bytes)| (path, blake3::hash(&bytes).to_hex().to_string()))
            .collect();
        let golden: Vec<(String, String)> = PRE_PORT_LAYOUT
            .iter()
            .map(|&(path, hash)| (path.to_string(), hash.to_string()))
            .collect();
        assert_eq!(files, golden);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `write_every_kind`'s files as the pre-port `ClientState` (std::fs
    /// direct, before project 9) wrote them: path and BLAKE3 of the bytes
    /// (`first-seen` hashed empty — it holds the real clock). Generated by
    /// running this scenario against that code. A mismatch means existing
    /// stores no longer read back — a data migration, never a test update.
    const PRE_PORT_LAYOUT: &[(&str, &str)] = &[
        (
            "blobs/0707070707070707070707070707070707070707070707070707070707070707",
            "c12f1481789d50a4c549e15c42bda1759277bc954d2f4b62c0f4531937f2e990",
        ),
        (
            "contacts/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.name",
            "e476f1b379438de7a1acfd567a94a8c53f08b9714042f7f17e5791645afc3176",
        ),
        (
            "contacts/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.record",
            "11dc9bd30b7c01cdd8abe4252844d6c58c46fb5753af0efe6b9ec7257247483e",
        ),
        (
            "contacts/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.relays",
            "21dacec23e6a57da30e85b7f28966ad5cc1aaf9b2928c7d1146a1a547cef0f0d",
        ),
        (
            "conversations/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39.acks",
            "1eed29b1654fbca94617004d7969dfc4652b1f30a7a8b771c34800155483380b",
        ),
        (
            "conversations/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39.env",
            "1416fa7e0dc41f681f8b1e73739328b96726a1f0fc561fc8df3465865c7da209",
        ),
        (
            "conversations/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39/first-seen",
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        ),
        (
            "conversations/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39/my-name",
            "0fbbcf0777f7bbeb24059f2d703ff784bbb469d4bc19a14d63ae8ce6dcc600a3",
        ),
        (
            "conversations/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39/read-count",
            "58d6fd3dc609068615d66b6a2616cce521e6bd49bbcd7854e3f2573b142c6637",
        ),
        (
            "devices/ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1.record",
            "8ef75146a4dd7fb4eae606806c77999ba1a134ae5ef5d31824f85510176d7c2a",
        ),
        (
            "devices/ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1.vouch",
            "099634e30c325a9d73c4ed63ea2e72fb0161b6e0e801f3ae38b3351e63528fd3",
        ),
        (
            "dismissed.keys",
            "281f569fe0ff35c4a15947dd66013122fb5eb26ae99bcc71bff36eec83755fc8",
        ),
        (
            "learned/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394/ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1.endorsements",
            "4a71df23be283ffe1037f1622b7cad5ce7fa4e3691184511956745ed42fe9b67",
        ),
        (
            "learned/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394/ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1.record",
            "11dc9bd30b7c01cdd8abe4252844d6c58c46fb5753af0efe6b9ec7257247483e",
        ),
        (
            "learned/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394/ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1.time",
            "76cfd91501a941b339652a5de556f085a523357d9114839dd85391b75516a425",
        ),
        (
            "lens/applied",
            "a11f356cec1af04998de5b1ed3d903c2e01816ae2fba9afa4ad59846aa53d5bf",
        ),
        (
            "lens/channels",
            "a11f356cec1af04998de5b1ed3d903c2e01816ae2fba9afa4ad59846aa53d5bf",
        ),
        (
            "lens/conflicts/00000000000000000000000000000005.author",
            "27cd8be90f976c5533f9ccf25da1b2be5398e2d5f5f8915a0686e1d0b541cf4e",
        ),
        (
            "lens/conflicts/00000000000000000000000000000005.label",
            "531e66bba487c03dc742ef7ad92500029682c7656ba71600b9983e4bb834868a",
        ),
        (
            "lens/emit",
            "a11f356cec1af04998de5b1ed3d903c2e01816ae2fba9afa4ad59846aa53d5bf",
        ),
        (
            "lens/offers/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.author",
            "27cd8be90f976c5533f9ccf25da1b2be5398e2d5f5f8915a0686e1d0b541cf4e",
        ),
        (
            "lens/offers/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.petname",
            "8776e9aaf37d64ad03ebc3eb7b02044086fee99e2861604f531a10afdbda751b",
        ),
        (
            "lens/offers/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.record",
            "11dc9bd30b7c01cdd8abe4252844d6c58c46fb5753af0efe6b9ec7257247483e",
        ),
        (
            "local-avatars/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394",
            "33f7222e8037668177e62fd7f1c88ad6633873e974874ac653e1524dd003294d",
        ),
        (
            "outbox/2493d00ca03da23a64da3413108d929efb5f5d10d211a7532ddea4c3f1310d39.5acfd656fb27120b",
            "41b367ed623340e65ff211bec7811d2ffad9e5368cbb4e9b570d78159718508a",
        ),
        (
            "participants/7a73a83b254f322bcc8acd485236b48e791532ce7e8344d189e5924b2408629a",
            "a59df4d02ad85baa433debc5b1c5943eb3340341e1a469651757e749f0f35b8c",
        ),
        (
            "persons/00000000000000000000000000000005",
            "b7c3edb4cf29b54ced4dd4bad5c9db23ed1940a9bb076b31bf913b0bff82b2d7",
        ),
        (
            "profile.avatar",
            "f135645fc2d7480c433517036ef2f6aacbab07a3abdae1e6b94795bf74c70f4a",
        ),
        (
            "profile.device",
            "c91c366fbda314d16d6ec9430c62561ff2824c0d8e341ba4b714ce663d967d22",
        ),
        (
            "profile.name",
            "38e8b28b68304e7a92394ec31664dffd27649f1c21ca2ae106db43d3fcb3f3a6",
        ),
        (
            "profile.relays",
            "21dacec23e6a57da30e85b7f28966ad5cc1aaf9b2928c7d1146a1a547cef0f0d",
        ),
        (
            "profile.revision",
            "813e9b729141e7f385afa0a2d0df3e6c3789e427ffe4aeef566a565bc8f2fe3d",
        ),
        (
            "unreachable.keys",
            "03d73e006aa225fb9984b84f5e49c2dfb6df5ccc5e178cf13f9f86307ce99465",
        ),
        (
            "vouches/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.attestation",
            "7eb6de14f3abcd8d6eeb6658e7df0bab35123a706094f9abe41e162cfddfffa4",
        ),
        (
            "vouches/8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394.avatar",
            "e60616d43cfa9cf3d8eb3f62d72e3aca83ae452602668e2fd0c12ad39bc63543",
        ),
    ];

    /// Every writer `ClientState` has, once, with fixed inputs — the
    /// layout a pre-port store already holds on disk.
    async fn write_every_kind(state: &ClientState) {
        let (me, bob, sib) = (device(1), device(2), device(3));
        let genesis = genesis(&me, bob.public());
        let conversation = genesis.id();
        let relays = vec![RelayEntry {
            mailbox: "aa@203.0.113.1:1".to_string(),
            relay_url: Some("http://203.0.113.1:1".to_string()),
        }];
        let bob_record = ContactRecord::new(
            vec![bob.public()],
            vec![name_claim(&bob, &bob, "bob")],
            relays.clone(),
        );
        let sib_record = ContactRecord::new(vec![sib.public()], vec![], relays.clone());
        let participants = BTreeSet::from([me.public(), bob.public()]);

        state
            .record_conversation(&participants, conversation)
            .await
            .unwrap();
        state
            .store_envelope(conversation, &genesis, 0)
            .await
            .unwrap();
        state
            .set_conversation_name(conversation, Some("trip"))
            .await
            .unwrap();
        state.set_read_count(conversation, 3).await.unwrap();
        state
            .add_acks(conversation, conversation, &BTreeSet::from([bob.public()]))
            .await
            .unwrap();
        state
            .save_blob(&BlobHash([7; 32]), b"ciphertext")
            .await
            .unwrap();
        state
            .add_outbox(conversation, "aa@203.0.113.1:1", conversation, 42)
            .await
            .unwrap();
        state.save_profile("me", &relays).await.unwrap();
        state.save_profile_revision(2).await.unwrap();
        state
            .save_avatar_meta(&BlobHash([8; 32]), &[9; 32], 1)
            .await
            .unwrap();
        state.save_device_label("laptop", 1).await.unwrap();
        state.save_contact("bob", &bob_record).await.unwrap();
        state
            .save_local_avatar(&bob.public(), b"jpeg")
            .await
            .unwrap();
        state
            .save_recognized_device(&sib_record, &name_claim(&me, &sib, "sib"))
            .await
            .unwrap();
        state
            .save_vouch(&bob.public(), &name_claim(&me, &bob, "bobby"))
            .await
            .unwrap();
        state
            .save_avatar_share(&bob.public(), &name_claim(&me, &bob, "pic"))
            .await
            .unwrap();
        state
            .save_relay_override(&bob.public(), &relays)
            .await
            .unwrap();
        state.save_person(5, "Bob", &[bob.public()]).await.unwrap();
        state
            .save_learned(
                &bob.public(),
                &sib.public(),
                &bob_record,
                &[name_claim(&sib, &bob, "b")],
                77,
            )
            .await
            .unwrap();
        state.dismiss_key(&sib.public()).await.unwrap();
        state.set_lens_conversation(conversation).await.unwrap();
        state.add_lens_channel(conversation).await.unwrap();
        state.add_lens_applied(conversation).await.unwrap();
        state
            .save_offer(&bob.public(), &sib.public(), "bobby", &bob_record)
            .await
            .unwrap();
        state
            .save_conflict(5, "Robert", &sib.public())
            .await
            .unwrap();
        state.save_unreachable(&[([4; 32], 99)]).await.unwrap();
    }

    fn device(seed: u8) -> DeviceKey {
        DeviceKey::from_seed([seed; 32])
    }

    fn genesis(author: &DeviceKey, recipient: PublicKey) -> MessageEnvelope {
        let core = MessageCore {
            version: MessageCore::CURRENT,
            conversation: None,
            parents: vec![],
            recipients: vec![recipient],
            sender: author.public(),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: vec![],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        };
        MessageEnvelope::new(core, author)
    }

    fn name_claim(attester: &DeviceKey, subject: &DeviceKey, name: &str) -> SignedAttestation {
        let attestation = Attestation {
            version: Attestation::CURRENT,
            attester: attester.public(),
            subject: subject.public(),
            claim: Claim::Name(name.to_string()),
            revision: 1,
        };
        SignedAttestation::new(attestation, attester)
    }

    /// Every file under `root`, by `/`-joined relative path. The
    /// `first-seen` marker holds the real clock's reading, so only its
    /// presence is comparable.
    fn tree(root: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let bytes = if name.ends_with("/first-seen") {
                    Vec::new()
                } else {
                    std::fs::read(&path).unwrap()
                };
                files.insert(name, bytes);
            }
        }
        files
    }
}
