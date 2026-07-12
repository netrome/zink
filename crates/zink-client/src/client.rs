//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::Endpoint;
use rand_core::{OsRng, RngCore};
use zink_protocol::{
    Attestation, BlobDraft, BlobHash, BlobRef, Claim, ContactRecord, DeviceKey, FORMAT_VERSION,
    MailboxOp, MailboxResult, MessageDraft, MessageEnvelope, MessageId, OpenError, PublicKey,
    SignedAttestation, distinct_relays,
};

use crate::state::ClientState;
use crate::{blobs, hex, keystore, net};

/// Outbox entries older than this stop being retried (but stay surfaced):
/// mirrors the relay's default mailbox retention — past it, recipients'
/// cursors have moved on and the message is socially dead.
const OUTBOX_GIVE_UP_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Tuning the edges inject at construction; `Default` fits production.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Deadline for reaching a relay. Long enough for a phone on flaky
    /// cellular; tests exercising down-relay paths shrink it.
    pub connect_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
        }
    }
}

pub struct Client {
    device: DeviceKey,
    endpoint: Endpoint,
    state: ClientState,
    config: ClientConfig,
}

impl Client {
    /// Open with an existing key (the CLI path — `keygen` created it).
    pub async fn open(key_path: &str) -> Result<Self, String> {
        Self::open_with(key_path, ClientConfig::default()).await
    }

    /// `open` with edge-injected tuning.
    pub async fn open_with(key_path: &str, config: ClientConfig) -> Result<Self, String> {
        Self::with_device(keystore::load(key_path)?, key_path, config).await
    }

    /// Open, creating the key on first run (the app path).
    pub async fn open_or_create(key_path: &str) -> Result<Self, String> {
        Self::with_device(
            keystore::load_or_create(key_path)?,
            key_path,
            ClientConfig::default(),
        )
        .await
    }

    async fn with_device(
        device: DeviceKey,
        key_path: &str,
        config: ClientConfig,
    ) -> Result<Self, String> {
        let endpoint = net::bind_endpoint(&device).await?;
        Ok(Self {
            device,
            endpoint,
            state: ClientState::open(key_path),
            config,
        })
    }

    pub fn public_key(&self) -> PublicKey {
        self.device.public()
    }

