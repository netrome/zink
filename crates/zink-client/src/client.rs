//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::Endpoint;
use rand_core::{OsRng, RngCore};
use zink_protocol::{
    Attestation, BlobDraft, BlobHash, BlobRef, Claim, ContactRecord, DeviceKey, FORMAT_VERSION,
    MailboxOp, MailboxResult, MessageDraft, MessageEnvelope, MessageId, OpenError, PublicKey,
    RelayEntry, SYNC_ALPN, SignedAttestation, SyncOp, SyncResult, distinct_relays,
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
    /// The client is also a server: this router serves `SYNC_ALPN` (peer
    /// history sync, D0) for as long as the client lives. Held, not called.
    _sync_router: iroh::protocol::Router,
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
        // State first: the endpoint homes to the profile's relay URLs (D0b),
        // and iroh fixes the relay transport at bind time — so a relay added
        // to the profile takes effect for the peer layer on the next open.
        let state = ClientState::open(key_path);
        let home_relays: Vec<iroh::RelayUrl> = state
            .home_relay_entries()
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref())
            .map(net::parse_relay_url)
            .collect::<Result<_, _>>()?;
        let endpoint = net::bind_endpoint(&device, &home_relays).await?;
        // Serve peer history sync on our own endpoint (D0). The endpoint is a
        // cheap handle to clone; the router keeps the serve loop alive.
        // Contacts-only serving gate (D0c) — our key is the always-allowed one.
        let sync_router =
            crate::sync::spawn_sync_router(endpoint.clone(), state.clone(), device.public());
        Ok(Self {
            device,
            endpoint,
            state,
            config,
            _sync_router: sync_router,
        })
    }

    /// Graceful shutdown for short-lived edges (the CLI): since the endpoint
    /// homes to a relay (D0b) it holds a live transport, and dropping that
    /// without closing makes iroh log an ungraceful-abort error on every
    /// one-shot command. Long-lived edges (the app) never call this.
    pub async fn close(self) {
        let _ = self._sync_router.shutdown().await;
        self.endpoint.close().await;
    }

    /// This client's peer dial string `<endpoint-id>@<ip:port>` — how another
    /// device reaches us on `SYNC_ALPN` to backfill history when it knows
    /// our address explicitly (same-LAN / dev tooling). The deployment path
    /// is dial-by-key via our home relay (`backfill_by_key`, D0b).
    pub fn sync_address(&self) -> Result<String, String> {
        let addr = self.endpoint.addr();
        let sock = addr.ip_addrs().next().ok_or("no bound address yet")?;
        Ok(format!("{}@{}", self.endpoint.id(), sock))
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
                    relays: record.relays.iter().map(|r| r.mailbox.clone()).collect(),
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

    /// The shared send tail: seal, persist
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
                    tracing::warn!(relay, %error, "delivery failed; queued for retry");
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
                    tracing::warn!(%error, "dropping unfulfillable outbox entry");
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
                        tracing::warn!(blob = %hex::encode(&blob_ref.hash.0), "blob missing from cache; delivering without it");
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
                    tracing::warn!(relay = %entry.relay, %error, "outbox retry failed");
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
        // Distinguishes the *poll* path from the nudge path in the logs: a
        // message that shows up here but not via "drained (nudge)" arrived
        // slowly (fell back to the poll) — the signature of a missed nudge.
        if !received.is_empty() {
            tracing::info!(count = received.len(), "drained (poll)");
        }
        // Auto-sync (D0d): heal orphaned conversations before returning, so
        // the caller sees a threadable history. Cheap when nothing is
        // orphaned (one missing-ancestors scan per touched conversation).
        self.auto_sync(&received).await;
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
                Err(error) => tracing::warn!(relay, %error, "subscription dropped"),
            }
            // ±50% jitter so a relay restart doesn't get a thundering herd.
            let jitter = 0.5 + f64::from(OsRng.next_u32()) / f64::from(u32::MAX);
            let delay = backoff.mul_f64(jitter);
            tracing::debug!(relay, ?delay, "reconnecting after backoff");
            n0_future::time::sleep(delay).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// One subscription session: lives until the connection dies. Resets
    /// `backoff` only after a full register+drain (see below), not on bare
    /// `Register`.
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
        tracing::info!(relay, "subscription live (registered)");
        // Catch up on what arrived while we were away *first* — incoming
        // messages take priority over retrying the outbox. Flushing before
        // the drain would delay catch-up by the backlog's timeouts (10s per
        // dead entry), the same coupling removed from the send path. Flush
        // after (the reconnect still means "network is back", §2).
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
            tracing::info!(relay, count = received.len(), "drained (catch-up)");
            // Heal before rendering (D0d): the edge's re-render then shows
            // the whole conversation, not an unthreadable orphan.
            self.auto_sync(&received).await;
            on_new(received);
        }
        let _ = self.flush_outbox().await;
        loop {
            // A nudge is a zero-length uni stream — accepting it IS the
            // signal; a failed accept means the connection is gone.
            connection
                .accept_uni()
                .await
                .map_err(|e| format!("connection lost: {e}"))?;
            let started = std::time::Instant::now();
            let received = self
                .drain_connection(relay, &connection, &mut BTreeSet::new())
                .await?;
            tracing::info!(
                relay,
                count = received.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "drained (nudge)"
            );
            if !received.is_empty() {
                // Heal before rendering (D0d). Costs nothing when the
                // conversation is ancestor-closed (the common case); dials
                // the sender only on an actual orphan.
                self.auto_sync(&received).await;
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
                tracing::warn!(
                    relay,
                    "relay returned a non-advancing fetch page; abandoning it"
                );
                break;
            }
            for item in items {
                if item.envelope.version != zink_protocol::FORMAT_VERSION
                    || item.envelope.core.version != zink_protocol::FORMAT_VERSION
                {
                    // A future protocol version this client can't parse
                    // (SPEC §10: surfaced, never misparsed). Skipped, and
                    // acked with the page so it doesn't wedge the drain.
                    tracing::warn!("skipping message with unsupported version");
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
    /// publishes and what `recv` drains by default. Each relay is the spec
    /// `zink-relay` prints: `<endpoint-id>@<ip:port>[#<relay-url>]` — the
    /// mailbox dial string, plus the same service's iroh relay URL, which
    /// makes this device reachable by key (D0b; applied at the next open,
    /// since the endpoint's relay transport is fixed at bind time).
    pub fn set_profile(&self, name: &str, relays: &[String]) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        let entries: Vec<RelayEntry> = relays.iter().map(|s| RelayEntry::from_spec(s)).collect();
        for entry in &entries {
            net::parse_relay(&entry.mailbox)?;
            if let Some(url) = &entry.relay_url {
                net::parse_relay_url(url)?;
            }
        }
        self.state.save_profile(name.trim(), &entries)
    }

    pub fn profile_name(&self) -> Option<String> {
        self.state.profile_name()
    }

    /// The home relays' mailbox dial strings — what the mailbox paths
    /// (recv, subscribe, register) dial.
    pub fn home_relays(&self) -> Vec<String> {
        self.state.home_relays()
    }

    /// The home relays as full specs (`dial[#relay-url]`) — the round-trip
    /// form: what an edge shows in a profile form and feeds back into
    /// `set_profile`. Using `home_relays` there instead would silently drop
    /// the relay URL on a re-save.
    pub fn home_relay_specs(&self) -> Vec<String> {
        self.state
            .home_relay_entries()
            .iter()
            .map(RelayEntry::to_spec)
            .collect()
    }

    /// This device's ContactRecord: key, self-attested name, home relays.
    /// The QR/paste payload is `record.to_qr_string()`.
    pub fn my_record(&self) -> Result<ContactRecord, String> {
        let name = self
            .state
            .profile_name()
            .ok_or("set a profile name first")?;
        let relays = self.state.home_relay_entries();
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

    /// Sync a partially-known conversation with a peer (SPEC §5.2): walk
    /// `parents` **backward** to the genesis (what lets a device added
    /// mid-conversation build the DAG and reply — without the genesis,
    /// `load_dag` can't even start), then pull **forward** via
    /// `get-successors` (D0d — catches messages that expired from the
    /// mailbox or live on concurrent branches). `from` is the peer's
    /// `<endpoint-id>@<ip:port>`.
    ///
    /// Best-effort (tenet 6): an unreachable peer, or one that declines to
    /// serve, just stops the walk — we never fabricate a root. A served peer
    /// is trusted no more than a relay: every envelope is verified, checked
    /// to be the id we asked for, and checked to belong to this conversation
    /// before it's stored. Returns the number of newly-stored messages.
    pub async fn backfill(&self, conversation: MessageId, from: &str) -> Result<usize, String> {
        self.backfill_addr(conversation, net::parse_relay(from)?)
            .await
    }

    /// `backfill` reaching the peer **by key alone** (D0b): the peer's relay
    /// URLs come from their stored ContactRecord, iroh routes the initial
    /// signaling via their relay and holepunches to a direct path (relaying
    /// the encrypted QUIC as fallback). The device key IS the endpoint key —
    /// no lookup service involved. Fails without a stored record carrying a
    /// relay URL for `peer` (a mailbox-only record can't rendezvous).
    pub async fn backfill_by_key(
        &self,
        conversation: MessageId,
        peer: PublicKey,
    ) -> Result<usize, String> {
        let records = self.state.contacts()?;
        let record = records
            .iter()
            .find(|(_, record)| record.keys.contains(&peer))
            .map(|(_, record)| record)
            .ok_or("no stored contact record for that key")?;
        let relay_urls: Vec<iroh::RelayUrl> = record
            .relays
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref())
            .map(net::parse_relay_url)
            .collect::<Result<_, _>>()?;
        if relay_urls.is_empty() {
            return Err(
                "contact record has no relay url — re-exchange records to enable dial-by-key"
                    .into(),
            );
        }
        self.backfill_addr(conversation, net::peer_addr(&peer, &relay_urls)?)
            .await
    }

    /// `backfill` with the peer address already resolved — the seam the string
    /// API parses into, and the one tests use to dial a locally-bound peer's
    /// full multi-address `EndpointAddr` (a bare public socket isn't reliably
    /// self-reachable on one host).
    async fn backfill_addr(
        &self,
        conversation: MessageId,
        from: iroh::EndpointAddr,
    ) -> Result<usize, String> {
        // A hostile peer could feed an unbounded fake chain; bound the walk
        // (shared across the backward and forward passes).
        const MAX_SYNC_FETCH: usize = 10_000;
        let connection =
            net::connect_addr(&self.endpoint, from, SYNC_ALPN, self.config.connect_timeout).await?;
        let mut fetched = 0usize;
        self.fill_backward(conversation, &connection, &mut fetched, MAX_SYNC_FETCH)
            .await?;
        self.fill_forward(conversation, &connection, &mut fetched, MAX_SYNC_FETCH)
            .await?;
        Ok(fetched)
    }

    /// The backward pass: fetch referenced-but-missing parents until the
    /// stored slice is ancestor-closed (genesis reached) or the peer stops
    /// yielding.
    async fn fill_backward(
        &self,
        conversation: MessageId,
        connection: &iroh::endpoint::Connection,
        fetched: &mut usize,
        cap: usize,
    ) -> Result<(), String> {
        loop {
            let frontier = self.state.missing_ancestors(conversation);
            if frontier.is_empty() {
                break; // ancestor-closed: the genesis (parents=[]) is reachable
            }
            let mut progressed = false;
            for id in frontier {
                if *fetched >= cap {
                    tracing::warn!("sync hit the fetch cap; stopping");
                    return Ok(());
                }
                if self.fetch_one(connection, id, conversation).await? {
                    *fetched += 1;
                    progressed = true;
                }
            }
            if !progressed {
                break; // this peer can't take us any closer to the genesis
            }
        }
        Ok(())
    }

    /// The forward pass (D0d): `get-successors` to learn children we lack —
    /// messages the mailbox never delivered (expired, or sent while we were
    /// unreachable) and concurrent branches. The first round queries every
    /// stored id (a fork can hang off any interior message); later rounds
    /// query only what the previous round fetched, so the walk converges.
    /// Chatty at one round-trip per id — fine at friend/family scale.
    async fn fill_forward(
        &self,
        conversation: MessageId,
        connection: &iroh::endpoint::Connection,
        fetched: &mut usize,
        cap: usize,
    ) -> Result<(), String> {
        let mut stored: BTreeSet<MessageId> = self
            .state
            .load_envelopes(conversation)
            .unwrap_or_default()
            .iter()
            .map(|envelope| envelope.id())
            .collect();
        let mut query: Vec<MessageId> = stored.iter().copied().collect();
        while !query.is_empty() {
            let mut learned: Vec<MessageId> = Vec::new();
            for id in query {
                let ids = match net::sync_request(connection, SyncOp::GetSuccessors { id }).await? {
                    SyncResult::Successors { ids } => ids,
                    other => return Err(format!("unexpected sync response: {other:?}")),
                };
                for child in ids {
                    if *fetched >= cap {
                        tracing::warn!("sync hit the fetch cap; stopping");
                        return Ok(());
                    }
                    if stored.contains(&child) {
                        continue;
                    }
                    if self.fetch_one(connection, child, conversation).await? {
                        stored.insert(child);
                        learned.push(child);
                        *fetched += 1;
                    }
                }
            }
            query = learned;
        }
        Ok(())
    }

    /// One `get` round-trip: fetch `id`, validate, store. `Ok(true)` iff a
    /// new envelope was stored. A served peer is trusted no more than a
    /// relay: the envelope must hash to the id we asked for, carry a valid
    /// sender signature, and belong to the conversation being synced — the
    /// last check matters for the forward pass, where ids are the *peer's
    /// claim* rather than parents read from envelopes we already verified.
    async fn fetch_one(
        &self,
        connection: &iroh::endpoint::Connection,
        id: MessageId,
        conversation: MessageId,
    ) -> Result<bool, String> {
        match net::sync_request(connection, SyncOp::Get { id }).await? {
            SyncResult::Envelope { envelope } => {
                if envelope.id() != id {
                    tracing::warn!("peer returned a mismatched id; skipping");
                    return Ok(false);
                }
                if envelope.version != FORMAT_VERSION || envelope.core.version != FORMAT_VERSION {
                    tracing::warn!("skipping synced message with unsupported version");
                    return Ok(false);
                }
                if envelope.verify().is_err() {
                    tracing::warn!("peer returned an unverifiable envelope; skipping");
                    return Ok(false);
                }
                if envelope.core.conversation.unwrap_or_else(|| envelope.id()) != conversation {
                    tracing::warn!("peer served a message from another conversation; skipping");
                    return Ok(false);
                }
                self.remember(&envelope)?;
                Ok(true)
            }
            SyncResult::NotHeld => Ok(false), // peer doesn't have it / declined
            other => Err(format!("unexpected sync response: {other:?}")),
        }
    }

    /// Auto-sync (D0d): after a drain stores new messages, heal every
    /// conversation left with missing ancestors by syncing from the received
    /// message's `sender` — the peer most likely to hold the history
    /// (sync-primitives.md §5). Runs *before* the edge renders, so a healed
    /// conversation appears whole. Best-effort and non-fatal: an unreachable
    /// sender or a missing/mailbox-only record just logs — a drain must
    /// never fail because a peer can't be dialed. Returns messages fetched.
    async fn auto_sync(&self, received: &[Received]) -> usize {
        let me = self.device.public();
        let mut targets: BTreeMap<MessageId, PublicKey> = BTreeMap::new();
        for message in received {
            let sender = message.envelope.core.sender;
            if sender == me {
                continue;
            }
            let conversation = message
                .envelope
                .core
                .conversation
                .unwrap_or_else(|| message.envelope.id());
            targets.entry(conversation).or_insert(sender);
        }
        let mut healed = 0usize;
        for (conversation, sender) in targets {
            if self.state.missing_ancestors(conversation).is_empty() {
                continue; // ancestor-closed — nothing to heal
            }
            match self.backfill_by_key(conversation, sender).await {
                Ok(fetched) => {
                    healed += fetched;
                    tracing::info!(fetched, "auto-sync healed a conversation");
                }
                Err(error) => tracing::debug!(%error, "auto-sync could not reach the sender"),
            }
        }
        healed
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
            relays: record.relays.iter().map(|r| r.mailbox.clone()).collect(),
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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use zink_protocol::{KeyCommitment, MessageCore};

    /// A key path in a per-test temp dir (tests run in parallel, so the dir is
    /// namespaced by `test` — a shared root would let one test's cleanup delete
    /// another's files mid-run). The caller cleans up with `temp_root(test)`.
    fn temp_key(test: &str, name: &str) -> String {
        let dir = temp_root(test);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(name).to_string_lossy().into_owned()
    }

    fn temp_root(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("zink-client-sync-{}-{test}", std::process::id()))
    }

    /// A signed linear chain: genesis (seq/logical 0), then children each
    /// threading onto the previous — what a real send would produce, enough to
    /// rebuild the DAG. Bodies are empty (the backfill test never decrypts).
    fn chain(author: &DeviceKey, recipient: PublicKey, len: u64) -> Vec<MessageEnvelope> {
        let mut envelopes: Vec<MessageEnvelope> = Vec::new();
        for seq in 0..len {
            let (conversation, parents) = match envelopes.first() {
                None => (None, vec![]),
                Some(genesis) => (Some(genesis.id()), vec![envelopes.last().unwrap().id()]),
            };
            let core = MessageCore {
                version: FORMAT_VERSION,
                conversation,
                parents,
                recipients: vec![recipient],
                sender: author.public(),
                seq,
                logical: seq,
                timestamp_ms: 0,
                body: vec![],
                key_commit: KeyCommitment([0; 32]),
                blob_refs: vec![],
            };
            envelopes.push(MessageEnvelope::new(core, author));
        }
        envelopes
    }

    #[tokio::test]
    async fn backfill__should_walk_a_conversation_back_to_its_genesis() {
        // Given: a 3-message conversation A holds in full, while B — added
        // mid-conversation — holds only the latest message, so it can't even
        // build the DAG (no genesis on disk) and thus can't reply.
        let a = Client::open_or_create(&temp_key("walk", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("walk", "client"))
            .await
            .expect("open B");
        befriend(&a, &b); // the D0c gate serves contacts only

        let author = DeviceKey::from_seed([9; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        let latest = msgs.last().unwrap();
        b.state.store_envelope(conversation, latest).unwrap();
        assert!(
            b.state.load_dag(conversation).is_err(),
            "B lacks the genesis before backfill"
        );

        // When: B backfills the missing ancestors from A (dialing A's full
        // address — a locally-bound peer's bare public socket isn't reliably
        // self-reachable on one host; the string API is exercised separately)
        let fetched = b
            .backfill_addr(conversation, a.endpoint.addr())
            .await
            .expect("backfill");

        // Then: B pulled the two missing ancestors, can now build the DAG, and
        // would thread a reply onto the true head at the next logical clock
        assert_eq!(fetched, 2, "genesis + the middle message");
        let dag = b
            .state
            .load_dag(conversation)
            .expect("DAG builds after backfill");
        assert_eq!(dag.heads(), vec![latest.id()]);
        assert_eq!(dag.next_logical(), 3);

        let _ = std::fs::remove_dir_all(temp_root("walk"));
    }

    /// An in-process iroh relay server (plain HTTP, `tls: None` — the same
    /// shape the `zink-relay` binary embeds). Returns the handle (kept alive
    /// by the caller) and its relay URL.
    async fn spawn_test_relay() -> (iroh_relay::server::Server, String) {
        use iroh_relay::server::{RelayConfig, Server, ServerConfig};
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new((std::net::Ipv4Addr::LOCALHOST, 0)));
        let server = Server::spawn(config).await.expect("spawn iroh relay");
        let url = format!("http://{}", server.http_addr().expect("relay http addr"));
        (server, url)
    }

    /// Store `requester`'s key as a contact of `server`, so the D0c
    /// contacts-only serving gate lets the requester's sync calls through
    /// (the minimal record — key only — via the store, skipping
    /// `add_contact`'s reachability validation, which serving doesn't need).
    fn befriend(server: &Client, requester: &Client) {
        let record = ContactRecord::new(vec![requester.public_key()], vec![], vec![]);
        server
            .state
            .save_contact("requester", &record)
            .expect("save contact");
    }

    /// A profile whose relay entry carries `relay_url` — written straight to
    /// state so the endpoint homes to it at the *next* open (the D0b
    /// restart-to-apply semantics; the mailbox dial string is never used
    /// here). Returns the homed client.
    async fn open_homed(test: &str, name: &str, relay_url: &str) -> Client {
        let key_path = temp_key(test, name);
        ClientState::open(&key_path)
            .save_profile(
                name,
                &[RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some(relay_url.to_string()),
                }],
            )
            .expect("save profile");
        Client::open_or_create(&key_path)
            .await
            .expect("open client")
    }

    #[tokio::test]
    async fn backfill_by_key__should_reach_a_peer_via_its_relay_across_two_relays() {
        // Given: two relay services; A homes to one, B to the other — the
        // D0b acceptance shape (never a single shared relay). B knows only
        // A's *key* plus A's stored ContactRecord naming A's relay URL.
        let (_relay_a, url_a) = spawn_test_relay().await;
        let (_relay_b, url_b) = spawn_test_relay().await;
        let a = open_homed("bykey", "server", &url_a).await;
        let b = open_homed("bykey", "client", &url_b).await;
        befriend(&a, &b); // the D0c gate serves contacts only
        a.endpoint.online().await; // A must be homed before B rendezvouses via its relay

        let author = DeviceKey::from_seed([5; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();
        let record = ContactRecord::new(
            vec![a.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some(url_a.clone()),
            }],
        );
        b.add_contact(&record, Some("a".to_string()))
            .expect("add contact");

        // When: B backfills by key alone — no ip:port anywhere; iroh
        // rendezvouses via A's relay and holepunches (or relays) from there.
        let fetched = b
            .backfill_by_key(conversation, a.public_key())
            .await
            .expect("backfill by key");

        // Then: the missing ancestors arrived and the DAG is reply-ready.
        assert_eq!(fetched, 2, "genesis + the middle message");
        let dag = b.state.load_dag(conversation).expect("DAG builds");
        assert_eq!(dag.next_logical(), 3);

        let _ = std::fs::remove_dir_all(temp_root("bykey"));
    }

    #[tokio::test]
    async fn backfill__should_be_refused_until_the_requester_is_a_contact() {
        // Given: A holds a full conversation; B — NOT in A's contact store —
        // holds only the latest message. D0b made peers dialable by anyone
        // holding key + relay URL; the D0c gate is what keeps "dialable"
        // from meaning "served".
        let a = Client::open_or_create(&temp_key("gate", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("gate", "client"))
            .await
            .expect("open B");
        let author = DeviceKey::from_seed([8; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();

        // When: the stranger backfills — the answers must be
        // indistinguishable from a peer that holds nothing
        let fetched = b
            .backfill_addr(conversation, a.endpoint.addr())
            .await
            .expect("gate declines, not errors");

        // Then: nothing served, and the successor view is empty too
        assert_eq!(fetched, 0, "a non-contact is served nothing");
        assert!(b.state.load_dag(conversation).is_err());
        let connection = net::connect_addr(
            &b.endpoint,
            a.endpoint.addr(),
            SYNC_ALPN,
            b.config.connect_timeout,
        )
        .await
        .expect("connect");
        let successors = net::sync_request(
            &connection,
            SyncOp::GetSuccessors {
                id: conversation, // the genesis id — A holds its children
            },
        )
        .await
        .expect("round-trip");
        assert_eq!(
            successors,
            SyncResult::Successors { ids: vec![] },
            "successors of a held message hide behind the gate too"
        );

        // When: A stores B's record — B is now a contact and gets served
        befriend(&a, &b);
        let fetched = b
            .backfill_addr(conversation, a.endpoint.addr())
            .await
            .expect("backfill as a contact");

        // Then: the walk reaches the genesis
        assert_eq!(fetched, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conversation).is_ok());

        let _ = std::fs::remove_dir_all(temp_root("gate"));
    }

    #[tokio::test]
    async fn backfill__should_pull_forward_successors_after_the_backward_walk() {
        // Given: A holds a 5-message chain; B holds only the MIDDLE message —
        // missing both its ancestors (backward) and everything sent after it
        // (forward — e.g. expired from B's mailbox before B fetched).
        let a = Client::open_or_create(&temp_key("forward", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("forward", "client"))
            .await
            .expect("open B");
        befriend(&a, &b);
        let author = DeviceKey::from_seed([4; 32]);
        let msgs = chain(&author, b.public_key(), 5);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state.store_envelope(conversation, &msgs[2]).unwrap();

        // When
        let fetched = b
            .backfill_addr(conversation, a.endpoint.addr())
            .await
            .expect("sync");

        // Then: 2 ancestors + 2 successors, and the DAG ends on the true head
        assert_eq!(fetched, 4);
        let dag = b.state.load_dag(conversation).expect("DAG builds");
        assert_eq!(dag.heads(), vec![msgs[4].id()]);
        assert_eq!(dag.next_logical(), 5);

        let _ = std::fs::remove_dir_all(temp_root("forward"));
    }

    #[tokio::test]
    async fn auto_sync__should_heal_an_orphaned_conversation_from_its_sender() {
        // Given: A authored a 3-message conversation to B and serves it
        // (homed to its own relay); B — on a different relay — receives only
        // the latest message, as a mid-conversation joiner would. B holds
        // A's record (key + relay URL), as any messageable contact does.
        let (_relay_a, url_a) = spawn_test_relay().await;
        let (_relay_b, url_b) = spawn_test_relay().await;
        let a = open_homed("autosync", "server", &url_a).await;
        let b = open_homed("autosync", "client", &url_b).await;
        befriend(&a, &b);
        a.endpoint.online().await;

        let msgs = chain(&a.device, b.public_key(), 3);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        let latest = msgs.last().unwrap();
        b.state.store_envelope(conversation, latest).unwrap();
        let record = ContactRecord::new(
            vec![a.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some(url_a.clone()),
            }],
        );
        b.add_contact(&record, Some("a".to_string()))
            .expect("add contact");
        assert!(b.state.load_dag(conversation).is_err(), "orphaned before");

        // When: the drain hands the orphan to auto-sync (what recv and the
        // subscription loops now do) — the sender is dialed by key
        let healed = b
            .auto_sync(&[Received {
                envelope: latest.clone(),
                relay: String::new(),
                body: Ok(vec![]),
            }])
            .await;

        // Then: the conversation is whole with zero explicit action
        assert_eq!(healed, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conversation).is_ok());

        let _ = std::fs::remove_dir_all(temp_root("autosync"));
    }

    #[tokio::test]
    async fn backfill_by_key__should_fail_plainly_without_a_relay_url_in_the_record() {
        // Given: a stored record that is mailbox-only (raw-contact shape)
        let b = Client::open_or_create(&temp_key("nourl", "client"))
            .await
            .expect("open B");
        let peer = DeviceKey::from_seed([6; 32]).public();
        let record = ContactRecord::new(
            vec![peer],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        b.add_contact(&record, Some("peer".to_string()))
            .expect("add contact");

        // When / Then: dial-by-key is impossible and says so — no fabricated
        // reachability, no hang.
        let err = b
            .backfill_by_key(MessageId([1; 32]), peer)
            .await
            .expect_err("no relay url to rendezvous through");
        assert!(err.contains("no relay url"), "got: {err}");

        let _ = std::fs::remove_dir_all(temp_root("nourl"));
    }

    #[tokio::test]
    async fn backfill__should_stop_when_the_peer_lacks_the_ancestors() {
        // Given: B holds only the latest message; A (the peer) holds nothing.
        let a = Client::open_or_create(&temp_key("stuck", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("stuck", "client"))
            .await
            .expect("open B");
        let author = DeviceKey::from_seed([7; 32]);
        let msgs = chain(&author, b.public_key(), 3);
        let conversation = msgs[0].id();
        b.state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();

        // When: B backfills from a peer that serves nothing
        let fetched = b
            .backfill_addr(conversation, a.endpoint.addr())
            .await
            .expect("backfill returns Ok even with nothing to fetch");

        // Then: it fetches nothing and gives up rather than looping — honesty
        // over a fabricated root (the genesis is still missing).
        assert_eq!(fetched, 0);
        assert!(b.state.load_dag(conversation).is_err());

        let _ = std::fs::remove_dir_all(temp_root("stuck"));
    }
}
