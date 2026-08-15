//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rand_core::OsRng;
use zink_protocol::{
    Attestation, BlobHash, BlobRef, Claim, ContactRecord, DeviceKey, EncryptedBlob,
    MAX_GET_KEYS_IDS, MessageEnvelope, MessageId, OpenError, PublicKey, RelayEntry, SYNC_ALPN,
    SignedAttestation, SyncOp, SyncResult, Versioned, open_avatar, seal_avatar,
};

use crate::adapters::iroh::IrohTransport;
use crate::adapters::system_clock::SystemClock;
use crate::adapters::system_rng::SystemRng;
use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::{Peer, Request, Transport};
use crate::reach::ReachLedger;
use crate::state::ClientState;
use crate::{blobs, hex, keystore, net};

mod contacts;
mod outbox;
mod recv;
mod send;
mod who_is;

pub use contacts::{Contact, DeviceEvidence, Disavowal, LearnedName, ResolvedName};
pub use outbox::FlushReport;
pub use recv::{Received, RecvReport, RelayFailure};
pub use send::{ReplyContacts, SendReceipt, StagedSend};
pub use who_is::{WhoIsAnswer, WhoIsOutcome};

/// Tuning the edges inject at construction; `Default` fits production.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Deadline for reaching a relay. Long enough for a phone on flaky
    /// cellular; tests exercising down-relay paths shrink it.
    pub connect_timeout: Duration,
    /// How long `close` waits for the endpoint to shut down gracefully.
    /// After a direct dial that got nowhere (D5), iroh spends ~3 s settling
    /// the relay-path machinery that dial started; a one-shot edge pays that
    /// per command. The default is generous enough to stay graceful — cutting
    /// it short is *correct* but makes iroh log an ungraceful-abort error, so
    /// only an edge that prefers speed to a clean log (the e2e harness)
    /// shortens it.
    pub close_deadline: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            close_deadline: Duration::from_secs(5),
        }
    }
}

pub struct Client<
    C: Clock = SystemClock,
    W: WallClock = SystemClock,
    N: Transport = IrohTransport,
    R: Draw = SystemRng,
> {
    device: DeviceKey,
    /// The network, behind ports (`crate::ports::transport`,
    /// `docs/design/transport.md`). `IrohTransport` in production.
    transport: N,
    state: ClientState,
    config: ClientConfig,
    /// Monotonic time, behind a port. `SystemClock` in production; see
    /// `crate::ports::clock`.
    clock: C,
    /// Wall time, behind its own port: it moves independently of `clock` in
    /// the real world, so tests can drive them apart.
    wall_clock: W,
    /// Timing entropy (reconnect jitter), behind a port so no domain code
    /// draws ambient randomness; see `crate::ports::rng`. `SystemRng` in
    /// production.
    rng: R,
    /// The auto-query rate limit; the rationale lives on `who_is::AskedOnce`.
    asked: who_is::AskedOnce,
    /// The client is also a server: this task pulls inbound sync requests
    /// (peer history sync, D0; direct delivery, D5) off the transport for as
    /// long as the client lives. Aborted on drop.
    _serve_task: n0_future::task::AbortOnDropHandle<()>,
    /// The edge's sink for directly-delivered messages (D5), shared with the
    /// router's handler. Registered after open via `on_direct_delivery`.
    direct_sink: crate::sync::DirectSink,
    /// Per-peer direct reachability (D5), shared with the router so an
    /// inbound connection counts as evidence. See `crate::reach`.
    reach: ReachLedger,
}

/// Production constructors: they read keys from disk and bind a real endpoint,
/// so they always build a `Client<SystemClock, SystemClock>`.
impl Client<SystemClock, SystemClock> {
    /// Open with an existing key (the CLI path — `keygen` created it).
    pub async fn open(key_path: &str) -> Result<Self, Error> {
        Self::open_with(key_path, ClientConfig::default()).await
    }

    /// `open` with edge-injected tuning.
    pub async fn open_with(key_path: &str, config: ClientConfig) -> Result<Self, Error> {
        Self::with_device(
            keystore::load(key_path)?,
            key_path,
            config,
            SystemClock,
            SystemClock,
        )
        .await
    }

    /// Open, creating the key on first run (the app path).
    pub async fn open_or_create(key_path: &str) -> Result<Self, Error> {
        Self::with_device(
            keystore::load_or_create(key_path)?,
            key_path,
            ClientConfig::default(),
            SystemClock,
            SystemClock,
        )
        .await
    }
}

/// Constructors that bind the real network: they build an `IrohTransport`
/// from the profile's home relays, fixing `N` while staying generic over the
/// clocks (tests inject `TestClock`s and still dial real iroh).
impl<C: Clock, W: WallClock> Client<C, W, IrohTransport> {
    async fn with_device(
        device: DeviceKey,
        key_path: &str,
        config: ClientConfig,
        clock: C,
        wall_clock: W,
    ) -> Result<Self, Error> {
        // State first: the endpoint homes to the profile's relay URLs (D0b).
        let state = ClientState::open(key_path);
        let home_relays: Vec<String> = state
            .home_relay_entries()
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref().map(str::to_string))
            .collect();
        let transport =
            IrohTransport::bind(&device, &home_relays, zink_protocol::MAX_SYNC_REQUEST_BYTES)
                .await?;
        Ok(Self::assemble(
            device, state, config, clock, wall_clock, transport, SystemRng,
        ))
    }

    /// This client's peer dial string `<endpoint-id>@<ip:port>` — how another
    /// device reaches us on `SYNC_ALPN` to backfill history when it knows
    /// our address explicitly (same-LAN / dev tooling). The deployment path
    /// is dial-by-key via our home relay (`backfill_by_key`, D0b).
    pub fn sync_address(&self) -> Result<String, Error> {
        self.transport.sync_address()
    }
}

/// The test constructor keeps the production rng: nothing scripts jitter
/// yet — a draw-injecting sibling appears when a test first drives it.
#[cfg(test)]
impl<C: Clock, W: WallClock, N: Transport> Client<C, W, N> {
    /// A client on injected doubles: no endpoint, no I/O — the network is
    /// whatever the test scripts.
    fn with_transport(
        device: DeviceKey,
        key_path: &str,
        config: ClientConfig,
        clock: C,
        wall_clock: W,
        transport: N,
    ) -> Self {
        Self::assemble(
            device,
            ClientState::open(key_path),
            config,
            clock,
            wall_clock,
            transport,
            SystemRng,
        )
    }
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Wire a client around an already-built transport — shared by
    /// `with_device` (real iroh) and the test constructor (doubles).
    fn assemble(
        device: DeviceKey,
        state: ClientState,
        config: ClientConfig,
        clock: C,
        wall_clock: W,
        transport: N,
        rng: R,
    ) -> Self {
        // Serve peer history sync on our own transport (D0): contacts-only
        // gate (D0c); serves fresh self-records for `who-is-this` (D1a), so
        // the handler needs signing — its own key instance, rebuilt from the
        // seed, since `DeviceKey` is deliberately not `Clone`.
        let direct_sink = crate::sync::DirectSink::default();
        // Negative reach evidence survives the process (De6b): without it a
        // fresh process re-pays the dial deadline for every peer that is
        // simply offline, which the app does on every start and the one-shot
        // CLI on every command.
        let reach = ReachLedger::restore(state.unreachable(), wall_clock.now_ms());
        let handler = crate::sync::SyncHandler::new(
            state.clone(),
            DeviceKey::from_seed(device.seed()),
            direct_sink.clone(),
            reach.clone(),
        );
        let serve_task = n0_future::task::AbortOnDropHandle::new(n0_future::task::spawn(
            crate::sync::serve(transport.clone(), handler),
        ));
        Self {
            device,
            transport,
            state,
            config,
            asked: who_is::AskedOnce::default(),
            _serve_task: serve_task,
            direct_sink,
            reach,
            clock,
            wall_clock,
            rng,
        }
    }

    /// Register the edge's sink for **directly delivered** messages (D5):
    /// messages a peer handed us over the sync ALPN, with no mailbox and so
    /// no nudge to drain. It is the direct-path sibling of `subscribe`'s
    /// `on_new` — notify, re-render — and fires on the router's task, so it
    /// must not block; edges that need async work (see `after_direct`)
    /// spawn it. First registration wins; later ones are ignored.
    ///
    /// Without a sink, direct arrivals are still stored and verified — only
    /// the *live* surfacing is missed, and the next drain/render shows them.
    pub fn on_direct_delivery(&self, sink: impl Fn(Vec<Received>) + Send + Sync + 'static) {
        if self.direct_sink.set(Box::new(sink)).is_err() {
            tracing::warn!("direct-delivery sink already registered; ignoring");
        }
    }