    /// Seal for all recipients, thread into the participant set's
    /// conversation (or start one), deposit once per distinct relay
    /// (idempotent retry), push blobs to each relay's cache.
    pub async fn send(
        &self,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<SendReceipt, String> {
        if contacts.is_empty() {
            return Err("at least one recipient required".into());
        }
        let recipients: Vec<PublicKey> = contacts.iter().flat_map(|c| c.keys.clone()).collect();
        let participants: BTreeSet<PublicKey> = recipients
            .iter()
            .copied()
            .chain([self.device.public()])
            .collect();
        let existing = self.state.conversation_for(&participants);
        let draft = match existing {
            Some(conversation) => self.threaded_draft(conversation, recipients)?,
            None => MessageDraft {
                conversation: None,
                parents: vec![],
                recipients,
                seq: 0,
                logical: 0,
                timestamp_ms: now_ms(),
                plaintext: vec![],
                blobs: vec![],
            },
        };
        let record_mapping = existing.is_none().then_some(&participants);
        self.finish_send(draft, plaintext, blob_drafts, contacts, record_mapping)
            .await
    }

    /// Send *into a known conversation*, whatever its participant set maps
    /// to — how an edge replies from a history view. Leaves the participant
    /// → conversation index alone (that index is `send`'s policy).
    pub async fn send_in(
        &self,
        conversation: MessageId,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<SendReceipt, String> {
        if contacts.is_empty() {
            return Err("at least one recipient required".into());
        }
        let recipients: Vec<PublicKey> = contacts.iter().flat_map(|c| c.keys.clone()).collect();
        let draft = self.threaded_draft(conversation, recipients)?;
        self.finish_send(draft, plaintext, blob_drafts, contacts, None)
            .await
    }

    /// Whom a reply in this conversation goes to: every participant except
    /// this device, resolved through the contact store. Participants with
    /// no stored record are returned as `unknown` — there is no relay to
    /// reach them at, so a reply is best-effort by design (the edge decides
    /// how loudly to say so).
    pub fn reply_contacts(&self, conversation: MessageId) -> Result<ReplyContacts, String> {
        let me = self.device.public();
        let participants: BTreeSet<PublicKey> = self
            .state
            .load_envelopes(conversation)?
            .iter()
            .flat_map(|envelope| {
                envelope
                    .core
                    .recipients
                    .iter()
                    .copied()
                    .chain([envelope.core.sender])
            })
            .filter(|key| *key != me)
            .collect();
        let records = self.state.contacts()?;
        let mut contacts = Vec::new();
        let mut unknown = Vec::new();
        for key in participants {
            match records
                .iter()
                .find(|(_, record)| record.keys.contains(&key))
            {
                Some((_, record)) => contacts.push(Contact {
                    keys: vec![key],
                    relays: record.relays.clone(),
                }),
                None => unknown.push(key),
            }
        }
        Ok(ReplyContacts { contacts, unknown })
    }

    /// A draft threaded onto the stored DAG's heads (body filled by
    /// `finish_send`).
    fn threaded_draft(
        &self,
        conversation: MessageId,
        recipients: Vec<PublicKey>,
    ) -> Result<MessageDraft, String> {
        let dag = self.state.load_dag(conversation)?;
        Ok(MessageDraft {
            conversation: Some(conversation),
            parents: dag.heads(),
            recipients,
            seq: dag.next_seq(&self.device.public()),
            logical: dag.next_logical(),
            timestamp_ms: now_ms(),
            plaintext: vec![],
            blobs: vec![],
        })
    }

    /// The shared send tail: flush older queued deliveries, seal, persist
    /// (envelope + own-blob cache + outbox ledger + optionally the
    /// participant mapping), then deliver per distinct relay. One relay
    /// failing never aborts the others; what failed stays in the outbox.
    /// Errors only when *no* relay took the deposit — the message is still
    /// stored and queued, so the error means "queued", not "lost".
    async fn finish_send(
        &self,
        mut draft: MessageDraft,
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
        contacts: &[Contact],
        record_mapping: Option<&BTreeSet<PublicKey>>,
    ) -> Result<SendReceipt, String> {
        // NOTE: the outbox is NOT flushed here. Flushing on the send path
        // coupled a new message's latency to the health of the *backlog* —
        // a slow/stuck queued delivery delayed every fresh send. The backlog
        // is retried off this path (recv, subscription reconnect, and the
        // edge's post-send background flush), so a fresh send pays only for
        // its own delivery.
        draft.plaintext = plaintext;
        draft.blobs = blob_drafts;
        let seq = draft.seq;
        let existing = draft.conversation;
        let sealed = MessageEnvelope::seal(draft, &self.device, &mut OsRng)
            .map_err(|e| format!("seal message: {e}"))?;
        let id = sealed.envelope.id();
        let conversation = existing.unwrap_or(id);
        self.state.store_envelope(conversation, &sealed.envelope)?;
        if let Some(participants) = record_mapping {
            self.state.record_conversation(participants, conversation)?;
        }
        // Own blobs go straight into the local cache: they get pushed to the
        // *recipients'* relays, so this is the only place we can refetch
        // them from when rendering our own history.
        for blob in &sealed.blobs {
            self.state.save_blob(&blob.hash, &blob.bytes)?;
        }

        // Ledger before network (live-delivery.md §2): a crash or failure
        // from here on leaves entries a later flush retries idempotently.
        let relays = distinct_relays(contacts.iter().map(|c| c.relays.clone()));
        let now = now_ms();
        for relay in &relays {
            self.state.add_outbox(id, relay, conversation, now)?;
        }

        let staging = blobs::stage(&sealed.blobs).await?;
        let mut pending_relays = 0;
        let mut last_error = String::new();
        for relay in &relays {
            match self
                .deliver_to_relay(relay, &sealed.envelope, &sealed.blobs, &staging)
                .await
            {
                Ok(()) => self.state.clear_outbox(id, relay),
                Err(error) => {
                    eprintln!("delivery to {relay} failed ({error}); queued for retry");
                    pending_relays += 1;
                    last_error = error;
                }
            }
        }
        if pending_relays == relays.len() && !relays.is_empty() {
            return Err(format!(
                "no relay took the deposit — message queued for retry ({last_error})"
            ));
        }
        Ok(SendReceipt {
            id,
            conversation,
            seq,
            blob_count: sealed.blobs.len(),
            relay_count: relays.len(),
            pending_relays,
        })
    }

    /// One relay's full delivery: deposit (idempotent retry inside), then
    /// every blob push. Only a fully-served relay counts as delivered.
    async fn deliver_to_relay(
        &self,
        relay: &str,
        envelope: &MessageEnvelope,
        encrypted_blobs: &[zink_protocol::EncryptedBlob],
        staging: &iroh_blobs::store::mem::MemStore,
    ) -> Result<(), String> {
        net::deposit_with_retry(&self.endpoint, relay, envelope, self.config.connect_timeout)
            .await?;
        if !encrypted_blobs.is_empty() {
            blobs::push_blobs(
                &self.endpoint,
                relay,
                staging,
                encrypted_blobs,
                self.config.connect_timeout,
            )
            .await?;
        }
        Ok(())
    }

    /// Retry every outstanding delivery (idempotent: deposits dedup by id,
    /// blob pushes by hash). Entries older than the give-up window are left
    /// in place unretried — the relay's retention has expired, the message
    /// stays surfaced as pending/undelivered (deleting it is not our call).
    pub async fn flush_outbox(&self) -> Result<FlushReport, String> {
        let mut report = FlushReport::default();
        let now = now_ms();
        for entry in self.state.outbox() {
            if now.saturating_sub(entry.created_ms) > OUTBOX_GIVE_UP_MS {
                report.expired += 1;
                continue;
            }
            let envelope = match self.state.load_envelope(entry.conversation, entry.message) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // No stored envelope — nothing a retry could ever send.
                    eprintln!("warning: dropping unfulfillable outbox entry: {error}");
                    self.state.clear_outbox(entry.message, &entry.relay);
                    continue;
                }
            };
            // Re-stage owed blobs from the local cache (put there at send).
            let encrypted: Vec<zink_protocol::EncryptedBlob> = envelope
                .core
                .blob_refs
                .iter()
                .filter_map(|blob_ref| {
                    let bytes = self.state.load_blob(&blob_ref.hash);
                    if bytes.is_none() {
                        eprintln!(
                            "warning: blob {} missing from cache; delivering without it",
                            hex::encode(&blob_ref.hash.0)
                        );
                    }
                    Some(zink_protocol::EncryptedBlob {
                        hash: blob_ref.hash,
                        bytes: bytes?,
                    })
                })
                .collect();
            let staging = blobs::stage(&encrypted).await?;
            match self
                .deliver_to_relay(&entry.relay, &envelope, &encrypted, &staging)
                .await
            {
                Ok(()) => {
                    self.state.clear_outbox(entry.message, &entry.relay);
                    report.delivered += 1;
                }
                Err(error) => {
                    eprintln!("outbox retry to {} failed: {error}", entry.relay);
                    report.pending += 1;
                }
            }
        }
        Ok(report)
    }