    /// Graceful shutdown for short-lived edges (the CLI): since the endpoint
    /// homes to a relay (D0b) it holds a live transport, and dropping that
    /// without closing makes iroh log an ungraceful-abort error on every
    /// one-shot command. Long-lived edges (the app) never call this.
    ///
    /// **Bounded** by `ClientConfig::close_deadline` (D5): after a direct dial
    /// that got nowhere, iroh spends ~3 s draining the relay-path machinery
    /// that dial started, and a one-shot edge pays that per command. Graceful
    /// is a courtesy to the log, not a correctness requirement — past the
    /// deadline the endpoint is dropped and iroh's abort warning accepted.
    pub async fn close(self) {
        let deadline = self.config.close_deadline;
        if self
            .clock
            .timeout(deadline, self.transport.close())
            .await
            .is_err()
        {
            tracing::debug!("close: endpoint still draining at the deadline; dropping it");
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.device.public()
    }

    /// Wait until other devices can reach us **by key** (De6c), bounded by
    /// `within`.
    ///
    /// Binding an endpoint is not being reachable: dial-by-key routes through
    /// a home relay (D0b), so until one of ours is connected, a peer holding
    /// our key and relay URL cannot reach us — who-is, backfill and direct
    /// delivery against this device all fail, silently, for about a second
    /// after start (measured ~991 ms; fast-failure.md F4). Nothing used to
    /// mark that transition, so edges announced themselves ready too early
    /// and tests papered over the gap by polling.
    ///
    /// Bounded and three-way on purpose. `Endpoint::online()` resolves when
    /// *any* home relay connects and **never resolves at all** when none is
    /// configured, so awaiting it bare would hang a profile-less client
    /// forever. `NoHomeRelay` is that case reported honestly rather than
    /// waited out — such a device is still directly dialable, just not by
    /// key.
    ///
    /// Advisory, not a gate: nothing here blocks sending or draining, both of
    /// which dial the relay's mailbox directly and need no homing.
    pub async fn await_reachable(&self, within: Duration) -> Reachable {
        if self
            .state
            .home_relay_entries()
            .iter()
            .all(|entry| entry.relay_url.is_none())
        {
            return Reachable::NoHomeRelay;
        }
        match self.clock.timeout(within, self.transport.online()).await {
            Ok(()) => Reachable::ByKey,
            Err(_) => Reachable::NotYet,
        }
    }

    /// Fetch + verify + decrypt one blob referenced by a received message:
    /// the local cache first, then the relay it arrived through (caching
    /// the ciphertext for the next time). A **direct** arrival (D5) has no
    /// relay on its path, so its blobs resolve through our own home relays —
    /// the same source `fetch_stored_blob` uses, and where the sender pushed
    /// them.
    pub async fn fetch_blob(&self, received: &Received, hash: &BlobHash) -> Result<Vec<u8>, Error> {
        let relays = match &received.relay {
            Some(relay) => std::slice::from_ref(relay).to_vec(),
            None => self.state.home_relays(),
        };
        self.open_cached_or_fetch(&received.envelope, hash, &relays)
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
    ) -> Result<Vec<u8>, Error> {
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
    ) -> Result<Vec<u8>, Error> {
        if let Some(bytes) = self.state.load_blob(hash)
            && let Ok(plaintext) = envelope.open_blob(&self.device, hash, &bytes)
        {
            return Ok(plaintext);
        }
        let mut last_error = String::from("no relay to fetch from");
        for relay in relays {
            match blobs::fetch_encrypted(
                &self.transport,
                relay,
                hash,
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            {
                Ok(bytes) => {
                    let plaintext = envelope
                        .open_blob(&self.device, hash, &bytes)
                        .map_err(Error::Open)?;
                    self.state.save_blob(hash, &bytes)?;
                    return Ok(plaintext);
                }
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(Error::BlobUnavailable(last_error))
    }

    /// A conversation's current membership (groups.md §2): the union over
    /// the DAG heads of each head's `recipients` ∪ `sender` — membership
    /// is a lens on the DAG, never an object. Adding someone = a message
    /// that includes them (the next heads carry them); stop-including
    /// shrinks it; concurrent heads union — honest over-inclusion that
    /// converges when the fork merges.
    pub fn membership(&self, conversation: MessageId) -> Result<BTreeSet<PublicKey>, Error> {
        Ok(self.membership_of(conversation, &self.state.load_envelopes(conversation)?))
    }

    /// `membership` over already-loaded envelopes. When the DAG can't
    /// build (missing genesis — the pre-heal window), falls back to the
    /// union over every stored message: best-effort, converges with sync.
    fn membership_of(
        &self,
        conversation: MessageId,
        envelopes: &[MessageEnvelope],
    ) -> BTreeSet<PublicKey> {
        let heads = ClientState::dag_of(envelopes, conversation)
            .ok()
            .map(|dag| dag.heads());
        envelopes
            .iter()
            .filter(|envelope| {
                heads
                    .as_ref()
                    .is_none_or(|heads| heads.contains(&envelope.id()))
            })
            .flat_map(participants_of)
            .collect()
    }

    /// Every stored conversation, newest first (by wall-clock hint — a
    /// display ordering, like everything timestamp-based).
    pub fn conversations(&self) -> Result<Vec<ConversationSummary>, Error> {
        // Loaded once for the whole pass, not once per conversation.
        let contacts = self.state.contacts()?;
        let own = self.own_keys();
        let mut summaries = Vec::new();
        for id in self.state.conversations() {
            let envelopes = self.state.load_envelopes(id)?;
            if envelopes.is_empty() {
                continue;
            }
            let participants = self.membership_of(id, &envelopes);
            summaries.push(ConversationSummary {
                id,
                participants: participants.into_iter().collect(),
                message_count: envelopes.len(),
                last_timestamp_ms: envelopes
                    .iter()
                    .map(|envelope| envelope.core.timestamp_ms)
                    .max()
                    .unwrap_or(0),
                // Computed from the envelopes already in hand — asking
                // `has_contributing_contact` here would re-read the whole
                // conversation from disk, once per conversation, per render.
                known: self.contributed_to(&envelopes, &contacts, &own),
                first_seen_ms: self.state.first_seen_ms(id),
            });
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_timestamp_ms));
        Ok(summaries)
    }

    /// Display labels for a participant set, deduped per *person*
    /// (multi-device.md §7): keys held by one contact entry collapse to a
    /// single petname — a two-device contact renders once in conversation
    /// labels. A recognized own device labels with its self-claimed name
    /// (D3c); unknown keys stay distinct, as honest short hex. Order
    /// follows the input; a cluster's label sits at its first key.
    pub fn participant_labels(&self, keys: &[PublicKey]) -> Result<Vec<String>, Error> {
        let contacts = self.state.contacts()?;
        let devices = self.state.recognized_devices();
        let mut labels = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for key in keys {
            let label = contacts
                .iter()
                .find(|(_, record)| record.keys.contains(key))
                .map(|(petname, _)| petname.clone())
                .or_else(|| {
                    devices
                        .iter()
                        .find(|(device_key, _)| device_key == key)
                        .map(|(_, record)| {
                            record
                                .self_claimed_name()
                                .map(str::to_string)
                                .unwrap_or_else(|| hex::encode(&key.0[..4]))
                        })
                });
            match label {
                Some(label) => {
                    if seen.insert(label.clone()) {
                        labels.push(label);
                    }
                }
                None => labels.push(hex::encode(&key.0[..4])),
            }
        }
        Ok(labels)
    }

    /// One conversation's stored messages in the DAG's linearized order.
    /// Bodies are opened per message and never fail the whole history — an
    /// envelope this device cannot open (e.g. sealed before the self-wrap
    /// convention) surfaces as `Err`, honestly, like `Received` does.
    pub fn history(&self, conversation: MessageId) -> Result<Vec<HistoryMessage>, Error> {
        let envelopes = self.state.load_envelopes(conversation)?;
        let by_id: BTreeMap<MessageId, &MessageEnvelope> = envelopes
            .iter()
            .map(|envelope| (envelope.id(), envelope))
            .collect();
        let dag = ClientState::dag_of(&envelopes, conversation)?;
        let pending = self.state.pending_messages();
        let confirmed = self.state.acks_in(conversation);
        let crossed = dag.crossed_in_flight();
        Ok(dag
            .linearize()
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|envelope| {
                let id = envelope.id();
                let (joined, left) = membership_delta(envelope, &by_id);
                HistoryMessage {
                    id,
                    sender: envelope.core.sender,
                    timestamp_ms: envelope.core.timestamp_ms,
                    body: envelope.open(&self.device),
                    blob_refs: envelope.core.blob_refs.clone(),
                    pending: pending.contains(&id),
                    joined,
                    left,
                    crossed: crossed.contains(&id),
                    merged: envelope.core.parents.len() > 1,
                    confirmed: confirmed.get(&id).cloned().unwrap_or_default(),
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
    pub async fn set_profile(&self, name: &str, relays: &[String]) -> Result<(), Error> {
        if name.trim().is_empty() {
            return Err(Error::ProfileIncomplete("name must not be empty"));
        }
        let entries: Vec<RelayEntry> = relays.iter().map(|s| RelayEntry::from_spec(s)).collect();
        for entry in &entries {
            crate::adapters::iroh::parse_dial(&entry.mailbox)?;
            if let Some(url) = &entry.relay_url {
                crate::adapters::iroh::parse_relay_url(url)?;
            }
        }
        // A rename supersedes the previous name attestation (SPEC §3.2):
        // bump the persisted revision so receivers holding both claims have
        // a winner. Only *name* changes bump — the counter is scoped per
        // claim-kind; relay changes order by receipt time instead (D1b).
        if let Some(previous) = self.state.profile_name()
            && previous != name.trim()
        {
            self.state
                .save_profile_revision(self.state.profile_revision() + 1)?;
        }
        let previous = self.home_relay_urls()?;
        self.state.save_profile(name.trim(), &entries)?;
        // Home the RUNNING endpoint (De5): the relay transport is always
        // bound (net::bind_endpoint), so map changes apply immediately —
        // a profile save no longer needs a restart to take effect.
        let next = self.home_relay_urls()?;
        for url in next.iter().filter(|url| !previous.contains(url)) {
            self.transport
                .insert_relay(url)
                .await
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
        }
        for url in previous.iter().filter(|url| !next.contains(url)) {
            self.transport.remove_relay(url).await;
        }
        Ok(())
    }

    /// The profile's home-relay URLs (entries without one skipped),
    /// normalized through the parser so `set_profile`'s diff compares the
    /// way the endpoint's relay map does — not raw string spellings.
    fn home_relay_urls(&self) -> Result<Vec<String>, Error> {
        self.state
            .home_relay_entries()
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref())
            .map(|url| crate::adapters::iroh::parse_relay_url(url).map(|url| url.to_string()))
            .collect()
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
    pub fn my_record(&self) -> Result<ContactRecord, Error> {
        if self.state.profile_name().is_none() {
            return Err(Error::ProfileIncomplete("set a profile name first"));
        }
        build_own_record(&self.device, &self.state)
            .ok_or(Error::ProfileIncomplete("set a home relay first"))
    }

    /// Ensure a mailbox exists on every home relay. Called when publishing
    /// a record: anyone who scans it must be able to deposit immediately —
    /// a record that names a relay where you have no mailbox is a lie.
    pub async fn register_at_home_relays(&self) -> Result<(), Error> {
        // Concurrent (De6d), but still **all-or-error**: publishing a record
        // that names a relay where we have no mailbox is a lie, so any
        // failure is reported. What changed is the price of learning that —
        // one deadline for n relays instead of n, and every reachable relay
        // gets its mailbox even when a sibling is down (serially, a dead
        // *first* relay meant the later ones were never even tried).
        let registrations = self
            .state
            .home_relays()
            .into_iter()
            .map(|relay| async move {
                let connection = net::connect(
                    &self.transport,
                    &relay,
                    zink_protocol::MAILBOX_ALPN,
                    self.config.connect_timeout,
                    &self.clock,
                )
                .await?;
                net::register(&connection, &relay).await?;
                Ok(())
            });
        n0_future::join_all(registrations)
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Error>>()?;
        Ok(())
    }

    /// Set this device's avatar (D1d, who-is-this.md §8): encrypt once
    /// with a fresh key, cache the ciphertext locally (rendering our own
    /// avatar must survive relay TTLs), persist the claim materials at the
    /// next supersession revision, and push the ciphertext to the home
    /// relays. The image should arrive edge-downscaled; the size cap here
    /// is a backstop, not the policy. Republish the record (QR /
    /// `who-is`) for contacts to pick the new claim up.
    pub async fn set_avatar(&self, image: Vec<u8>) -> Result<AvatarReceipt, Error> {
        const MAX_AVATAR_BYTES: usize = 512 * 1024;
        if image.is_empty() {
            return Err(Error::InvalidInput("empty avatar image".into()));
        }
        if image.len() > MAX_AVATAR_BYTES {
            return Err(Error::InvalidInput(format!(
                "avatar too large ({} bytes; max {MAX_AVATAR_BYTES})",
                image.len()
            )));
        }
        let (blob, key) = seal_avatar(&image, &mut OsRng);
        self.state.save_blob(&blob.hash, &blob.bytes)?;
        let revision = self
            .state
            .avatar_meta()
            .map(|(_, _, revision)| revision + 1)
            .unwrap_or(0);
        self.state.save_avatar_meta(&blob.hash, &key, revision)?;
        Ok(AvatarReceipt {
            hash: blob.hash,
            revision,
            pushed_relays: self.push_avatar().await,
        })
    }

    /// Push the current avatar ciphertext to every home relay (relays
    /// dedup by hash) — run at publish, and re-run by long-lived edges on
    /// startup: relay caches expire (30-day TTL), and the publisher's push
    /// is the only source contacts can fetch from. Best-effort per relay;
    /// returns how many took it.
    pub async fn push_avatar(&self) -> usize {
        let Some((hash, _, _)) = self.state.avatar_meta() else {
            return 0;
        };
        let Some(bytes) = self.state.load_blob(&hash) else {
            return 0;
        };
        let blob = EncryptedBlob { hash, bytes };
        let mut pushed = 0;
        for relay in self.state.home_relays() {
            match blobs::push_blobs(
                &self.transport,
                &relay,
                std::slice::from_ref(&blob),
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            {
                Ok(()) => pushed += 1,
                Err(error) => tracing::warn!(relay, %error, "avatar push failed"),
            }
        }
        pushed
    }

    /// Set a local avatar override for a contact (U6, my lens): a photo *I*
    /// chose, stored plaintext on this device only — never published, never a
    /// claim. Wins over the resolved self-claim in `avatar`.
    pub fn set_local_avatar(&self, key: PublicKey, image: Vec<u8>) -> Result<(), Error> {
        if image.len() > 512 * 1024 {
            return Err(Error::InvalidInput("image too large (max 512 KiB)".into()));
        }
        self.state.save_local_avatar(&key, &image)
    }

    /// Drop the local avatar override — `avatar` falls back to the self-claim.
    pub fn clear_local_avatar(&self, key: PublicKey) {
        self.state.remove_local_avatar(&key);
    }

    /// Whether a local avatar override is set for a key (drives the "remove
    /// your photo" affordance).
    pub fn has_local_avatar(&self, key: &PublicKey) -> bool {
        self.state.local_avatar(key).is_some()
    }

    /// The best-believed avatar for a key (D1d): the highest-revision
    /// verified self-issued `Avatar` claim across the stored record and
    /// every learned record; ciphertext from the local cache, else fetched
    /// from the relays of the record that carried the winning claim
    /// (that's where its owner pushes), verified against the claim (hash +
    /// AEAD) and cached. `Ok(None)` for no claim *and* for a claim whose
    /// blob is currently unfetchable — display data is best-effort.
    pub async fn avatar(&self, subject: PublicKey) -> Result<Option<Vec<u8>>, Error> {
        // A local override (U6, my lens) wins over any claim — a photo I
        // chose for them, stored on this device only, never fetched.
        if let Some(bytes) = self.state.local_avatar(&subject) {
            return Ok(Some(bytes));
        }
        if subject == self.device.public() {
            let Some((hash, key, _)) = self.state.avatar_meta() else {
                return Ok(None);
            };
            let Some(bytes) = self.state.load_blob(&hash) else {
                return Ok(None);
            };
            return Ok(Some(open_avatar(&bytes, &hash, &key).map_err(Error::Open)?));
        }
        let mut best: Option<(BlobHash, [u8; 32], u64, Vec<RelayEntry>)> = None;
        let mut consider = |record: &ContactRecord| {
            if let Some((hash, key, revision)) = record.self_avatar_claim()
                && best.as_ref().is_none_or(|(_, _, held, _)| revision > *held)
            {
                best = Some((hash, key, revision, record.relays.clone()));
            }
        };
        for (_, record) in self.state.contacts()? {
            if record.keys.contains(&subject) {
                consider(&record);
            }
        }
        for learned in self.state.learned(&subject) {
            consider(&learned.record);
        }
        let Some((hash, key, _, relays)) = best else {
            return Ok(None);
        };
        if let Some(bytes) = self.state.load_blob(&hash)
            && let Ok(plaintext) = open_avatar(&bytes, &hash, &key)
        {
            return Ok(Some(plaintext));
        }
        for relay in relays {
            match blobs::fetch_encrypted(
                &self.transport,
                &relay.mailbox,
                &hash,
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            {
                Ok(bytes) => match open_avatar(&bytes, &hash, &key) {
                    Ok(plaintext) => {
                        self.state.save_blob(&hash, &bytes)?;
                        return Ok(Some(plaintext));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "served avatar failed verification; skipping")
                    }
                },
                Err(error) => {
                    tracing::debug!(relay = relay.mailbox, %error, "avatar fetch failed")
                }
            }
        }
        Ok(None)
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
    pub async fn backfill(&self, conversation: MessageId, from: &str) -> Result<usize, Error> {
        self.backfill_addr(conversation, crate::adapters::iroh::parse_dial(from)?)
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
    ) -> Result<usize, Error> {
        // A contact's record, or a recognized own device's (D3c/D3d) — a
        // fresh paired device backfills its sibling by key.
        let record = self
            .trusted_record_for(&peer)
            .ok_or_else(|| Error::NotAContact("no stored record for that key".into()))?;
        self.backfill_addr(conversation, self.peer_addr_for(peer, Some(&record))?)
            .await
    }

    /// `backfill` with the peer address already resolved — the seam the string
    /// API parses into, and the one tests use to dial a locally-bound peer's
    /// full multi-address `EndpointAddr` (a bare public socket isn't reliably
    /// self-reachable on one host).
    async fn backfill_addr(&self, conversation: MessageId, from: Peer) -> Result<usize, Error> {
        // A hostile peer could feed an unbounded fake chain; one budget
        // bounds the whole walk — the forward pass gets what the backward
        // pass didn't spend.
        const MAX_SYNC_FETCH: usize = 10_000;
        let connection = net::connect_peer(
            &self.transport,
            &from,
            SYNC_ALPN,
            self.config.connect_timeout,
            &self.clock,
        )
        .await?;
        let backward = self
            .fill_backward(conversation, &connection, MAX_SYNC_FETCH)
            .await?;
        let forward = self
            .fill_forward(conversation, &connection, MAX_SYNC_FETCH - backward)
            .await?;
        Ok(backward + forward)
    }

    /// The backward pass: fetch referenced-but-missing parents until the
    /// stored slice is ancestor-closed (genesis reached), the peer stops
    /// yielding, or `budget` is spent. Returns the number fetched.
    async fn fill_backward(
        &self,
        conversation: MessageId,
        connection: &impl Request,
        budget: usize,
    ) -> Result<usize, Error> {
        let mut fetched = 0usize;
        loop {
            let frontier = self.state.missing_ancestors(conversation);
            if frontier.is_empty() {
                break; // ancestor-closed: the genesis (parents=[]) is reachable
            }
            let mut progressed = false;
            for id in frontier {
                if fetched >= budget {
                    tracing::warn!("sync hit the fetch budget; stopping");
                    return Ok(fetched);
                }
                if self.fetch_one(connection, id, conversation).await? {
                    fetched += 1;
                    progressed = true;
                }
            }
            if !progressed {
                break; // this peer can't take us any closer to the genesis
            }
        }
        Ok(fetched)
    }

    /// The forward pass (D0d): `get-successors` to learn children we lack —
    /// messages the mailbox never delivered (expired, or sent while we were
    /// unreachable) and concurrent branches. The first round queries every
    /// stored id (a fork can hang off any interior message); later rounds
    /// query only what the previous round fetched, so the walk converges.
    /// Chatty at one round-trip per id — fine at friend/family scale.
    /// Returns the number fetched, at most `budget`.
    async fn fill_forward(
        &self,
        conversation: MessageId,
        connection: &impl Request,
        budget: usize,
    ) -> Result<usize, Error> {
        let mut fetched = 0usize;
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
                    other => return Err(Error::UnexpectedResponse(format!("sync: {other:?}"))),
                };
                for child in ids {
                    if fetched >= budget {
                        tracing::warn!("sync hit the fetch budget; stopping");
                        return Ok(fetched);
                    }
                    if stored.contains(&child) {
                        continue;
                    }
                    if self.fetch_one(connection, child, conversation).await? {
                        stored.insert(child);
                        learned.push(child);
                        fetched += 1;
                    }
                }
            }
            query = learned;
        }
        Ok(fetched)
    }

    /// One `get` round-trip: fetch `id`, validate, store. `Ok(true)` iff a
    /// new envelope was stored. A served peer is trusted no more than a
    /// relay: the envelope must hash to the id we asked for, carry a valid
    /// sender signature, and belong to the conversation being synced — the
    /// last check matters for the forward pass, where ids are the *peer's
    /// claim* rather than parents read from envelopes we already verified.
    async fn fetch_one(
        &self,
        connection: &impl Request,
        id: MessageId,
        conversation: MessageId,
    ) -> Result<bool, Error> {
        match net::sync_request(connection, SyncOp::Get { id }).await? {
            SyncResult::Envelope { envelope } => {
                if envelope.id() != id {
                    tracing::warn!("peer returned a mismatched id; skipping");
                    return Ok(false);
                }
                if !envelope.version_supported() {
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
            other => Err(Error::UnexpectedResponse(format!("sync: {other:?}"))),
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

    /// Heal unopenable history from paired devices (D3d, multi-device.md
    /// §6): after a skeleton sync leaves envelopes stored but unopenable,
    /// batch their ids to each recognized device and append the verified
    /// wraps to the stored envelopes. Ids never move — wraps live outside
    /// the hashed core; storage just rewrites the file. Best-effort like
    /// every peer op; returns how many messages became readable.
    pub async fn rewrap_backlog(&self) -> usize {
        let mut healed = 0;
        for conversation in self.state.conversations() {
            healed += self.rewrap_conversation(conversation).await;
        }
        healed
    }

    async fn rewrap_conversation(&self, conversation: MessageId) -> usize {
        let me = self.device.public();
        let Ok(envelopes) = self.state.load_envelopes(conversation) else {
            return 0;
        };
        let mut unopenable: BTreeMap<MessageId, MessageEnvelope> = envelopes
            .into_iter()
            .filter(|envelope| !envelope.key_wraps.iter().any(|wrap| wrap.recipient == me))
            .map(|envelope| (envelope.id(), envelope))
            .collect();
        if unopenable.is_empty() {
            return 0;
        }
        let mut healed = 0;
        for (device_key, record) in self.state.recognized_devices() {
            if unopenable.is_empty() {
                break;
            }
            let Ok(addr) = self.peer_addr_for(device_key, Some(&record)) else {
                continue;
            };
            let Ok(connection) = net::connect_peer(
                &self.transport,
                &addr,
                SYNC_ALPN,
                self.config.connect_timeout,
                &self.clock,
            )
            .await
            else {
                continue;
            };
            let missing: Vec<MessageId> = unopenable.keys().copied().collect();
            for chunk in missing.chunks(MAX_GET_KEYS_IDS) {
                let Ok(SyncResult::Wraps { wraps }) = net::sync_request(
                    &connection,
                    SyncOp::GetKeys {
                        ids: chunk.to_vec(),
                    },
                )
                .await
                else {
                    break; // declined or failed — try the next device
                };
                for (id, wrap) in wraps {
                    let Some(envelope) = unopenable.get(&id) else {
                        continue;
                    };
                    // Verify before trusting: the wrap is ours and the
                    // body opens under it (the commitment check inside
                    // `open` rejects a wrong key). A bad wrap is dropped
                    // with a warning, never stored.
                    if wrap.recipient != me {
                        tracing::warn!("re-wrap for a different key; dropped");
                        continue;
                    }
                    let mut updated = envelope.clone();
                    updated.key_wraps.push(wrap);
                    if updated.open(&self.device).is_err() {
                        tracing::warn!("re-wrap does not open the body; dropped");
                        continue;
                    }
                    if self.state.store_envelope(conversation, &updated).is_err() {
                        continue;
                    }
                    unopenable.remove(&id);
                    healed += 1;
                }
            }
        }
        healed
    }

    /// The opportunistic re-wrap (D3d): after a drain — whose auto-sync
    /// may just have pulled pre-pairing history — heal the touched
    /// conversations. Free for single-device clients: no recognized
    /// devices, no scan.
    async fn auto_rewrap(&self, received: &[Received]) {
        if self.state.recognized_devices().is_empty() {
            return;
        }
        let conversations: BTreeSet<MessageId> = received
            .iter()
            .map(|message| {
                message
                    .envelope
                    .core
                    .conversation
                    .unwrap_or_else(|| message.envelope.id())
            })
            .collect();
        for conversation in conversations {
            let healed = self.rewrap_conversation(conversation).await;
            if healed > 0 {
                tracing::info!(healed, "re-wrapped history from a paired device");
            }
        }
    }

    /// Persist a verified envelope and its participant→conversation mapping,
    /// so a later `send` to the same people threads into this conversation.
    fn remember(&self, envelope: &MessageEnvelope) -> Result<(), Error> {
        remember(&self.state, envelope)
    }
}

/// `Client::remember` over bare state — the serving router (D5 `Deliver`)
/// stores exactly what a drain stores, and holds state but no `Client`.
pub(crate) fn remember(state: &ClientState, envelope: &MessageEnvelope) -> Result<(), Error> {
    let conversation = envelope.core.conversation.unwrap_or_else(|| envelope.id());
    state.store_envelope(conversation, envelope)?;
    let participants: BTreeSet<PublicKey> = envelope
        .core
        .recipients
        .iter()
        .copied()
        .chain([envelope.core.sender])
        .collect();
    state.record_conversation(&participants, conversation)
}

/// The self-record — key, self-attested name, home relays — or `None`
/// until the profile is complete (both parts). Shared by `my_record` (the
/// QR/paste publishing path) and the sync handler (serving `WhoIs` about
/// our own key, D1a), so the two can't drift. The attestation `revision`
/// is the persisted supersession counter, bumped per rename (D1b).
pub(crate) fn build_own_record(device: &DeviceKey, state: &ClientState) -> Option<ContactRecord> {
    let name = state.profile_name()?;
    let relays = state.home_relay_entries();
    if relays.is_empty() {
        return None;
    }
    let me = device.public();
    let self_claim = |claim: Claim, revision: u64| {
        SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: me,
                subject: me,
                claim,
                revision,
            },
            device,
        )
    };
    let mut attestations = vec![self_claim(Claim::Name(name), state.profile_revision())];
    // The avatar claim (D1d): hash + key together, under the signature —
    // whoever holds the record can fetch and decrypt; relays cannot.
    if let Some((hash, key, revision)) = state.avatar_meta() {
        attestations.push(self_claim(Claim::Avatar { hash, key }, revision));
    }
    // The outgoing device vouches (D3b, multi-device.md §4): the record
    // gains exactly this — links live in the record's attestations
    // (SPEC §3.6). `keys` stays this device's own key; observers gather
    // link evidence across the records they hold.
    attestations.extend(state.device_vouches());
    // …and the issued repudiations (D4b, web-of-trust.md §5): a lost key's
    // disavowal reaches contacts through any freshness pull on US — the
    // endorsement channel needs a servable record for the *subject*, which
    // an un-recognized key no longer has.
    attestations.extend(state.issued_negatives());
    Some(ContactRecord::new(vec![me], attestations, relays))
}

/// Whether other devices can reach this one by key (De6c) — the answer from
/// `Client::await_reachable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachable {
    /// A home relay connection is live: a peer holding our key and relay URL
    /// can dial us now.
    ByKey,
    /// The profile names no relay URL, so there is nothing to wait for —
    /// dial-by-key cannot work at all. Still directly dialable at an explicit
    /// address, which is how the dev tooling and same-LAN paths work.
    NoHomeRelay,
    /// Still not connected when the deadline passed. Not fatal — homing keeps
    /// retrying underneath — but honest: right now, we are not reachable.
    NotYet,
}

/// What `set_avatar` accomplished (D1d).
pub struct AvatarReceipt {
    /// The ciphertext's content address — what relays cache and serve.
    pub hash: BlobHash,
    /// The claim's supersession counter (bumped per avatar change).
    pub revision: u64,
    /// Home relays that took the push just now. 0 = fetchable by no one
    /// until a later `push_avatar` succeeds — set, but not yet published.
    pub pushed_relays: usize,
}

/// One stored conversation, as the edge lists it. Participants are keys —
/// naming them is the edge's policy (petnames, hex, whatever).
pub struct ConversationSummary {
    pub id: MessageId,
    /// The current membership — heads-based (groups.md §2), sorted;
    /// includes this device. Union over all messages when the DAG can't
    /// build yet (pre-heal partial view).
    pub participants: Vec<PublicKey>,
    pub message_count: usize,
    /// Largest wall-clock hint seen — display ordering only, never trusted.
    pub last_timestamp_ms: u64,
    /// The contributing-contact rule (groups.md §6): a contact — or one of
    /// our own devices — has **authored** a message we hold here. Presence
    /// in `recipients` deliberately does not count: a spammer can list your
    /// friends for free, authorship they cannot forge.
    ///
    /// False means "no contact has spoken here *yet*", not "spam": triage is
    /// at presentation only, so a contact's message arriving later promotes
    /// the conversation with no migration and nothing lost.
    pub known: bool,
    /// When this device first stored anything here — our clock, not the
    /// sender's. What the requests queue orders and evicts by.
    pub first_seen_ms: u64,
}

/// A conversation list split by the contributing-contact rule (groups.md §6).
pub struct Inbox {
    /// Conversations a contact has contributed to — the main list.
    pub conversations: Vec<ConversationSummary>,
    /// Requests from senders nobody you know has vouched for by speaking,
    /// newest-first and capped at [`MAX_MESSAGE_REQUESTS`].
    pub requests: Vec<ConversationSummary>,
    /// Requests beyond the cap. Surfaced rather than silently swallowed —
    /// a view that quietly hides mail is the failure this one exists to
    /// prevent.
    pub dropped: usize,
}

/// How many pending requests the quarantine view holds. Anyone with your
/// record can deposit (mutuality is not required — that is what makes
/// one-way adds work), so without a cap a flood of strangers makes the
/// requests view as unusable as the main list it protects.
pub const MAX_MESSAGE_REQUESTS: usize = 32;

/// Split a conversation list into the main inbox and the bounded requests
/// queue (groups.md §6, the parked unknown-sender quarantine).
///
/// **Pure, and deliberately here rather than in the edges.** Both the CLI
/// and the app need exactly this split; implemented twice it would drift,
/// the way "what do I call this key" already has.
///
/// **View-only.** Nothing is deleted and nothing is refused at delivery —
/// groups.md §6 is explicit that messages arrive in any order, so a
/// contact's first contribution may land *after* the stranger's message
/// that opened the conversation. Dropping data at the cap would destroy
/// what a later arrival would have legitimised. Bounding client *storage*
/// is a separate question, and belongs to the deferred ephemerality work.
pub fn triage(summaries: Vec<ConversationSummary>) -> Inbox {
    let (conversations, mut requests): (Vec<_>, Vec<_>) =
        summaries.into_iter().partition(|summary| summary.known);
    // Newest first by *our* clock, so the queue cannot be steered by a
    // sender's chosen timestamp (see `ClientState::first_seen_ms`).
    requests.sort_by_key(|summary| std::cmp::Reverse(summary.first_seen_ms));
    let dropped = requests.len().saturating_sub(MAX_MESSAGE_REQUESTS);
    requests.truncate(MAX_MESSAGE_REQUESTS);
    Inbox {
        conversations,
        requests,
        dropped,
    }
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
    /// Membership delta vs this message's parents (groups.md §2): keys
    /// this message added to / dropped from the addressed set — derived
    /// from the signed cores, not a message type. Empty for the genesis
    /// and when no parent is held (partial view).
    pub joined: Vec<PublicKey>,
    pub left: Vec<PublicKey>,
    /// Causally incomparable with the message rendered just above it —
    /// they crossed in flight (D4d, tenet 7). The order is unchanged.
    pub crossed: bool,
    /// Merges concurrent branches (more than one parent).
    pub merged: bool,
    /// Recipient devices that confirmed a durable store of this message
    /// (De7) — D5's `Stored` ack, attributable to the **recipient's own
    /// device key**, not to a relay. Only ever non-empty for our own sends.
    ///
    /// **Positive-only** (tenet 7, honesty over false order): a key here
    /// has confirmed; absence means *no confirmation was received*, never
    /// "not delivered" — the recipient may well hold it via the mailbox and
    /// simply have no way to say so. `pending` remains the only negative
    /// signal, and it means "we still owe a relay".
    pub confirmed: Vec<PublicKey>,
}

/// One message's participant set: `recipients` ∪ `sender` (signed core).
fn participants_of(envelope: &MessageEnvelope) -> impl Iterator<Item = PublicKey> + '_ {
    envelope
        .core
        .recipients
        .iter()
        .copied()
        .chain([envelope.core.sender])
}

/// This message's membership delta vs its parents — `(joined, left)`,
/// derived from signed cores (groups.md §2), never a message type. Empty
/// for the genesis and when no parent is held (partial view — honest
/// silence over guessing).
fn membership_delta(
    envelope: &MessageEnvelope,
    by_id: &BTreeMap<MessageId, &MessageEnvelope>,
) -> (Vec<PublicKey>, Vec<PublicKey>) {
    let held_parents: Vec<&MessageEnvelope> = envelope
        .core
        .parents
        .iter()
        .filter_map(|parent| by_id.get(parent).copied())
        .collect();
    if held_parents.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let before: BTreeSet<PublicKey> = held_parents
        .iter()
        .copied()
        .flat_map(participants_of)
        .collect();
    let now: BTreeSet<PublicKey> = participants_of(envelope).collect();
    (
        now.difference(&before).copied().collect(),
        before.difference(&now).copied().collect(),
    )
}

#[cfg(test)]
mod test_kit;

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::test_kit::{
        befriend, chain, deposited_envelopes, deposited_frame, loop_client, mailbox_only,
        mailbox_spec, message, open_homed, routed_record, script_drain, sealed_chain,
        signed_record, spawn_test_relay, summary, temp_key, temp_root,
    };
    use super::*;
    use crate::ports::clock::TestClock;
    use crate::ports::transport::{Home, Loopback, TestTransport};

    #[tokio::test]
    async fn membership__should_follow_the_heads_not_the_full_history() {
        // Given: A→{B}; A adds C; A stops including C
        let client = Client::open_or_create(&temp_key("members", "viewer"))
            .await
            .expect("open");
        let a = DeviceKey::from_seed([1; 32]);
        let b = DeviceKey::from_seed([2; 32]);
        let (pa, pb) = (a.public(), b.public());
        let pc = DeviceKey::from_seed([3; 32]).public();
        let genesis = message(&a, vec![pb], None, vec![], 0, 0);
        let conversation = genesis.id();
        let add = message(
            &a,
            vec![pb, pc],
            Some(conversation),
            vec![genesis.id()],
            1,
            1,
        );
        let drop_c = message(&a, vec![pb], Some(conversation), vec![add.id()], 2, 2);
        for envelope in [&genesis, &add, &drop_c] {
            client
                .state
                .store_envelope(conversation, envelope)
                .expect("store");
        }

        // Then: the sole head excludes C — membership shrank (a full-
        // history union could never)
        assert_eq!(
            client.membership(conversation).expect("membership"),
            BTreeSet::from([pa, pb])
        );

        // When: a concurrent head (B replying off the add) still holds C
        let fork = message(&b, vec![pa, pc], Some(conversation), vec![add.id()], 0, 2);
        client
            .state
            .store_envelope(conversation, &fork)
            .expect("store fork");

        // Then: heads union — honest over-inclusion until the fork merges
        assert_eq!(
            client.membership(conversation).expect("membership"),
            BTreeSet::from([pa, pb, pc])
        );

        let _ = std::fs::remove_dir_all(temp_root("members"));
    }

    #[tokio::test]
    async fn history__should_derive_membership_deltas_from_signed_cores() {
        // Given: A→{B}, then C added, then C stop-included
        let client = Client::open_or_create(&temp_key("deltas", "viewer"))
            .await
            .expect("open");
        let a = DeviceKey::from_seed([1; 32]);
        let pb = DeviceKey::from_seed([2; 32]).public();
        let pc = DeviceKey::from_seed([3; 32]).public();
        let genesis = message(&a, vec![pb], None, vec![], 0, 0);
        let conversation = genesis.id();
        let add = message(
            &a,
            vec![pb, pc],
            Some(conversation),
            vec![genesis.id()],
            1,
            1,
        );
        let drop_c = message(&a, vec![pb], Some(conversation), vec![add.id()], 2, 2);
        for envelope in [&genesis, &add, &drop_c] {
            client
                .state
                .store_envelope(conversation, envelope)
                .expect("store");
        }

        // When
        let history = client.history(conversation).expect("history");

        // Then: deltas derive per message — genesis has none, the add
        // joined C, the stop-include left C
        assert!(history[0].joined.is_empty() && history[0].left.is_empty());
        assert_eq!(history[1].joined, vec![pc]);
        assert!(history[1].left.is_empty());
        assert_eq!(history[2].left, vec![pc]);
        assert!(history[2].joined.is_empty());

        let _ = std::fs::remove_dir_all(temp_root("deltas"));
    }

    #[tokio::test]
    async fn backfill__should_walk_a_conversation_back_to_its_genesis() {
        // Given: a 3-message conversation A holds in full, while B — added
        // mid-conversation — holds only the latest message, so it can't even
        // build the DAG (no genesis on disk) and thus can't reply.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("walk", "server", &wire);
        let (b, _b_net, _b_clock) = loop_client("walk", "client", &wire);
        befriend(&a.state, b.public_key()); // the D0c gate serves contacts only

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
            .backfill_addr(
                conversation,
                Peer {
                    key: a.public_key(),
                    relays: vec![],
                    sockets: vec![],
                },
            )
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

    #[tokio::test]
    async fn homed_endpoint__should_report_online_without_waiting_out_probe_timeout() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): real `online()` readiness timing.
        // Given: a home relay serving QAD (De2). Without it, the first
        // net-report waited out iroh's full 3 s probe timeout before the
        // endpoint reported online (measured ~3.03 s of the ~3.15 s
        // relay-based e2e tests).
        let (_relay, url) = spawn_test_relay().await;
        let client = open_homed("qad", "client", &url).await;

        // When
        let started = std::time::Instant::now();
        client.transport.online().await;
        let elapsed = started.elapsed();

        // Then: nowhere near the 3 s probe timeout (bound leaves CI headroom;
        // locally this is well under a second)
        assert!(
            elapsed < Duration::from_secs(2),
            "online took {elapsed:?} — QAD probe likely failing"
        );

        let _ = std::fs::remove_dir_all(temp_root("qad"));
    }

    #[test]
    fn triage__should_split_on_the_contributing_contact_rule() {
        // Given: one conversation a contact has written in, one nobody known has
        let inbox = triage(vec![summary(1, true, 10), summary(2, false, 20)]);

        // Then
        assert_eq!(inbox.conversations.len(), 1);
        assert_eq!(inbox.conversations[0].id, MessageId([1; 32]));
        assert_eq!(inbox.requests.len(), 1);
        assert_eq!(inbox.requests[0].id, MessageId([2; 32]));
        assert_eq!(inbox.dropped, 0);
    }

    #[test]
    fn triage__should_order_requests_by_our_clock_not_the_senders() {
        // Given: three requests seen locally in a known order. Their
        // `last_timestamp_ms` is a sender-chosen hint (SPEC §4.3) and is
        // deliberately left at 0 — nothing here may depend on it.
        let inbox = triage(vec![
            summary(1, false, 100),
            summary(2, false, 300),
            summary(3, false, 200),
        ]);

        // Then: newest-first by first-seen, so a stranger cannot pin itself
        // to the top of the queue by dating a message in the future.
        let order: Vec<u8> = inbox.requests.iter().map(|s| s.id.0[0]).collect();
        assert_eq!(order, vec![2, 3, 1]);
    }

    #[test]
    fn triage__should_cap_the_requests_queue_and_report_what_it_dropped() {
        // Given: more requests than the queue holds, oldest first
        let flood: Vec<ConversationSummary> = (0..MAX_MESSAGE_REQUESTS + 5)
            .map(|i| summary(i as u8, false, i as u64))
            .collect();

        // When
        let inbox = triage(flood);

        // Then: bounded, newest kept, and the overflow is *counted* rather
        // than silently swallowed — a view that quietly hides mail is the
        // failure this one exists to prevent.
        assert_eq!(inbox.requests.len(), MAX_MESSAGE_REQUESTS);
        assert_eq!(inbox.dropped, 5);
        assert_eq!(
            inbox.requests[0].first_seen_ms,
            (MAX_MESSAGE_REQUESTS + 4) as u64,
            "the newest request survives the cap"
        );
    }

    #[tokio::test]
    async fn conversations__should_quarantine_a_stranger_until_a_contact_speaks() {
        // Given: a message from a key we have never heard of
        let client = Client::open_or_create(&temp_key("quarantine", "me"))
            .await
            .expect("open");
        let stranger = DeviceKey::from_seed([81; 32]);
        let me = client.public_key();
        let genesis = message(&stranger, vec![me], None, vec![], 0, 0);
        let conversation = genesis.id();
        client
            .state
            .store_envelope(conversation, &genesis)
            .expect("store");

        // Then: it is a request, not a conversation — presence in
        // `recipients` is attacker-controlled, authorship is not.
        let inbox = triage(client.conversations().expect("conversations"));
        assert!(inbox.conversations.is_empty());
        assert_eq!(inbox.requests.len(), 1);

        // When: we add them as a contact (the promote-out path)
        client
            .add_contact(
                &ContactRecord::new(
                    vec![stranger.public()],
                    vec![],
                    vec![RelayEntry {
                        mailbox: format!("{}@203.0.113.9:1", hex::encode(&stranger.public().0)),
                        relay_url: None,
                    }],
                ),
                Some("stranger".to_string()),
            )
            .expect("add");

        // Then: it promotes with nothing migrated and nothing lost —
        // triage is at presentation only (groups.md §6).
        let inbox = triage(client.conversations().expect("conversations"));
        assert_eq!(inbox.conversations.len(), 1);
        assert!(inbox.requests.is_empty());
        assert_eq!(
            client.history(conversation).expect("history").len(),
            1,
            "the message was never withheld from storage"
        );

        let _ = std::fs::remove_dir_all(temp_root("quarantine"));
    }

    #[tokio::test]
    async fn history__should_report_no_confirmation_rather_than_a_failure() {
        // Given: a recipient nobody can reach — no peer to ack, no mailbox
        // to deposit in. The rendering rule (tenet 7) is what's under test:
        // absence of a confirmation must read as *silence*, not as a
        // negative claim, because the mailbox path never produces an ack
        // even when it delivers perfectly well.
        let key_path = temp_key("unconfirmed", "a");
        keystore::create(&key_path).expect("key");
        let a = Client::open_with(
            &key_path,
            ClientConfig {
                connect_timeout: Duration::from_millis(300),
                ..Default::default()
            },
        )
        .await
        .expect("open");
        let absent = DeviceKey::from_seed([62; 32]).public();
        let contact = Contact {
            keys: vec![absent],
            relays: vec![format!("{}@203.0.113.9:1", hex::encode(&absent.0))],
        };

        // When
        let staged = a
            .stage_send(&[contact], b"into the void".to_vec(), vec![])
            .expect("stage");
        let _ = a.deliver(&staged).await; // every path fails; queued, not lost

        // Then: no confirmation is claimed, and `pending` — not the empty
        // confirmation — carries the honest "we still owe a relay".
        let history = a.history(staged.conversation).expect("history");
        assert!(
            history[0].confirmed.is_empty(),
            "nothing acked, so nothing is claimed"
        );
        assert!(history[0].pending, "the ledger still owes the delivery");
        assert_eq!(
            history[0].body.as_deref(),
            Ok(b"into the void".as_slice()),
            "and the message renders regardless"
        );

        let _ = std::fs::remove_dir_all(temp_root("unconfirmed"));
    }

    #[tokio::test]
    async fn add_acks__should_union_so_a_later_pass_cannot_erase_a_confirmation() {
        // Given: a stored message confirmed by one of two recipients
        let client = Client::open_or_create(&temp_key("acks", "a"))
            .await
            .expect("open");
        let author = DeviceKey::from_seed([71; 32]);
        let (first, second) = (
            DeviceKey::from_seed([72; 32]).public(),
            DeviceKey::from_seed([73; 32]).public(),
        );
        let genesis = message(&author, vec![first, second], None, vec![], 0, 0);
        let conversation = genesis.id();
        client
            .state
            .store_envelope(conversation, &genesis)
            .expect("store");
        client
            .state
            .add_acks(conversation, genesis.id(), &BTreeSet::from([first]))
            .expect("first ack");

        // When: a later delivery pass reaches the *other* recipient, and
        // then one that reaches nobody (a flush with everyone offline)
        client
            .state
            .add_acks(conversation, genesis.id(), &BTreeSet::from([second]))
            .expect("second ack");
        client
            .state
            .add_acks(conversation, genesis.id(), &BTreeSet::new())
            .expect("empty pass");

        // Then: both are held — confirmations accumulate, and a pass that
        // earned nothing takes nothing away. (Stored key-sorted, so compare
        // as a set: the order is the store's, not the acks' arrival order.)
        let held = client.state.acks_in(conversation);
        assert_eq!(
            held.get(&genesis.id())
                .map(|keys| keys.iter().copied().collect::<BTreeSet<_>>()),
            Some(BTreeSet::from([first, second]))
        );

        let _ = std::fs::remove_dir_all(temp_root("acks"));
    }

    #[tokio::test]
    async fn groups__should_grow_thread_and_shrink_through_the_dag() {
        // Given: alice knows bob + carol; bob knows only alice (carol is his
        // non-contact group member); carol knows nobody (receive-only, so
        // the D5 gate declines every direct push to her — her mailbox is the
        // only way in, and the test shuttles it visibly). All three run real
        // handlers over the loopback.
        let wire = Loopback::new();
        let (a, a_net, _a_clock) = loop_client("groups", "alice", &wire);
        let (b, b_net, _b_clock) = loop_client("groups", "bob", &wire);
        let (c, c_net, _c_clock) = loop_client("groups", "carol", &wire);
        let relay_b = DeviceKey::from_seed([101; 32]).public();
        let relay_c = DeviceKey::from_seed([102; 32]).public();
        let relay_a = DeviceKey::from_seed([103; 32]).public();
        a.add_contact(&routed_record(b.public_key(), &relay_b), Some("Bob".into()))
            .expect("alice adds bob");
        a.add_contact(
            &routed_record(c.public_key(), &relay_c),
            Some("Carol".into()),
        )
        .expect("alice adds carol");
        b.add_contact(
            &routed_record(a.public_key(), &relay_a),
            Some("Alice".into()),
        )
        .expect("bob adds alice");

        // When: a 1:1 becomes a group — alice replies with carol added.
        // Bob acks directly (discharging his relay); carol declines the
        // stranger's push, so her copy goes through her mailbox.
        let first = a
            .send(
                &[a.resolve_contact("Bob").expect("resolve")],
                b"hi bob".to_vec(),
                vec![],
            )
            .await
            .expect("first send");
        let conv = first.conversation;
        let rc_welcome = a_net.dial.connect(&relay_c);
        rc_welcome.reply(deposited_frame());
        let mut contacts = a.reply_contacts(conv).expect("reply contacts").contacts;
        contacts.push(a.resolve_contact("Carol").expect("resolve"));
        a.send_in(conv, &contacts, b"welcome carol".to_vec(), vec![])
            .await
            .expect("grow the group");

        // Then: bob has it (direct), carol drains it, and the derived join
        // delta names carol
        let bob_history = b.history(conv).expect("bob history");
        assert_eq!(bob_history.len(), 2);
        assert_eq!(
            bob_history[1].body.as_deref(),
            Ok(b"welcome carol".as_slice())
        );
        assert_eq!(bob_history[1].joined, vec![c.public_key()]);
        script_drain(
            &c_net.dial.connect(&relay_c),
            deposited_envelopes(&rc_welcome),
        );
        let carol_got = c.recv(&[mailbox_spec(&relay_c)]).await.expect("carol recv");
        assert_eq!(carol_got.received.len(), 1);
        assert!(carol_got.received[0].body.is_ok());

        // When: the §3 regression — the adder sends BY NAME to the grown set
        let rc_thread = a_net.dial.connect(&relay_c);
        rc_thread.reply(deposited_frame());
        a.send(
            &[
                a.resolve_contact("Bob").expect("resolve"),
                a.resolve_contact("Carol").expect("resolve"),
            ],
            b"threading check".to_vec(),
            vec![],
        )
        .await
        .expect("send by name");

        // Then: still exactly one conversation on both ends
        let a_convs = a.conversations().expect("alice conversations");
        assert_eq!((a_convs.len(), a_convs[0].id), (1, conv));
        let b_convs = b.conversations().expect("bob conversations");
        assert_eq!((b_convs.len(), b_convs[0].id), (1, conv));

        // When: bob replies with no record for carol — she has no route yet
        // (membership holds; nothing is deposited anywhere: alice acks
        // directly and an unscripted dial to carol's relay would panic)
        let pre_route = b.reply_contacts(conv).expect("bob reply contacts");
        assert_eq!(pre_route.unknown, vec![c.public_key()]);
        b.send_in(
            conv,
            &pre_route.contacts,
            b"from bob, pre-route".to_vec(),
            vec![],
        )
        .await
        .expect("pre-route reply");

        // …and bob learns carol's record from alice (who-is over the wire;
        // alice's real handler serves her user-added contact)
        let learned = b.who_is(c.public_key()).await.expect("who-is");
        assert_eq!(learned.answers.len(), 1);

        // Then: the reply reaches the non-contact member through the
        // learned route — address, don't trust (groups.md §2)
        let rc_learned = b_net.dial.connect(&relay_c);
        rc_learned.reply(deposited_frame());
        let via_learned = b.reply_contacts(conv).expect("bob reply contacts");
        assert!(via_learned.unknown.is_empty(), "carol resolves now");
        b.send_in(
            conv,
            &via_learned.contacts,
            b"carol via learned route".to_vec(),
            vec![],
        )
        .await
        .expect("learned-route reply");
        script_drain(
            &c_net.dial.connect(&relay_c),
            deposited_envelopes(&rc_learned),
        );
        let carol_got = c.recv(&[mailbox_spec(&relay_c)]).await.expect("carol recv");
        let bodies: Vec<_> = carol_got
            .received
            .iter()
            .filter_map(|received| received.body.as_deref().ok())
            .collect();
        assert_eq!(bodies, vec![b"carol via learned route".as_slice()]);
        assert!(
            !b.state
                .contacts()
                .expect("bob contacts")
                .iter()
                .any(|(_, record)| record.keys.contains(&c.public_key())),
            "carol must not have been promoted"
        );

        // When: alice stops including carol — a plain send to bob threads
        // (the {alice,bob} mapping predates the group) and its head shrinks
        // membership, so the next reply-all no longer reaches carol
        a.send(
            &[a.resolve_contact("Bob").expect("resolve")],
            b"just us".to_vec(),
            vec![],
        )
        .await
        .expect("stop-include send");
        let shrunk = a.reply_contacts(conv).expect("alice reply contacts");
        a.send_in(
            conv,
            &shrunk.contacts,
            b"current members only".to_vec(),
            vec![],
        )
        .await
        .expect("post-shrink reply");

        // Then: membership shrank through the DAG; bob got both, carol got
        // neither (her relay took no new deposit — unscripted dials panic)
        let members = a.membership(conv).expect("membership");
        assert_eq!(
            members,
            BTreeSet::from([a.public_key(), b.public_key()]),
            "stop-include must shrink membership"
        );
        let a_convs = a.conversations().expect("alice conversations");
        assert_eq!((a_convs.len(), a_convs[0].id), (1, conv), "no fork");
        let bob_bodies: Vec<_> = b
            .history(conv)
            .expect("bob history")
            .into_iter()
            .filter_map(|message| message.body.ok())
            .collect();
        assert!(bob_bodies.contains(&b"just us".to_vec()));
        assert!(bob_bodies.contains(&b"current members only".to_vec()));

        let _ = std::fs::remove_dir_all(temp_root("groups"));
    }

    #[tokio::test]
    async fn backfill_by_key__should_reach_a_peer_via_its_relay_across_two_relays() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): cross-relay rendezvous, by key alone.
        // Given: two relay services; A homes to one, B to the other — the
        // D0b acceptance shape (never a single shared relay). B knows only
        // A's *key* plus A's stored ContactRecord naming A's relay URL.
        let (_relay_a, url_a) = spawn_test_relay().await;
        let (_relay_b, url_b) = spawn_test_relay().await;
        let a = open_homed("bykey", "server", &url_a).await;
        let b = open_homed("bykey", "client", &url_b).await;
        befriend(&a.state, b.public_key()); // the D0c gate serves contacts only
        a.transport.online().await; // A must be homed before B rendezvouses via its relay

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
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("gate declines, not errors");

        // Then: nothing served, and the successor view is empty too
        assert_eq!(fetched, 0, "a non-contact is served nothing");
        assert!(b.state.load_dag(conversation).is_err());
        let connection = net::connect_peer(
            &b.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            b.config.connect_timeout,
            &SystemClock,
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
        befriend(&a.state, b.public_key());
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
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
        befriend(&a.state, b.public_key());
        let author = DeviceKey::from_seed([4; 32]);
        let msgs = chain(&author, b.public_key(), 5);
        let conversation = msgs[0].id();
        for envelope in &msgs {
            a.state.store_envelope(conversation, envelope).unwrap();
        }
        b.state.store_envelope(conversation, &msgs[2]).unwrap();

        // When
        let fetched = b
            .backfill_addr(conversation, a.transport.peer())
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
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("autosync", "server", &wire);
        let (b, _b_net, _b_clock) = loop_client("autosync", "client", &wire);
        befriend(&a.state, b.public_key());

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
                relay_url: Some("http://203.0.113.1:1".to_string()),
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
                relay: None,
                body: Ok(vec![]),
            }])
            .await;

        // Then: the conversation is whole with zero explicit action
        assert_eq!(healed, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conversation).is_ok());