    /// Drain every relay: register, then fetch page-by-page, dedup by
    /// message id, open, and remember what verified; ack each page at its
    /// own cursor.
    pub async fn recv(&self, relays: &[String]) -> Result<Vec<Received>, String> {
        let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut received = Vec::new();
        for relay in relays {
            let connection = net::connect(
                &self.endpoint,
                relay,
                zink_protocol::MAILBOX_ALPN,
                self.config.connect_timeout,
            )
            .await?;
            net::request(&connection, MailboxOp::Register).await?;
            received.extend(self.drain_connection(relay, &connection, &mut seen).await?);
        }
        // Post-drain flush (live-delivery.md §2): we're evidently online,
        // so retry anything still owed. Best-effort — a recv must not fail
        // because a *different* relay is down.
        let _ = self.flush_outbox().await;
        Ok(received)
    }

    /// Live delivery (live-delivery.md §4): one relay's subscription loop —
    /// connect, register (a registered live connection is what the relay
    /// nudges), flush the outbox, drain, then drain again on every nudge.
    /// Reconnects forever with jittered exponential backoff; ends only when
    /// the edge drops the future. `on_new` fires per non-empty drain.
    ///
    /// One loop per relay: with several home relays, a message may arrive
    /// through more than one, so `on_new` can repeat a message another
    /// loop already delivered — storage dedups by id; edges that alert
    /// should dedup by `envelope.id()`.
    pub async fn subscribe(&self, relay: &str, mut on_new: impl FnMut(Vec<Received>)) {
        let initial = Duration::from_secs(1);
        let mut backoff = initial;
        loop {
            match self.subscribe_once(relay, &mut on_new, &mut backoff).await {
                Ok(()) => {}
                Err(error) => eprintln!("subscription to {relay} dropped: {error}"),
            }
            // ±50% jitter so a relay restart doesn't get a thundering herd.
            let jitter = 0.5 + f64::from(OsRng.next_u32()) / f64::from(u32::MAX);
            n0_future::time::sleep(backoff.mul_f64(jitter)).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// One subscription session: lives until the connection dies. Resets
    /// `backoff` once registered (the relay is demonstrably healthy).
    async fn subscribe_once(
        &self,
        relay: &str,
        on_new: &mut impl FnMut(Vec<Received>),
        backoff: &mut Duration,
    ) -> Result<(), String> {
        let connection = net::connect(
            &self.endpoint,
            relay,
            zink_protocol::MAILBOX_ALPN,
            self.config.connect_timeout,
        )
        .await?;
        net::request(&connection, MailboxOp::Register).await?;
        // Reconnect = the network is back: flush queued sends (§2), then
        // catch up on whatever arrived while we were away.
        let _ = self.flush_outbox().await;
        let received = self
            .drain_connection(relay, &connection, &mut BTreeSet::new())
            .await?;
        // Reset backoff only now — a full register+drain proves the relay is
        // actually usable, not merely willing to accept `Register`. A relay
        // that registers then fails the drain must NOT reset backoff, or it
        // pins reconnects at the 1s floor forever (a phone radio wake every
        // second — tenet 5: relays are untrusted, and a buggy one does this).
        *backoff = Duration::from_secs(1);
        if !received.is_empty() {
            on_new(received);
        }
        loop {
            // A nudge is a zero-length uni stream — accepting it IS the
            // signal; a failed accept means the connection is gone.
            connection
                .accept_uni()
                .await
                .map_err(|e| format!("connection lost: {e}"))?;
            let received = self
                .drain_connection(relay, &connection, &mut BTreeSet::new())
                .await?;
            if !received.is_empty() {
                on_new(received);
            }
        }
    }

    /// Page through one registered connection's mailbox (the relay caps
    /// each response, so a large mailbox needs several rounds), acking each
    /// page's high-water mark, until a page comes back empty.
    async fn drain_connection(
        &self,
        relay: &str,
        connection: &iroh::endpoint::Connection,
        seen: &mut BTreeSet<[u8; 32]>,
    ) -> Result<Vec<Received>, String> {
        let mut received = Vec::new();
        let mut after = 0u64;
        loop {
            let items = match net::request(connection, MailboxOp::Fetch { after }).await? {
                MailboxResult::Envelopes { items } => items,
                other => return Err(format!("unexpected response from {relay}: {other:?}")),
            };
            if items.is_empty() {
                break;
            }
            let page_cursor = items
                .iter()
                .map(|item| item.cursor)
                .max()
                .expect("non-empty");
            // Relays are untrusted (tenet 5). An honest page always
            // advances (the store yields only `cursor > after`); a
            // non-advancing page is a hostile/buggy relay trying to spin
            // this drain forever. Abandon it — don't loop on its input.
            if page_cursor <= after {
                eprintln!("relay {relay} returned a non-advancing fetch page; abandoning it");
                break;
            }
            for item in items {
                if item.envelope.version != zink_protocol::FORMAT_VERSION
                    || item.envelope.core.version != zink_protocol::FORMAT_VERSION
                {
                    // A future protocol version this client can't parse
                    // (SPEC §10: surfaced, never misparsed). Skipped, and
                    // acked with the page so it doesn't wedge the drain.
                    eprintln!("skipping message with unsupported version");
                    continue;
                }
                if !seen.insert(item.envelope.id().0) {
                    continue; // already drained via another relay
                }
                let body = item.envelope.open(&self.device);
                if body.is_ok() {
                    self.remember(&item.envelope)?;
                }
                received.push(Received {
                    envelope: item.envelope,
                    relay: relay.to_string(),
                    body,
                });
            }
            net::request(connection, MailboxOp::Ack { up_to: page_cursor }).await?;
            after = page_cursor;
        }
        Ok(received)
    }

    /// Fetch + verify + decrypt one blob referenced by a received message:
    /// the local cache first, then the relay it arrived through (caching
    /// the ciphertext for the next time).
    pub async fn fetch_blob(
        &self,
        received: &Received,
        hash: &BlobHash,
    ) -> Result<Vec<u8>, String> {
        self.open_cached_or_fetch(
            &received.envelope,
            hash,
            std::slice::from_ref(&received.relay),
        )
        .await
    }

    /// Fetch + verify + decrypt a blob referenced by a *stored* message:
    /// the local cache first, then this device's home relays (senders push
    /// blobs to their recipients' relays — for stored history, that's us).
    pub async fn fetch_stored_blob(
        &self,
        conversation: MessageId,
        message: MessageId,
        hash: &BlobHash,
    ) -> Result<Vec<u8>, String> {
        let envelope = self.state.load_envelope(conversation, message)?;
        self.open_cached_or_fetch(&envelope, hash, &self.state.home_relays())
            .await
    }

    /// The shared blob path: try the cache, then each relay in turn;
    /// verify + decrypt against the referencing envelope (`open_blob`
    /// checks the hash and the key commitment); cache ciphertext that
    /// proved out. A cache entry that fails to open is ignored, not fatal —
    /// the refetch replaces it.
    async fn open_cached_or_fetch(
        &self,
        envelope: &MessageEnvelope,
        hash: &BlobHash,
        relays: &[String],
    ) -> Result<Vec<u8>, String> {
        if let Some(bytes) = self.state.load_blob(hash)
            && let Ok(plaintext) = envelope.open_blob(&self.device, hash, &bytes)
        {
            return Ok(plaintext);
        }
        let mut last_error = String::from("no relay to fetch from");
        for relay in relays {
            match blobs::fetch_encrypted(&self.endpoint, relay, hash, self.config.connect_timeout)
                .await
            {
                Ok(bytes) => {
                    let plaintext = envelope
                        .open_blob(&self.device, hash, &bytes)
                        .map_err(|e| format!("decrypt blob: {e}"))?;
                    self.state.save_blob(hash, &bytes)?;
                    return Ok(plaintext);
                }
                Err(error) => last_error = error,
            }
        }
        Err(format!("blob fetch failed: {last_error}"))
    }

    /// Every stored conversation, newest first (by wall-clock hint — a
    /// display ordering, like everything timestamp-based).
    pub fn conversations(&self) -> Result<Vec<ConversationSummary>, String> {
        let mut summaries = Vec::new();
        for id in self.state.conversations() {
            let envelopes = self.state.load_envelopes(id)?;
            if envelopes.is_empty() {
                continue;
            }
            let participants: BTreeSet<PublicKey> = envelopes
                .iter()
                .flat_map(|envelope| {
                    envelope
                        .core
                        .recipients
                        .iter()
                        .copied()
                        .chain([envelope.core.sender])
                })
                .collect();
            summaries.push(ConversationSummary {
                id,
                participants: participants.into_iter().collect(),
                message_count: envelopes.len(),
                last_timestamp_ms: envelopes
                    .iter()
                    .map(|envelope| envelope.core.timestamp_ms)
                    .max()
                    .unwrap_or(0),
            });
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_timestamp_ms));
        Ok(summaries)
    }

    /// One conversation's stored messages in the DAG's linearized order.
    /// Bodies are opened per message and never fail the whole history — an
    /// envelope this device cannot open (e.g. sealed before the self-wrap
    /// convention) surfaces as `Err`, honestly, like `Received` does.
    pub fn history(&self, conversation: MessageId) -> Result<Vec<HistoryMessage>, String> {
        let envelopes = self.state.load_envelopes(conversation)?;
        let by_id: BTreeMap<MessageId, &MessageEnvelope> = envelopes
            .iter()
            .map(|envelope| (envelope.id(), envelope))
            .collect();
        let dag = self.state.load_dag(conversation)?;
        let pending = self.state.pending_messages();
        Ok(dag
            .linearize()
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|envelope| {
                let id = envelope.id();
                HistoryMessage {
                    id,
                    sender: envelope.core.sender,
                    timestamp_ms: envelope.core.timestamp_ms,
                    body: envelope.open(&self.device),
                    blob_refs: envelope.core.blob_refs.clone(),
                    pending: pending.contains(&id),
                }
            })
            .collect())
    }

    /// Set this device's display name and home relays — what `my_record`
    /// publishes and what `recv` drains by default.
    pub fn set_profile(&self, name: &str, relays: &[String]) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        for relay in relays {
            net::parse_relay(relay)?;
        }
        self.state.save_profile(name.trim(), relays)
    }

    pub fn profile_name(&self) -> Option<String> {
        self.state.profile_name()
    }

    pub fn home_relays(&self) -> Vec<String> {
        self.state.home_relays()
    }

    /// This device's ContactRecord: key, self-attested name, home relays.
    /// The QR/paste payload is `record.to_qr_string()`.
    pub fn my_record(&self) -> Result<ContactRecord, String> {
        let name = self
            .state
            .profile_name()
            .ok_or("set a profile name first")?;
        let relays = self.state.home_relays();
        if relays.is_empty() {
            return Err("set a home relay first".into());
        }
        let me = self.device.public();
        let attestation = SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: me,
                subject: me,
                claim: Claim::Name(name),
                revision: 0,
            },
            &self.device,
        );
        Ok(ContactRecord::new(vec![me], vec![attestation], relays))
    }

    /// Ensure a mailbox exists on every home relay. Called when publishing
    /// a record: anyone who scans it must be able to deposit immediately —
    /// a record that names a relay where you have no mailbox is a lie.
    pub async fn register_at_home_relays(&self) -> Result<(), String> {
        for relay in self.state.home_relays() {
            let connection = net::connect(
                &self.endpoint,
                &relay,
                zink_protocol::MAILBOX_ALPN,
                self.config.connect_timeout,
            )
            .await?;
            net::request(&connection, MailboxOp::Register).await?;
        }
        Ok(())
    }

    /// Store a scanned/pasted record. The petname defaults to the contact's
    /// self-claimed name; the caller may override (petnames are ours, not
    /// theirs). Returns the petname it was stored under.
    pub fn add_contact(
        &self,
        record: &ContactRecord,
        petname: Option<String>,
    ) -> Result<String, String> {
        if record.keys.is_empty() {
            return Err("record has no keys".into());
        }
        if record.relays.is_empty() {
            return Err("record has no relays — no way to reach them".into());
        }
        let petname = petname
            .or_else(|| record.self_claimed_name().map(str::to_string))
            .ok_or("record has no valid self-claimed name; provide a petname")?;
        // A petname must resolve to one person: reject collisions with a
        // *different* key; re-adding the same person updates their record.
        for (existing_name, existing) in self.state.contacts()? {
            if existing_name == petname && existing.keys.first() != record.keys.first() {
                return Err(format!("a different contact is already named {petname:?}"));
            }
        }
        self.state.save_contact(&petname, record)?;
        Ok(petname)
    }

    /// All stored contacts as `(petname, record)`.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, String> {
        self.state.contacts()
    }

    /// Petname → the Contact to send to.
    pub fn resolve_contact(&self, petname: &str) -> Result<Contact, String> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .map(|(_, record)| Contact::from_record(&record))
            .ok_or_else(|| format!("no contact named {petname:?}"))
    }

    /// Persist a verified envelope and its participant→conversation mapping,
    /// so a later `send` to the same people threads into this conversation.
    fn remember(&self, envelope: &MessageEnvelope) -> Result<(), String> {
        let conversation = envelope.core.conversation.unwrap_or_else(|| envelope.id());
        self.state.store_envelope(conversation, envelope)?;
        let participants: BTreeSet<PublicKey> = envelope
            .core
            .recipients
            .iter()
            .copied()
            .chain([envelope.core.sender])
            .collect();
        self.state.record_conversation(&participants, conversation)
    }
}