        let _ = std::fs::remove_dir_all(temp_root("autosync"));
    }

    #[tokio::test]
    async fn participant_labels__should_collapse_a_contacts_device_keys_to_one_label() {
        // Given: bob's record holds two device keys; one unknown key
        let a = Client::open_or_create(&temp_key("labels-dedup", "a"))
            .await
            .expect("open");
        let phone = DeviceKey::from_seed([39; 32]);
        let laptop = DeviceKey::from_seed([40; 32]);
        let unknown = DeviceKey::from_seed([41; 32]).public();
        a.add_contact(
            &ContactRecord::new(
                vec![phone.public(), laptop.public()],
                vec![],
                mailbox_only("bb@203.0.113.1:1"),
            ),
            Some("bob".to_string()),
        )
        .expect("add bob");

        // When
        let labels = a
            .participant_labels(&[phone.public(), laptop.public(), unknown])
            .expect("labels");

        // Then: both device keys collapse to one petname; the unknown key
        // stays distinct, as honest short hex
        assert_eq!(
            labels,
            vec!["bob".to_string(), hex::encode(&unknown.0[..4])]
        );

        let _ = std::fs::remove_dir_all(temp_root("labels-dedup"));
    }

    #[tokio::test]
    async fn participant_labels__should_label_a_recognized_device_by_its_self_claim() {
        // Given
        let a = Client::open_or_create(&temp_key("labels-device", "a"))
            .await
            .expect("open");
        let laptop = DeviceKey::from_seed([55; 32]);
        a.recognize_device(&signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        ))
        .expect("recognize");

        // When / Then: the device labels by its self-claim, and the own
        // cluster covers both keys (what edges filter "others" with)
        assert_eq!(
            a.participant_labels(&[laptop.public()]).expect("labels"),
            vec!["mårten laptop".to_string()]
        );
        assert!(a.own_keys().contains(&laptop.public()));
        assert!(a.own_keys().contains(&a.public_key()));

        let _ = std::fs::remove_dir_all(temp_root("labels-device"));
    }

    #[tokio::test]
    async fn history__should_mark_crossed_in_flight_on_both_clients() {
        // Given: alice and bob share a genesis, then reply concurrently —
        // each deposit is taken by a scripted relay, and the copies cross by
        // direct state transfer (the crossing is what the flag is about; the
        // dial strings' endpoint ids double as the recipient keys)
        let open = |name: &'static str| {
            let key = temp_key("crossed", name);
            keystore::create(&key).expect("key");
            let net = TestTransport::new();
            let client = Client::with_transport(
                keystore::load(&key).expect("load key"),
                &key,
                ClientConfig::default(),
                TestClock::new(),
                SystemClock,
                net.clone(),
            );
            (client, net)
        };
        let (alice, a_net) = open("alice");
        let (bob, b_net) = open("bob");
        let contact = |key: PublicKey| {
            vec![Contact {
                keys: vec![key],
                relays: vec![format!("{}@203.0.113.7:1", hex::encode(&key.0))],
            }]
        };
        for _ in 0..3 {
            a_net
                .dial
                .connect(&bob.public_key())
                .reply(deposited_frame());
        }
        b_net
            .dial
            .connect(&alice.public_key())
            .reply(deposited_frame());
        alice
            .send(&contact(bob.public_key()), b"genesis".to_vec(), vec![])
            .await
            .expect("send");
        let conversation = alice.state.conversations()[0];
        type LoopClient = Client<TestClock, SystemClock, TestTransport>;
        let copy = |from: &LoopClient, to: &LoopClient| {
            for envelope in from.state.load_envelopes(conversation).expect("load") {
                to.state
                    .store_envelope(conversation, &envelope)
                    .expect("copy");
            }
        };
        copy(&alice, &bob);

        // When: concurrent replies, then cross-delivery
        let _ = alice
            .send_in(
                conversation,
                &contact(bob.public_key()),
                b"from alice".to_vec(),
                vec![],
            )
            .await;
        let _ = bob
            .send_in(
                conversation,
                &contact(alice.public_key()),
                b"from bob".to_vec(),
                vec![],
            )
            .await;
        copy(&alice, &bob);
        copy(&bob, &alice);

        // Then: identical linear order on both sides — and both mark the
        // linearized-second of the concurrent pair, nothing else
        let history_a = alice.history(conversation).expect("history");
        let history_b = bob.history(conversation).expect("history");
        let order_a: Vec<MessageId> = history_a.iter().map(|m| m.id).collect();
        let order_b: Vec<MessageId> = history_b.iter().map(|m| m.id).collect();
        assert_eq!(order_a, order_b, "the linear default is unchanged");
        assert_eq!(history_a.len(), 3);
        for history in [&history_a, &history_b] {
            assert!(!history[0].crossed && !history[1].crossed);
            assert!(history[2].crossed, "the second of the concurrent pair");
            assert!(history.iter().all(|m| !m.merged));
        }

        // And: the next reply sees both heads — it renders as the merge
        // it is, not as crossed
        let _ = alice
            .send_in(
                conversation,
                &contact(bob.public_key()),
                b"merge".to_vec(),
                vec![],
            )
            .await;
        let history_a = alice.history(conversation).expect("history");
        assert!(history_a[3].merged && !history_a[3].crossed);

        let _ = std::fs::remove_dir_all(temp_root("crossed"));
    }

    #[tokio::test]
    async fn rewrap__should_make_pre_pairing_history_readable_on_the_paired_device() {
        // Given: the phone holds a fully-sealed conversation from before
        // the laptop's key existed; both home to a relay for dial-by-key
        let wire = Loopback::new();
        let (phone, _p_net, _p_clock) = loop_client("rewrap", "phone", &wire);
        let (laptop, _l_net, _l_clock) = loop_client("rewrap", "laptop", &wire);
        let author = DeviceKey::from_seed([60; 32]);
        let msgs = sealed_chain(&author, phone.public_key(), &[b"one", b"two", b"three"]);
        let conversation = msgs[0].id();
        let ids: BTreeSet<MessageId> = msgs.iter().map(|m| m.id()).collect();
        for envelope in &msgs {
            phone.state.store_envelope(conversation, envelope).unwrap();
        }
        // The pair: the phone recognizes the laptop (what authorizes
        // GetKeys), the laptop recognizes the phone with its homed record
        // (the dial route for backfill + pull)
        phone
            .recognize_device(&ContactRecord::new(
                vec![laptop.public_key()],
                vec![],
                mailbox_only("ll@203.0.113.5:5"),
            ))
            .expect("recognize");
        laptop
            .recognize_device(&ContactRecord::new(
                vec![phone.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            ))
            .expect("recognize back");

        // When: the D2a-style full flow — the laptop holds only the tip,
        // backfills the skeleton by key, then pulls re-wraps
        laptop
            .state
            .store_envelope(conversation, msgs.last().unwrap())
            .unwrap();
        let fetched = laptop
            .backfill_by_key(conversation, phone.public_key())
            .await
            .expect("backfill via the devices store");
        assert_eq!(fetched, 2, "genesis + middle");
        let unreadable = laptop
            .history(conversation)
            .expect("history")
            .iter()
            .filter(|message| message.body.is_err())
            .count();
        assert_eq!(unreadable, 3, "skeleton synced; nothing readable yet");
        let healed = laptop.rewrap_backlog().await;

        // Then: every body opens, and no id moved
        assert_eq!(healed, 3);
        let history = laptop.history(conversation).expect("history");
        let bodies: Vec<Vec<u8>> = history
            .iter()
            .map(|message| message.body.as_ref().expect("opens").clone())
            .collect();
        assert_eq!(
            bodies,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        assert_eq!(
            history.iter().map(|m| m.id).collect::<BTreeSet<_>>(),
            ids,
            "wraps appended outside the hashed core — ids unchanged"
        );

        let _ = std::fs::remove_dir_all(temp_root("rewrap"));
    }

    #[tokio::test]
    async fn get_keys__should_decline_anyone_but_a_recognized_device() {
        // Given: the phone holds a sealed message; alice is a full
        // contact — served history by the gate — but not a device
        let phone = Client::open_or_create(&temp_key("getkeys", "phone"))
            .await
            .expect("open phone");
        let alice = Client::open_or_create(&temp_key("getkeys", "alice"))
            .await
            .expect("open alice");
        let laptop = Client::open_or_create(&temp_key("getkeys", "laptop"))
            .await
            .expect("open laptop");
        let author = DeviceKey::from_seed([61; 32]);
        let msgs = sealed_chain(&author, phone.public_key(), &[b"secret"]);
        let id = msgs[0].id();
        phone.state.store_envelope(id, &msgs[0]).unwrap();
        befriend(&phone.state, alice.public_key());

        // When: the contact asks for re-wraps
        let connection = net::connect_peer(
            &alice.transport,
            &phone.transport.peer(),
            SYNC_ALPN,
            alice.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let declined = net::sync_request(&connection, SyncOp::GetKeys { ids: vec![id] })
            .await
            .expect("round-trip");

        // Then: declined like a miss — re-wrap serving is narrower than
        // the history gate (own devices only at D3)
        assert_eq!(declined, SyncResult::NotHeld);

        // When: a recognized device asks
        phone
            .recognize_device(&ContactRecord::new(
                vec![laptop.public_key()],
                vec![],
                mailbox_only("ll@203.0.113.5:5"),
            ))
            .expect("recognize");
        let connection = net::connect_peer(
            &laptop.transport,
            &phone.transport.peer(),
            SYNC_ALPN,
            laptop.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let served = net::sync_request(&connection, SyncOp::GetKeys { ids: vec![id] })
            .await
            .expect("round-trip");

        // Then: one fresh wrap, sealed to the caller
        let SyncResult::Wraps { wraps } = served else {
            panic!("expected wraps, got {served:?}");
        };
        assert_eq!(wraps.len(), 1);
        assert_eq!(wraps[0].0, id);
        assert_eq!(wraps[0].1.recipient, laptop.public_key());

        let _ = std::fs::remove_dir_all(temp_root("getkeys"));
    }

    #[tokio::test]
    async fn set_profile__should_bump_the_name_attestation_revision_on_rename_only() {
        // Given: a valid dial string (any 32-byte key is an endpoint id)
        let a = Client::open_or_create(&temp_key("rev", "me"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        let revision = |client: &Client| {
            client
                .my_record()
                .expect("record")
                .self_name_claim()
                .expect("claim")
                .1
        };

        // When / Then: first profile starts at 0; a re-save of the same
        // name doesn't bump; a rename supersedes (SPEC §3.2)
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("set");
        assert_eq!(revision(&a), 0);
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("re-set");
        assert_eq!(revision(&a), 0);
        a.set_profile("alicia", std::slice::from_ref(&relay))
            .await
            .expect("rename");
        assert_eq!(revision(&a), 1);

        let _ = std::fs::remove_dir_all(temp_root("rev"));
    }

    #[tokio::test]
    async fn fresh_client__should_dial_by_key_and_home_without_a_reopen() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): homing + dial-by-key rendezvous through a real iroh relay.
        // Given: B homed + serving; A is a FRESH client — key created this
        // run, no profile yet (the new-participant moment from the D2c
        // field run, where who-is was dead until an app restart)
        let (_relay, url) = spawn_test_relay().await;
        let b = open_homed("rebind5", "responder", &url).await;
        let a = Client::open_or_create(&temp_key("rebind5", "newborn"))
            .await
            .expect("open A");
        befriend(&b.state, a.public_key());
        a.add_contact(
            &ContactRecord::new(
                vec![b.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some(url.clone()),
                }],
            ),
            Some("bob".to_string()),
        )
        .expect("add bob");
        b.transport.online().await;

        // When: who-is BEFORE any profile exists — outbound dial-by-key
        // needs only the relay transport, which is now always bound
        let outcome = a.who_is(b.public_key()).await.expect("who_is");

        // Then: answered, not hung and not unreachable
        assert_eq!(
            outcome.answers.len(),
            1,
            "asked {}, unreachable {}",
            outcome.asked,
            outcome.unreachable
        );

        // When: the profile is saved on the RUNNING client
        let spec = format!("{}@203.0.113.1:1#{url}", hex::encode(&a.public_key().0));
        a.set_profile("newborn", std::slice::from_ref(&spec))
            .await
            .expect("profile");

        // Then: the endpoint homes with no reopen — restart-to-apply is gone
        n0_future::time::timeout(Duration::from_secs(5), a.transport.online())
            .await
            .expect("homed at runtime");

        let _ = std::fs::remove_dir_all(temp_root("rebind5"));
    }

    #[tokio::test]
    async fn set_avatar__should_supersede_and_render_our_own() {
        // Given (avatars first: no profile relays yet, so the push loop has
        // nothing to dial and the test stays offline)
        let a = Client::open_or_create(&temp_key("avatar", "me"))
            .await
            .expect("open");

        // When: an avatar is set, then replaced
        let first = a
            .set_avatar(b"first image bytes".to_vec())
            .await
            .expect("set");
        let second = a
            .set_avatar(b"second image bytes".to_vec())
            .await
            .expect("replace");

        // Then: supersession counts up; the published record carries the
        // current claim; our own avatar renders from the local cache
        assert_eq!((first.revision, second.revision), (0, 1));
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let record = a.my_record().expect("record");
        assert_eq!(
            record
                .self_avatar_claim()
                .map(|(hash, _, revision)| (hash, revision)),
            Some((second.hash, 1))
        );
        let rendered = a.avatar(a.public_key()).await.expect("avatar");
        assert_eq!(rendered.as_deref(), Some(b"second image bytes".as_slice()));

        let _ = std::fs::remove_dir_all(temp_root("avatar"));
    }

    #[tokio::test]
    async fn avatar__should_render_a_contacts_avatar_from_the_verified_cache() {
        // Given: A set an avatar and published a record carrying the claim;
        // B stores that record as a contact and holds the ciphertext in its
        // blob cache — exactly what a successful fetch leaves behind
        let a = Client::open_or_create(&temp_key("avatarb", "a"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("avatarb", "b"))
            .await
            .expect("open B");
        let receipt = a.set_avatar(b"portrait".to_vec()).await.expect("set");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("alice", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let ciphertext = a.state.load_blob(&receipt.hash).expect("cached at set");
        b.state.save_blob(&receipt.hash, &ciphertext).expect("seed");
        b.add_contact(&a.my_record().expect("record"), None)
            .expect("add");

        // When
        let rendered = b.avatar(a.public_key()).await.expect("avatar");

        // Then: decrypted via the claim's key; at rest it stays ciphertext
        assert_eq!(rendered.as_deref(), Some(b"portrait".as_slice()));
        assert_ne!(
            b.state.load_blob(&receipt.hash).expect("still cached"),
            b"portrait".to_vec(),
            "cache holds ciphertext, like a relay would"
        );

        let _ = std::fs::remove_dir_all(temp_root("avatarb"));
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
        assert!(matches!(err, Error::NoRelayUrl), "got: {err}");

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
            .backfill_addr(conversation, a.transport.peer())
            .await
            .expect("backfill returns Ok even with nothing to fetch");

        // Then: it fetches nothing and gives up rather than looping — honesty
        // over a fabricated root (the genesis is still missing).
        assert_eq!(fetched, 0);
        assert!(b.state.load_dag(conversation).is_err());

        let _ = std::fs::remove_dir_all(temp_root("stuck"));
    }
}