/// A resolved recipient: the person's device keys and the relays hosting
/// their mailboxes.
pub struct Contact {
    pub keys: Vec<PublicKey>,
    pub relays: Vec<String>,
}

impl Contact {
    /// `<pubkey-hex>@<relay>[,<relay>…]` — hex contains no `@`, so the
    /// first `@` splits key from relay list. The raw escape hatch next to
    /// named contacts.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (key_hex, relay_list) = spec
            .split_once('@')
            .ok_or("contact must be <pubkey>@<relay>[,<relay>...]")?;
        let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
        for relay in &relays {
            net::parse_relay(relay)?; // validate early, before any network work
        }
        Ok(Contact {
            keys: vec![PublicKey(hex::parse32(key_hex)?)],
            relays,
        })
    }

    fn from_record(record: &ContactRecord) -> Self {
        Contact {
            keys: record.keys.clone(),
            relays: record.relays.clone(),
        }
    }
}

pub struct SendReceipt {
    pub id: MessageId,
    pub conversation: MessageId,
    pub seq: u64,
    pub blob_count: usize,
    pub relay_count: usize,
    /// Relays that did not take the delivery — queued in the outbox for a
    /// later flush. `0` = fully delivered.
    pub pending_relays: usize,
}

/// What one outbox flush accomplished.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlushReport {
    pub delivered: usize,
    pub pending: usize,
    /// Entries past the give-up window: left in place, no longer retried.
    pub expired: usize,
}

/// One fetched envelope: opened if this device could decrypt it. The edge
/// decides presentation; `envelope.core` has sender, conversation, blob refs.
pub struct Received {
    pub envelope: MessageEnvelope,
    /// The relay it arrived through — where its blobs can be fetched.
    pub relay: String,
    pub body: Result<Vec<u8>, OpenError>,
}

/// Whom a reply reaches: the resolvable participants, and the keys we hold
/// no record for (unreachable — surfaced, not silently dropped).
pub struct ReplyContacts {
    pub contacts: Vec<Contact>,
    pub unknown: Vec<PublicKey>,
}

/// One stored conversation, as the edge lists it. Participants are keys —
/// naming them is the edge's policy (petnames, hex, whatever).
pub struct ConversationSummary {
    pub id: MessageId,
    /// Every key seen in the conversation (senders ∪ recipients), sorted;
    /// includes this device.
    pub participants: Vec<PublicKey>,
    pub message_count: usize,
    /// Largest wall-clock hint seen — display ordering only, never trusted.
    pub last_timestamp_ms: u64,
}

/// One message out of a stored conversation, in linearized order.
pub struct HistoryMessage {
    pub id: MessageId,
    pub sender: PublicKey,
    /// The sender's wall-clock hint — display only.
    pub timestamp_ms: u64,
    pub body: Result<Vec<u8>, OpenError>,
    pub blob_refs: Vec<BlobRef>,
    /// True while ≥1 relay is still owed this message (outbox entry
    /// present) — including entries past the give-up window (undelivered).
    pub pending: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}
