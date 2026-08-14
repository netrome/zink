//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use zink_protocol::{
    Attestation, BlobHash, BlobRef, Claim, ContactRecord, DeviceKey, EncryptedBlob,
    MAX_GET_KEYS_IDS, MailboxOp, MailboxResult, MessageEnvelope, MessageId, OpenError, PublicKey,
    RelayEntry, SYNC_ALPN, SignedAttestation, SyncOp, SyncResult, Versioned, open_avatar,
    seal_avatar,
};

use crate::adapters::iroh::IrohTransport;
use crate::adapters::system_clock::SystemClock;
use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::transport::{AcceptUni, Peer, Request, Transport};
use crate::reach::ReachLedger;
use crate::state::ClientState;
use crate::{blobs, hex, keystore, net};

mod outbox;
mod send;

pub use outbox::FlushReport;
pub use send::{ReplyContacts, SendReceipt, StagedSend};

/// A nudge is a zero-length uni stream (live-delivery.md §3); the cap is a
/// backstop against a hostile relay streaming into the signal.
const MAX_NUDGE_BYTES: usize = 64;

/// A who-is query is a burst of speculative dials for display/freshness —
/// it never inherits a send's patience. Effective deadline is
/// `min(connect_timeout, cap)`, so edge tunings only tighten it.
/// Module-level so the tests that fire it reference the same number.
const WHO_IS_DIAL_CAP: Duration = Duration::from_secs(5);

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

pub struct Client<C: Clock = SystemClock, W: WallClock = SystemClock, N: Transport = IrohTransport>
{
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
    /// The auto-query rate limit (D2b, groups.md §4): (subject, conversation)
    /// pairs already asked this run — a drain loop must not re-broadcast
    /// interest in a key. In-memory on purpose; the manual trigger re-asks.
    queried: std::sync::Mutex<BTreeSet<([u8; 32], [u8; 32])>>,
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
            device, state, config, clock, wall_clock, transport,
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

impl<C: Clock, W: WallClock, N: Transport> Client<C, W, N> {
    /// Wire a client around an already-built transport — shared by
    /// `with_device` (real iroh) and the test constructor (doubles).
    fn assemble(
        device: DeviceKey,
        state: ClientState,
        config: ClientConfig,
        clock: C,
        wall_clock: W,
        transport: N,
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
            queried: std::sync::Mutex::default(),
            _serve_task: serve_task,
            direct_sink,
            reach,
            clock,
            wall_clock,
        }
    }

    /// A client on injected doubles: no endpoint, no I/O — the network is
    /// whatever the test scripts.
    #[cfg(test)]
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
        )
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

    /// The post-arrival seam for a direct delivery (D5): the same healing
    /// `recv`/`subscribe` run after a drain — auto-sync the DAG, scoped
    /// who-is for unknown members, re-wrap for paired devices. The edge
    /// calls this from its `on_direct_delivery` sink, because the serving
    /// router holds no `Client` (and the lib spawns no tasks of its own —
    /// I/O and runtimes stay at the edges).
    ///
    /// Skipping it costs correctness only in the healing sense: a directly
    /// delivered message whose ancestors we lack stays an honest orphan
    /// until some later drain heals it — and with the relay unreachable
    /// there may be no later drain.
    pub async fn after_direct(&self, received: &[Received]) {
        self.auto_sync(received).await;
        self.auto_who_is(received).await;
        self.auto_rewrap(received).await;
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

    /// Drain every relay: register, then fetch page-by-page, dedup by
    /// message id, open, and remember what verified; ack each page at its
    /// own cursor.
    ///
    /// **Best-effort per relay** (De6a): one relay we cannot reach costs its
    /// own mail and nothing else — the healthy relays are still drained and
    /// the failures are reported. This is the shape C4a gave the send path
    /// (`SendReceipt.pending_relays`); before De6a a `?` in this loop let the
    /// *first* unreachable relay abort the whole pass, so a second relay's
    /// mail stayed invisible until an unrelated relay came back. Multi-relay
    /// is a tenet; a drain must not be only as available as its worst relay.
    ///
    /// Still an error when **nothing** could be drained anywhere: the caller
    /// asked for mail and got none for a reason it should see. The first
    /// failure is returned verbatim (the rest are logged) — with every relay
    /// down there is no partial result to protect, and edges keep rendering
    /// the precise message they always did.
    pub async fn recv(&self, relays: &[String]) -> Result<RecvReport, Error> {
        // Concurrent per relay (De6d): De6a stopped one dead relay losing
        // another's mail, but left the *deadlines* additive — two relays with
        // the first down cost two `connect_timeout`s to drain one mailbox.
        //
        // The cross-relay dedup set is shared behind a mutex, and its
        // `insert` IS the dedup point: for one id exactly one drain wins and
        // stores it, so a message deposited to several relays still surfaces
        // once. No await is held across the lock. `join_all` preserves input
        // order, so the batch still reads relay by relay.
        let seen: std::sync::Mutex<BTreeSet<[u8; 32]>> = std::sync::Mutex::default();
        let seen = &seen;
        let drains = relays
            .iter()
            .map(|relay| async move { (relay, self.drain_relay(relay, seen).await) });
        let mut received = Vec::new();
        let mut failed: Vec<RelayFailure> = Vec::new();
        for (relay, outcome) in n0_future::join_all(drains).await {
            match outcome {
                Ok(batch) => received.extend(batch),
                Err(error) => {
                    tracing::warn!(relay, %error, "drain failed; other relays continue");
                    failed.push(RelayFailure {
                        relay: relay.clone(),
                        error,
                    });
                }
            }
        }
        if !failed.is_empty() && failed.len() == relays.len() {
            return Err(failed.swap_remove(0).error);
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
        self.auto_who_is(&received).await;
        self.auto_rewrap(&received).await;
        // Post-drain flush (live-delivery.md §2): we're evidently online,
        // so retry anything still owed. Best-effort — a recv must not fail
        // because a *different* relay is down.
        let _ = self.flush_outbox().await;
        Ok(RecvReport { received, failed })
    }

    /// One relay's full drain: connect, register (a registered connection is
    /// what the relay nudges), then page through the mailbox. Split out of
    /// `recv` so a failure anywhere in it — connect, register *or* mid-drain
    /// — is one relay's failure rather than the pass's.
    async fn drain_relay(
        &self,
        relay: &str,
        seen: &std::sync::Mutex<BTreeSet<[u8; 32]>>,
    ) -> Result<Vec<Received>, Error> {
        let connection = net::connect(
            &self.transport,
            relay,
            zink_protocol::MAILBOX_ALPN,
            self.config.connect_timeout,
            &self.clock,
        )
        .await?;
        net::register(&connection, relay).await?;
        self.drain_connection(relay, &connection, seen).await
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
            self.clock.sleep(delay).await;
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
    ) -> Result<(), Error> {
        let connection = net::connect(
            &self.transport,
            relay,
            zink_protocol::MAILBOX_ALPN,
            self.config.connect_timeout,
            &self.clock,
        )
        .await?;
        net::register(&connection, relay).await?;
        tracing::info!(relay, "subscription live (registered)");
        // Catch up on what arrived while we were away *first* — incoming
        // messages take priority over retrying the outbox. Flushing before
        // the drain would delay catch-up by the backlog's timeouts (10s per
        // dead entry), the same coupling removed from the send path. Flush
        // after (the reconnect still means "network is back", §2).
        let received = self
            .drain_connection(relay, &connection, &std::sync::Mutex::default())
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
            self.auto_who_is(&received).await;
            self.auto_rewrap(&received).await;
            on_new(received);
        }
        let _ = self.flush_outbox().await;
        loop {
            // A nudge is a zero-length uni stream — accepting it IS the
            // signal; a failed accept means the connection is gone. The cap
            // is a backstop against a hostile relay streaming into it.
            connection
                .accept_uni(MAX_NUDGE_BYTES)
                .await
                .map_err(|e| Error::Transport(format!("connection lost: {e}")))?;
            let started = self.clock.now();
            let received = self
                .drain_connection(relay, &connection, &std::sync::Mutex::default())
                .await?;
            tracing::info!(
                relay,
                count = received.len(),
                elapsed = ?self.clock.now().duration_since(started),
                "drained (nudge)"
            );
            if !received.is_empty() {
                // Heal before rendering (D0d). Costs nothing when the
                // conversation is ancestor-closed (the common case); dials
                // the sender only on an actual orphan.
                self.auto_sync(&received).await;
                self.auto_who_is(&received).await;
                self.auto_rewrap(&received).await;
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
        connection: &impl Request,
        seen: &std::sync::Mutex<BTreeSet<[u8; 32]>>,
    ) -> Result<Vec<Received>, Error> {
        let mut received = Vec::new();
        let mut after = 0u64;
        loop {
            let items = match net::request(connection, MailboxOp::Fetch { after }).await? {
                MailboxResult::Envelopes { items } => items,
                other => {
                    return Err(Error::UnexpectedResponse(format!(
                        "from {relay}: {other:?}"
                    )));
                }
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
                if !item.envelope.version_supported() {
                    // A future protocol version this client can't parse
                    // (SPEC §10: surfaced, never misparsed). Skipped, and
                    // acked with the page so it doesn't wedge the drain.
                    tracing::warn!("skipping message with unsupported version");
                    continue;
                }
                // The dedup point across concurrent relay drains (De6d): the
                // lock is held for the insert alone. A poisoned lock recovers
                // rather than failing the drain — the set guards no invariant,
                // and the worst a lost entry costs is one duplicate, which
                // storage collapses anyway (content-addressed).
                let first_time = seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(item.envelope.id().0);
                if !first_time {
                    continue; // already drained via another relay
                }
                let body = item.envelope.open(&self.device);
                if body.is_ok() {
                    self.remember(&item.envelope)?;
                }
                received.push(Received {
                    envelope: item.envelope,
                    relay: Some(relay.to_string()),
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

    /// Store a scanned/pasted record. The petname defaults to the contact's
    /// self-claimed name; the caller may override (petnames are ours, not
    /// theirs). Returns the petname it was stored under.
    ///
    /// **Contact identity is key overlap** (multi-device.md §4): a record
    /// sharing any key with an existing contact is an update *of that
    /// entry* — accepted only under that entry's own petname, which is the
    /// explicit confirm. A `keys` list is unauthenticated per-key, so a
    /// hostile record smuggling a contact's key must never rewrite that
    /// contact's trust anchor as a side effect of adding "someone new";
    /// a record overlapping two or more contacts is refused outright.
    pub fn add_contact(
        &self,
        record: &ContactRecord,
        petname: Option<String>,
    ) -> Result<String, Error> {
        if record.keys.is_empty() {
            return Err(Error::InvalidRecord("record has no keys".into()));
        }
        if record.relays.is_empty() {
            return Err(Error::InvalidRecord(
                "record has no relays — no way to reach them".into(),
            ));
        }
        let petname = petname
            .or_else(|| record.self_claimed_name().map(str::to_string))
            .ok_or_else(|| {
                Error::InvalidRecord(
                    "record has no valid self-claimed name; provide a petname".into(),
                )
            })?;
        let contacts = self.state.contacts()?;
        let overlapping: Vec<&(String, ContactRecord)> = contacts
            .iter()
            .filter(|(_, existing)| existing.keys.iter().any(|key| record.keys.contains(key)))
            .collect();
        match overlapping.as_slice() {
            // A brand-new person; the petname must still resolve to one
            // person (send-by-name stays unambiguous).
            [] => {
                if contacts.iter().any(|(name, _)| *name == petname) {
                    return Err(Error::PetnameCollision(petname));
                }
                self.state.save_contact(&petname, record)?;
            }
            [(existing_name, existing)] => {
                if *existing_name != petname {
                    return Err(Error::ContactOverlap {
                        existing: existing_name.clone(),
                    });
                }
                self.state.replace_contact(existing, &petname, record)?;
            }
            several => {
                let names: Vec<&str> = several.iter().map(|(name, _)| name.as_str()).collect();
                return Err(Error::AmbiguousOverlap(names.join(", ")));
            }
        }
        Ok(petname)
    }

    /// Rename a contact — set *my* petname for them (my lens, U4). Purely
    /// local: the petname is a key-stemmed sibling file, so this rewrites it
    /// in place; nothing is published (sharing a name with friends is the
    /// explicit `vouch`). Rejects an empty name or a collision with another
    /// contact, so send-by-name stays unambiguous.
    pub fn rename(&self, current: &str, new: &str) -> Result<(), Error> {
        let new = new.trim();
        if new.is_empty() {
            return Err(Error::InvalidInput("petname cannot be empty".into()));
        }
        if new == current {
            return Ok(());
        }
        let contacts = self.state.contacts()?;
        let record = contacts
            .iter()
            .find(|(name, _)| name == current)
            .map(|(_, record)| record.clone())
            .ok_or_else(|| Error::NotAContact(format!("no contact named {current:?}")))?;
        if contacts.iter().any(|(name, _)| name == new) {
            return Err(Error::PetnameCollision(new.to_string()));
        }
        self.state.save_contact(new, &record)
    }

    /// All stored contacts as `(petname, record)`.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, Error> {
        self.state.contacts()
    }

    /// The one-way "recognize this device as me" act (multi-device.md §3),
    /// called by the edge *after* its fingerprint confirm: store the
    /// scanned record in the own-devices store and sign the link vouch
    /// that `my_record` carries from now on. One direction only — the
    /// shown side does nothing, and serving/inclusion move only from this
    /// device toward the recognized key. Returns that key.
    pub fn recognize_device(&self, record: &ContactRecord) -> Result<PublicKey, Error> {
        let device_key = *record
            .keys
            .first()
            .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?;
        if device_key == self.device.public() {
            return Err(Error::InvalidInput(
                "that is this device's own record".into(),
            ));
        }
        if record.relays.is_empty() {
            return Err(Error::InvalidRecord(
                "record has no relays — send-to-self deposits need a mailbox".into(),
            ));
        }
        // Revision 0 is right: supersession scopes per linked key
        // (SPEC §3.2), so the first link per device never contends and a
        // re-recognize re-signs the identical attestation. Withdrawal is
        // the deferred `Negative` flow (D4).
        let vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject: self.device.public(),
                claim: Claim::SamePersonAs(device_key),
                revision: 0,
            },
            &self.device,
        );
        self.state.save_recognized_device(record, &vouch)?;
        Ok(device_key)
    }

    /// Recognized own devices as `(device key, record)` — this device's
    /// recognition set, its own social-graph decision like everything else.
    pub fn recognized_devices(&self) -> Vec<(PublicKey, ContactRecord)> {
        self.state.recognized_devices()
    }

    /// Vouch for a contact (D4a, web-of-trust.md §2): sign "I call this
    /// key <petname>" and serve it as an endorsement with every `WhoIs`
    /// answer about them from now on. **Explicit** — it broadcasts your
    /// petname, which stays private by default (SPEC §3.2); nothing
    /// vouches on add. A re-vouch (after a rename, say) supersedes at the
    /// next revision. Returns the vouched key.
    pub fn vouch(&self, petname: &str) -> Result<PublicKey, Error> {
        let subject = self.contact_key(petname)?;
        let revision = self
            .state
            .vouch_for(&subject)
            .map(|prior| prior.attestation.revision + 1)
            .unwrap_or(0);
        let vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject,
                claim: Claim::Name(petname.to_string()),
                revision,
            },
            &self.device,
        );
        self.state.save_vouch(&subject, &vouch)?;
        Ok(subject)
    }

    /// Withdraw a vouch: it stops being served, and observers'
    /// per-responder learned entries replace it away on their next
    /// freshness pull. The *active* disavowal (`Negative`) is D4b.
    pub fn unvouch(&self, petname: &str) -> Result<(), Error> {
        let subject = self.contact_key(petname)?;
        self.state.remove_vouch(&subject);
        Ok(())
    }

    /// Un-recognize a device, locally only (web-of-trust.md §6): it stops
    /// being served, included, and re-wrapped — but nothing is published.
    /// Losing interest in a sibling is not the same as declaring it
    /// compromised; that is `repudiate`.
    pub fn unrecognize_device(&self, key: &PublicKey) {
        self.state.remove_recognized_device(key);
    }

    /// Repudiate a key (web-of-trust.md §4/§5): sign the `Negative` that
    /// voids our earlier claims about it, publish it (record +
    /// endorsements), and un-recognize a repudiated sibling. Advisory
    /// like every claim — observers weigh it by their own policy, and a
    /// yet-higher re-vouch restores.
    pub fn repudiate(&self, key: PublicKey) -> Result<(), Error> {
        if key == self.device.public() {
            return Err(Error::InvalidInput("that is this device's own key".into()));
        }
        let stance = self
            .state
            .vouch_for(&key)
            .map(|prior| prior.attestation.revision);
        let device_link = self
            .state
            .recognized_devices()
            .iter()
            .find(|(device_key, _)| *device_key == key)
            .and_then(|_| {
                self.state.device_vouches().into_iter().find(|vouch| {
                matches!(vouch.attestation.claim, Claim::SamePersonAs(linked) if linked == key)
            })
            })
            .map(|vouch| vouch.attestation.revision);
        let revision = match stance.into_iter().chain(device_link).max() {
            Some(highest) => highest + 1,
            None => 0,
        };
        let negative = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject: key,
                claim: Claim::Negative,
                revision,
            },
            &self.device,
        );
        self.state.save_vouch(&key, &negative)?;
        self.state.remove_recognized_device(&key);
        Ok(())
    }

    /// Valid disavowals of a key across everything held, each saying WHO
    /// and whether it `excludes` the key from addressed sets — true only
    /// for our own stance or a same-person disavowal; third-party
    /// negatives warn, never exclude (web-of-trust.md §4).
    pub fn disavowals(&self, key: PublicKey) -> Result<Vec<Disavowal>, Error> {
        let contacts = self.state.contacts()?;
        let attestations = self.held_attestations(key)?;
        let same_entry = |attester: &PublicKey| {
            contacts
                .iter()
                .any(|(_, record)| record.keys.contains(attester) && record.keys.contains(&key))
        };
        // No voiding here, deliberately: a voided link no longer clusters,
        // but it still proves the keys were one person — which is what
        // makes a disavowal "their own" (web-of-trust.md §4).
        let linked = |attester: &PublicKey| {
            attestations.iter().any(|signed| {
                let attestation = &signed.attestation;
                let Claim::SamePersonAs(to) = attestation.claim else {
                    return false;
                };
                attestation.attester == attestation.subject
                    && signed.verify().is_ok()
                    && ((attestation.attester == *attester && to == key)
                        || (attestation.attester == key && to == *attester))
            })
        };
        let own = self.own_keys();
        let mut disavowals: Vec<Disavowal> = Vec::new();
        for signed in &attestations {
            let Some((attester, disavowed, _)) = zink_protocol::verified_negative(signed) else {
                continue;
            };
            if disavowed != key || disavowals.iter().any(|d| d.attester == attester) {
                continue;
            }
            let excludes = own.contains(&attester) || same_entry(&attester) || linked(&attester);
            disavowals.push(Disavowal {
                attester,
                attester_label: self
                    .participant_labels(&[attester])?
                    .pop()
                    .unwrap_or_default(),
                excludes,
            });
        }
        Ok(disavowals)
    }

    /// Every attestation this client holds that could bear on `key`: its
    /// own stances, stored contact records, and the learned records +
    /// endorsements for the key and for each contact's keys.
    fn held_attestations(&self, key: PublicKey) -> Result<Vec<SignedAttestation>, Error> {
        let mut attestations: Vec<SignedAttestation> = Vec::new();
        attestations.extend(self.state.vouch_for(&key));
        attestations.extend(self.state.issued_negatives());
        for entry in self.state.learned(&key) {
            attestations.extend(entry.record.attestations.clone());
            attestations.extend(entry.endorsements.clone());
        }
        for (_, record) in self.state.contacts()? {
            for contact_key in &record.keys {
                for entry in self.state.learned(contact_key) {
                    attestations.extend(entry.record.attestations.clone());
                    attestations.extend(entry.endorsements.clone());
                }
            }
            attestations.extend(record.attestations);
        }
        Ok(attestations)
    }

    /// Whether this device currently vouches for a key (edge rendering).
    pub fn vouches(&self, subject: &PublicKey) -> bool {
        self.state.vouch_for(subject).is_some()
    }

    /// A contact entry's identity key (its record's first key).
    fn contact_key(&self, petname: &str) -> Result<PublicKey, Error> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .and_then(|(_, record)| record.keys.first().copied())
            .ok_or_else(|| Error::NotAContact(format!("no contact named {petname:?}")))
    }

    /// This device's key cluster as its own client sees it: self plus the
    /// recognized devices (D3c). Edges filter "other participants" with
    /// this — a conversation with a contact is not "with mårten laptop".
    pub fn own_keys(&self) -> BTreeSet<PublicKey> {
        std::iter::once(self.device.public())
            .chain(self.state.recognized_devices().into_iter().map(|(k, _)| k))
            .collect()
    }

    /// The stored record for a key this client trusts: a user-added
    /// contact's, else a recognized own device's (D3c — devices resolve
    /// routes and labels through their own store, never through contacts).
    fn trusted_record_for(&self, key: &PublicKey) -> Option<ContactRecord> {
        self.state
            .contacts()
            .unwrap_or_default()
            .into_iter()
            .find(|(_, record)| record.keys.contains(key))
            .map(|(_, record)| record)
            .or_else(|| {
                self.state
                    .recognized_devices()
                    .into_iter()
                    .find(|(device_key, _)| device_key == key)
                    .map(|(_, record)| record)
            })
    }

    /// Petname → the Contact to send to. Keys come from the user-added
    /// record alone; relays resolve at read time (D1b, who-is-this.md §7).
    pub fn resolve_contact(&self, petname: &str) -> Result<Contact, Error> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .map(|(_, record)| self.contact_from(&record))
            .ok_or_else(|| Error::NotAContact(format!("no contact named {petname:?}")))
    }

    /// Keys from the stored record; relays resolved at read time (§7).
    fn contact_from(&self, record: &ContactRecord) -> Contact {
        let relays = match record.keys.first() {
            Some(&key) => self.effective_relays(key, Some(record)),
            None => record.relays.clone(),
        };
        Contact {
            keys: record.keys.clone(),
            relays: relays.into_iter().map(|entry| entry.mailbox).collect(),
        }
    }

    /// The relay entries to reach a person at, resolved at read time
    /// (who-is-this.md §7) — nothing stored is ever mutated. Provenance
    /// classes, first non-empty class wins, latest receipt within one:
    /// **subject-served** (authenticated by the connection key) > the
    /// **user-added record** (authenticated by the scan / explicit add) >
    /// **contact-served** hearsay (only ever decisive in the one-way-add
    /// bootstrap, where it's the whole point). Keys never come from
    /// learned records — sealing stays on the user-added record until D3.
    fn effective_relays(&self, key: PublicKey, stored: Option<&ContactRecord>) -> Vec<RelayEntry> {
        let learned = self.state.learned(&key);
        let best = |from_subject: bool| {
            learned
                .iter()
                .filter(|entry| (entry.responder == key) == from_subject)
                .filter(|entry| !entry.record.relays.is_empty())
                .max_by_key(|entry| entry.received_ms)
                .map(|entry| entry.record.relays.clone())
        };
        best(true)
            .or_else(|| {
                stored
                    .filter(|record| !record.relays.is_empty())
                    .map(|record| record.relays.clone())
            })
            .or_else(|| best(false))
            .unwrap_or_default()
    }

    /// The dialable peer address for a person: their key, routed via the
    /// relay URLs their records resolve to at read time.
    fn peer_addr_for(&self, key: PublicKey, stored: Option<&ContactRecord>) -> Result<Peer, Error> {
        let relay_urls: Vec<String> = self
            .effective_relays(key, stored)
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref().map(str::to_string))
            .collect();
        if relay_urls.is_empty() {
            return Err(Error::NoRelayUrl);
        }
        crate::adapters::iroh::validated_peer(key, relay_urls)
    }

    /// Ask the network "who is this key?" (D1b, who-is-this.md §5): dial
    /// every dialable contact — the subject itself among them, if stored —
    /// send `WhoIs`, validate answers like scanned QRs, and append them to
    /// the learned store with provenance. The contact store is never
    /// touched. **Manual trigger only** (§5): asking broadcasts your
    /// interest in the key to everyone asked, so no drain path calls this.
    /// Best-effort — and **concurrent with a capped deadline** (De3): one
    /// offline contact costs one bounded dial, never a serial sum of
    /// timeouts. Resolution over everything learned so far is
    /// `resolve_name`.
    pub async fn who_is(&self, subject: PublicKey) -> Result<WhoIsOutcome, Error> {
        // Contacts plus recognized own devices (D3c): siblings serve this
        // caller like self, and on a fresh device they are the only
        // responders there are.
        let responders: Vec<PublicKey> = self
            .state
            .contacts()?
            .iter()
            .filter_map(|(_, record)| record.keys.first().copied())
            .chain(self.state.recognized_devices().into_iter().map(|(k, _)| k))
            .collect();
        self.who_is_among(subject, &responders).await
    }

    /// `who_is` scoped to specific responders (D2b, groups.md §4) — the
    /// auto-query's shape: inside a conversation, asking its *own
    /// participants* about a member key reveals nothing they don't already
    /// know, unlike asking the whole contact list. Responders resolve to
    /// routes like reply targets do (contact or learned records);
    /// undialable ones are skipped and never counted as "asked".
    pub async fn who_is_among(
        &self,
        subject: PublicKey,
        responders: &[PublicKey],
    ) -> Result<WhoIsOutcome, Error> {
        enum Query {
            Answer(WhoIsAnswer),
            Nothing,
            Unreachable,
        }
        let records = self.state.contacts()?;
        let me = self.device.public();
        let mut targets = Vec::new();
        for &responder in responders {
            if responder == me {
                continue;
            }
            let named = records
                .iter()
                .find(|(_, record)| record.keys.contains(&responder));
            let petname = named
                .map(|(petname, _)| petname.clone())
                .unwrap_or_else(|| hex::encode(&responder.0)[..8].to_string());
            // A responder that is no contact may be a recognized own
            // device (D3c) — its route lives in the devices store.
            let stored = match named.map(|(_, record)| record.clone()) {
                Some(record) => Some(record),
                None => self.trusted_record_for(&responder),
            };
            match self.peer_addr_for(responder, stored.as_ref()) {
                Ok(addr) => targets.push((petname, responder, addr)),
                // No dialable route — never counted as "asked".
                Err(_) => tracing::debug!(%petname, "who-is: no dialable route; skipped"),
            }
        }
        let asked = targets.len();
        let timeout = self.config.connect_timeout.min(WHO_IS_DIAL_CAP);
        let queries = targets.into_iter().map(|(petname, responder, addr)| async move {
            let connection =
                match net::connect_peer(&self.transport, &addr, SYNC_ALPN, timeout, &self.clock)
                    .await
                {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::debug!(%petname, %error, "who-is: contact unreachable");
                        return Query::Unreachable;
                    }
                };
            match net::sync_request(&connection, SyncOp::WhoIs { key: subject }).await {
                Ok(SyncResult::Known {
                    record: served,
                    endorsements,
                }) => {
                    // Validated like a scanned QR: the record must name the
                    // subject; name claims verify at read time (§5).
                    if !served.keys.contains(&subject) {
                        tracing::warn!(%petname, "who-is: answer does not name the subject; dropped");
                        return Query::Nothing;
                    }
                    Query::Answer(WhoIsAnswer {
                        responder,
                        responder_petname: petname,
                        record: *served,
                        endorsements: valid_endorsements(responder, subject, endorsements),
                    })
                }
                Ok(SyncResult::NotHeld) => Query::Nothing,
                Ok(other) => {
                    tracing::warn!(%petname, ?other, "who-is: unexpected response");
                    Query::Nothing
                }
                Err(error) => {
                    tracing::debug!(%petname, %error, "who-is: request failed");
                    Query::Unreachable
                }
            }
        });
        let mut answers = Vec::new();
        let mut unreachable = 0;
        for outcome in n0_future::join_all(queries).await {
            match outcome {
                Query::Answer(answer) => {
                    self.state.save_learned(
                        &subject,
                        &answer.responder,
                        &answer.record,
                        &answer.endorsements,
                        self.wall_clock.now_ms(),
                    )?;
                    answers.push(answer);
                }
                Query::Nothing => {}
                Query::Unreachable => unreachable += 1,
            }
        }
        Ok(WhoIsOutcome {
            answers,
            asked,
            unreachable,
        })
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

    /// The contributing-contact rule (D2b, groups.md §6): a conversation
    /// is legitimate iff at least one stored contact **authored** a held
    /// message in it. Presence in `recipients` is attacker-controlled — a
    /// spammer can list your friends for free — authorship is not (every
    /// stored envelope verified its sender's signature). Presentation and
    /// auto-query policy, never storage: a conversation upgrades
    /// retroactively the moment a contact's message arrives.
    pub fn has_contributing_contact(&self, conversation: MessageId) -> Result<bool, Error> {
        let contacts = self.state.contacts()?;
        let own = self.own_keys();
        let envelopes = self.state.load_envelopes(conversation)?;
        Ok(self.contributed_to(&envelopes, &contacts, &own))
    }

    /// The contributing-contact rule over envelopes already loaded — has a
    /// key we trust **authored** one of them?
    ///
    /// Own-cluster authorship counts (D3c, multi-device.md §5): there is no
    /// key this client trusts more than its own — a fresh device's
    /// conversations arrive authored by its siblings, and an empty contact
    /// store must not mute the auto-query bootstrap or quarantine your own
    /// history.
    fn contributed_to(
        &self,
        envelopes: &[MessageEnvelope],
        contacts: &[(String, ContactRecord)],
        own: &BTreeSet<PublicKey>,
    ) -> bool {
        envelopes.iter().any(|envelope| {
            own.contains(&envelope.core.sender)
                || contacts
                    .iter()
                    .any(|(_, record)| record.keys.contains(&envelope.core.sender))
        })
    }

    /// The scoped auto-query (D2b, groups.md §4 — the who-is-this.md §5
    /// carve-out): after a drain, resolve unknown members of the touched
    /// conversations by asking those conversations' *own participants* —
    /// their presence in the signed `recipients` is already mutual
    /// knowledge there, so the query reveals nothing (unlike asking the
    /// whole contact list, which stays forbidden). Gated on the
    /// contributing-contact rule (§6) so a fabricated group can't make
    /// this client broadcast queries, and rate-limited per
    /// (subject, conversation) per run. Best-effort and non-fatal: answers
    /// land in the learned store; edges pick them up via `resolve_name`
    /// at the next render.
    async fn auto_who_is(&self, received: &[Received]) {
        let me = self.device.public();
        let Ok(records) = self.state.contacts() else {
            return;
        };
        // Recognized own devices are never "unknown members" (D3c) — and
        // they ARE responders: a fresh device's only route to its
        // contacts' records is asking its siblings (multi-device.md §5).
        let own = self.own_keys();
        let known = |key: &PublicKey| {
            own.contains(key) || records.iter().any(|(_, record)| record.keys.contains(key))
        };
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
            if !self.has_contributing_contact(conversation).unwrap_or(false) {
                continue;
            }
            let Ok(members) = self.membership(conversation) else {
                continue;
            };
            let responders: Vec<PublicKey> =
                members.iter().copied().filter(|key| *key != me).collect();
            for subject in members.iter().copied() {
                if subject == me || known(&subject) || !self.state.learned(&subject).is_empty() {
                    continue;
                }
                if !self
                    .queried
                    .lock()
                    .expect("queried lock")
                    .insert((subject.0, conversation.0))
                {
                    continue;
                }
                match self.who_is_among(subject, &responders).await {
                    Ok(outcome) if !outcome.answers.is_empty() => {
                        tracing::info!(
                            answers = outcome.answers.len(),
                            "auto who-is resolved a member"
                        )
                    }
                    Ok(_) => tracing::debug!("auto who-is: no answers"),
                    Err(error) => tracing::debug!(%error, "auto who-is failed"),
                }
            }
        }
    }

    /// Resolve a key to the best-believed name (who-is-this.md §6):
    /// petname (manual, always wins) > learned self-claims (grouped by
    /// name, highest revision first — a genuine tie surfaces both, never
    /// arbitrated) > unknown (the edge renders the key). Provenance rides
    /// along: which contacts hold a record claiming each name, and whether
    /// the subject itself served one.
    pub fn resolve_name(&self, key: PublicKey) -> Result<ResolvedName, Error> {
        if let Some((petname, _)) = self
            .state
            .contacts()?
            .iter()
            .find(|(_, record)| record.keys.contains(&key))
        {
            return Ok(ResolvedName::Petname(petname.clone()));
        }
        let names: Vec<LearnedName> = self
            .learned_candidates(key)?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        if names.is_empty() {
            return Ok(ResolvedName::Unknown);
        }
        Ok(ResolvedName::Learned(names))
    }

    /// Render-ready candidates for a key from everything learned so far:
    /// `resolve_name`'s groups, each paired with the freshest record
    /// claiming that name (highest revision, then latest receipt) — the
    /// promotable payload behind the wild-key popup's add button (D2c,
    /// groups.md §5). Best first; a genuine tie surfaces both.
    pub fn learned_candidates(
        &self,
        subject: PublicKey,
    ) -> Result<Vec<(LearnedName, ContactRecord)>, Error> {
        let contacts = self.state.contacts()?;
        let petname_of = |responder: PublicKey| {
            contacts
                .iter()
                .find(|(_, record)| record.keys.contains(&responder))
                .map(|(petname, _)| petname.clone())
                // A learned entry can outlive its responder's contact
                // status; fall back to an honest key prefix.
                .unwrap_or_else(|| hex::encode(&responder.0[..4]))
        };
        let mut groups: BTreeMap<String, (LearnedName, ContactRecord, (u64, u64))> =
            BTreeMap::new();
        let entries = self.state.learned(&subject);
        for entry in &entries {
            let Some((name, revision)) = entry.record.self_name_claim() else {
                continue; // no verifiable self-claim — relays-only evidence
            };
            let name = name.to_string();
            let rank = (revision, entry.received_ms);
            let group = groups.entry(name.clone()).or_insert_with(|| {
                (
                    LearnedName {
                        name,
                        revision,
                        held_by: Vec::new(),
                        confirmed_by_subject: false,
                        endorsed_by: Vec::new(),
                    },
                    entry.record.clone(),
                    rank,
                )
            });
            group.0.revision = group.0.revision.max(revision);
            if entry.responder == subject {
                group.0.confirmed_by_subject = true;
            } else {
                group.0.held_by.push(petname_of(entry.responder));
            }
            if rank > group.2 {
                group.1 = entry.record.clone();
                group.2 = rank;
            }
        }
        // Endorsed names (D4a): each responder's own vouch joins its name
        // group — or founds one, paired with that responder's served
        // record as the promotable payload. Endorsement revisions are the
        // voucher's counter (a different supersession scope), so they
        // never mix into the group's self-claim `revision`. The voiding
        // rule applies per voucher (D4b): a name behind the same
        // attester's higher-revision `Negative` is withdrawn, not shown.
        let negatives: Vec<(PublicKey, u64)> = entries
            .iter()
            .flat_map(|entry| entry.endorsements.iter())
            .filter_map(zink_protocol::verified_negative)
            .filter(|(_, disavowed, _)| *disavowed == subject)
            .map(|(attester, _, revision)| (attester, revision))
            .collect();
        for entry in &entries {
            for signed in &entry.endorsements {
                let Claim::Name(name) = &signed.attestation.claim else {
                    continue; // negatives render via `disavowals`, not here
                };
                let voided = negatives.iter().any(|(attester, revision)| {
                    *attester == signed.attestation.attester
                        && *revision > signed.attestation.revision
                });
                if voided || signed.verify().is_err() {
                    continue;
                }
                let name = name.clone();
                let rank = (0, entry.received_ms);
                let group = groups.entry(name.clone()).or_insert_with(|| {
                    (
                        LearnedName {
                            name,
                            revision: 0,
                            held_by: Vec::new(),
                            confirmed_by_subject: false,
                            endorsed_by: Vec::new(),
                        },
                        entry.record.clone(),
                        rank,
                    )
                });
                group.0.endorsed_by.push(petname_of(entry.responder));
            }
        }
        let mut candidates: Vec<(LearnedName, ContactRecord)> = groups
            .into_values()
            .map(|(name, record, _)| (name, record))
            .collect();
        // Ranking (web-of-trust.md §2): names with verified self-claim
        // evidence outrank endorsed-only ones; then self-claim revision,
        // then agreement, then name — a deterministic *default lens*.
        candidates.sort_by(|a, b| {
            let self_claimed =
                |name: &LearnedName| name.confirmed_by_subject || !name.held_by.is_empty();
            let agreement = |name: &LearnedName| name.held_by.len() + name.endorsed_by.len();
            self_claimed(&b.0)
                .cmp(&self_claimed(&a.0))
                .then_with(|| b.0.revision.cmp(&a.0.revision))
                .then_with(|| agreement(&b.0).cmp(&agreement(&a.0)))
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        Ok(candidates)
    }

    /// Link evidence for an unknown key across everything this client
    /// holds — stored contact records plus all learned records for the
    /// subject and for each contact's keys (multi-device.md §7): per
    /// contact whose keys verifiably vouch the subject, the evidence tier.
    /// Strongest first; several contacts claiming the same key all
    /// surface, honestly — the §8 misattribution case is exactly why the
    /// popup says *who* claims, and why nothing here auto-adopts: every
    /// tier only ever produces an offer, accepted via `add_contact`.
    pub fn device_evidence(&self, subject: PublicKey) -> Result<Vec<DeviceEvidence>, Error> {
        let contacts = self.state.contacts()?;
        // Links AND negatives now travel as endorsements too, so the pool
        // is everything held (D4b) — `link_tier` applies the voiding rule.
        let attestations = self.held_attestations(subject)?;
        let mut evidence: Vec<DeviceEvidence> = contacts
            .iter()
            .filter_map(|(petname, record)| {
                match zink_protocol::link_tier(&record.keys, subject, &attestations) {
                    zink_protocol::LinkTier::None => None,
                    tier => Some(DeviceEvidence {
                        petname: petname.clone(),
                        tier,
                    }),
                }
            })
            .collect();
        evidence.sort_by(|a, b| b.tier.cmp(&a.tier).then_with(|| a.petname.cmp(&b.petname)));
        Ok(evidence)
    }

    /// Ignore an unknown key (D2c, groups.md §5): the popup stops
    /// proposing it; the key keeps rendering as hex (honest), and the
    /// manual who-is path stays available. Local presentation policy.
    pub fn dismiss(&self, key: PublicKey) -> Result<(), Error> {
        self.state.dismiss_key(&key)
    }

    pub fn dismissed(&self) -> BTreeSet<PublicKey> {
        self.state.dismissed_keys()
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
    pub fn parse(spec: &str) -> Result<Self, Error> {
        let (key_hex, relay_list) = spec.split_once('@').ok_or_else(|| {
            Error::InvalidInput("contact must be <pubkey>@<relay>[,<relay>...]".into())
        })?;
        let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
        for relay in &relays {
            // Validate early, before any network work.
            crate::adapters::iroh::parse_dial(relay)?;
        }
        Ok(Contact {
            keys: vec![PublicKey(hex::parse32(key_hex)?)],
            relays,
        })
    }
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

/// What one `recv` pass drained, and from where it got nothing (De6a).
///
/// `failed` non-empty with `received` non-empty is the honest partial view
/// (tenet 6): mail from the relays that answered, plus which relays did not,
/// so an edge can say "your view may be incomplete" instead of implying the
/// mailbox is empty. Every relay answering leaves `failed` empty; *no* relay
/// answering is an `Err` from `recv` rather than a report.
///
/// Deliberately not `Debug`: it carries opened message bodies, and a report
/// that can be formatted into a log is a plaintext leak waiting to happen.
#[derive(Default)]
pub struct RecvReport {
    pub received: Vec<Received>,
    /// Relays this pass could not drain, with why. Not queued for anything:
    /// unlike a send, a drain has nothing owed — the mail stays in the
    /// mailbox and the next pass (or nudge) picks it up.
    pub failed: Vec<RelayFailure>,
}

/// One relay that could not be drained this pass.
#[derive(Debug)]
pub struct RelayFailure {
    pub relay: String,
    pub error: Error,
}

/// One fetched envelope: opened if this device could decrypt it. The edge
/// decides presentation; `envelope.core` has sender, conversation, blob refs.
pub struct Received {
    pub envelope: MessageEnvelope,
    /// The relay it arrived through — where its blobs can be fetched.
    /// `None` for a **direct** arrival (D5): no relay was on the path, so
    /// blobs resolve through this device's own home-relay caches, which is
    /// where senders push them anyway (C3a).
    pub relay: Option<String>,
    pub body: Result<Vec<u8>, OpenError>,
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

/// What a `who_is` query accomplished (De3): the validated answers plus the
/// honest denominator — "0 answers with 3 of 4 unreachable" and "0 answers,
/// everyone reachable, nobody knows this key" are different verdicts, and
/// the edge must be able to say which one happened.
pub struct WhoIsOutcome {
    pub answers: Vec<WhoIsAnswer>,
    /// Dialable contacts queried (mailbox-only records are skipped).
    pub asked: usize,
    /// Of those, how many could not be reached or asked to completion.
    pub unreachable: usize,
}

/// One validated `who-is` answer (already persisted to the learned store).
/// `responder` — the contact who served it — vouches for *holding* this
/// record, nothing more; the record's claims verify on their own.
pub struct WhoIsAnswer {
    pub responder: PublicKey,
    /// The petname the responder is stored under (the contact we asked).
    pub responder_petname: String,
    pub record: ContactRecord,
    /// The responder's own validated claims about the subject (D4a).
    pub endorsements: Vec<SignedAttestation>,
}

/// Endorsement validation (D4a, web-of-trust.md §3): keep only claims the
/// answering key itself signed about the queried subject — signature
/// verifies, `attester` IS the responder (relaying others' claims would
/// be second-hand gossip; hop limit 1 stays structural), `subject` is the
/// queried key. Anything else is dropped with a warning, never fatal.
pub(crate) fn valid_endorsements(
    responder: PublicKey,
    subject: PublicKey,
    endorsements: Vec<SignedAttestation>,
) -> Vec<SignedAttestation> {
    endorsements
        .into_iter()
        .filter(|signed| {
            let attestation = &signed.attestation;
            let valid = attestation.attester == responder
                && attestation.subject == subject
                && signed.verify().is_ok();
            if !valid {
                tracing::warn!("dropping an invalid endorsement");
            }
            valid
        })
        .collect()
}

/// `resolve_name`'s verdict (who-is-this.md §6).
pub enum ResolvedName {
    /// The key belongs to a contact — the manual label always wins.
    Petname(String),
    /// Not a contact; what the learned store supports, best first
    /// (highest revision; a genuine tie keeps both, surfaced honestly).
    Learned(Vec<LearnedName>),
    /// Nothing known — the edge renders the key itself.
    Unknown,
}

/// One name the learned store supports, with its provenance.
pub struct LearnedName {
    pub name: String,
    /// The *self-claim's* supersession counter (SPEC §3.2) — orders
    /// conflicting names across answers; 0 for endorsed-only names
    /// (endorsement revisions are the voucher's own counter, a different
    /// scope, never mixed in).
    pub revision: u64,
    /// Petnames of the contacts serving a record with this claim.
    pub held_by: Vec<String>,
    /// The subject itself served a record claiming this name.
    pub confirmed_by_subject: bool,
    /// Petnames of the contacts who *vouch* this name — their own signed
    /// claim, not the subject's (D4a: "your friends call them…").
    pub endorsed_by: Vec<String>,
}

/// One contact's verified link evidence for an unknown key (D3c,
/// multi-device.md §7): the popup's "P says this is their device" line —
/// an offer's provenance, never an instruction.
pub struct DeviceEvidence {
    /// Whose device the evidence says it is.
    pub petname: String,
    /// Vouched-from-trust, or mutually confirmed (the upgrade).
    pub tier: zink_protocol::LinkTier,
}

/// One valid `Negative` about a key, with the observer's verdict (D4b).
pub struct Disavowal {
    pub attester: PublicKey,
    /// The attester rendered (petname / device name / short hex).
    pub attester_label: String,
    /// Whether the MVP policy excludes the key from addressed sets: true
    /// only for this client's own stance or a same-person disavowal;
    /// third-party negatives warn, never exclude.
    pub excludes: bool,
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
        befriend, chain, deposited_envelopes, deposited_frame, dir_bytes, loop_client,
        mailbox_only, mailbox_spec, message, open_homed, routed_record, script_drain, sealed_chain,
        sealed_for, signed_record, spawn_test_relay, summary, temp_key, temp_root,
    };
    use super::*;
    use crate::ports::clock::TestClock;
    use crate::ports::transport::{Home, Loopback, TestTransport};
    use zink_protocol::SyncResponse;

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
    async fn recv__should_drain_the_healthy_relay_when_another_is_unreachable() {
        // Given: a mailbox on two relays, mail waiting on the SECOND only —
        // and the first held silent. Before De6a a `?` in recv's per-relay
        // loop aborted the pass on the first unreachable relay, so this mail
        // stayed invisible until an unrelated relay came back.
        let dead = DeviceKey::from_seed([91; 32]).public();
        let healthy = DeviceKey::from_seed([92; 32]).public();
        let key_path = temp_key("recvpartial", "b");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        let b = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            clock.clone(),
            SystemClock,
            net.clone(),
        );
        let sender = DeviceKey::from_seed([93; 32]);
        let mail = sealed_for(&sender, b.public_key(), b"mail on the relay that stayed up");
        net.dial.hold(&dead);
        script_drain(&net.dial.connect(&healthy), vec![mail]);
        let dead_spec = mailbox_spec(&dead);

        // When: draining both, the dead one first (the abort order that
        // used to lose the mail)
        let relays = [dead_spec.clone(), mailbox_spec(&healthy)];
        let (report, ()) = tokio::join!(b.recv(&relays), async {
            clock.wait_for_sleepers(1).await;
            clock.advance(ClientConfig::default().connect_timeout);
        });

        // Then: the healthy relay's mail arrived, and the failure is
        // reported rather than swallowed — a partial view that says so
        let report = report.expect("partial drain succeeds");
        assert_eq!(report.received.len(), 1);
        assert!(report.received[0].body.is_ok());
        assert_eq!(report.failed.len(), 1, "the dead relay is named");
        assert_eq!(report.failed[0].relay, dead_spec);

        // When: both relays dead — nothing can be drained anywhere
        net.dial.hold(&dead);
        net.dial.hold(&healthy);
        let (result, ()) = tokio::join!(b.recv(&relays), async {
            clock.wait_for_sleepers(2).await;
            clock.advance(ClientConfig::default().connect_timeout);
        },);

        // Then: an error — "best-effort per relay" is not "silently
        // succeed with nothing"
        assert!(result.is_err(), "a drain reaching no relay must fail");

        let _ = std::fs::remove_dir_all(temp_root("recvpartial"));
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
    async fn auto_query__should_learn_an_added_members_record_during_recv() {
        // Given: alice↔bob mutual contacts; bob knows carol; alice's copy of
        // bob's messages goes through her mailbox (bob's record of alice is
        // mailbox-only), so the scoped auto-query fires inside her drain.
        let wire = Loopback::new();
        let (a, a_net, _a_clock) = loop_client("autoquery", "alice", &wire);
        let (b, b_net, _b_clock) = loop_client("autoquery", "bob", &wire);
        let (c, c_net, _c_clock) = loop_client("autoquery", "carol", &wire);
        let relay_a = DeviceKey::from_seed([111; 32]).public();
        let relay_b = DeviceKey::from_seed([112; 32]).public();
        let relay_c = DeviceKey::from_seed([113; 32]).public();
        a.add_contact(&routed_record(b.public_key(), &relay_b), Some("Bob".into()))
            .expect("alice adds bob");
        b.add_contact(
            &ContactRecord::new(
                vec![a.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: mailbox_spec(&relay_a),
                    relay_url: None,
                }],
            ),
            Some("Alice".into()),
        )
        .expect("bob adds alice, mailbox-only");
        b.add_contact(
            &routed_record(c.public_key(), &relay_c),
            Some("Carol".into()),
        )
        .expect("bob adds carol");

        // …bob starts the 1:1 and grows it by replying with carol added
        let ra_hi = b_net.dial.connect(&relay_a);
        ra_hi.reply(deposited_frame());
        let first = b
            .send(
                &[b.resolve_contact("Alice").expect("resolve")],
                b"hi alice".to_vec(),
                vec![],
            )
            .await
            .expect("bob sends");
        let conv = first.conversation;
        let ra_welcome = b_net.dial.connect(&relay_a);
        ra_welcome.reply(deposited_frame());
        let rc_welcome = b_net.dial.connect(&relay_c);
        rc_welcome.reply(deposited_frame());
        let mut contacts = b.reply_contacts(conv).expect("reply contacts").contacts;
        contacts.push(b.resolve_contact("Carol").expect("resolve"));
        b.send_in(conv, &contacts, b"welcome carol".to_vec(), vec![])
            .await
            .expect("bob grows the group");

        // When: alice drains — the scoped auto-query fires inside recv
        // (bob authored, so the conversation is legitimate; carol is an
        // unknown member; bob is the only dialable participant, and his
        // real handler serves carol's record)
        let mut mail = deposited_envelopes(&ra_hi);
        mail.extend(deposited_envelopes(&ra_welcome));
        script_drain(&a_net.dial.connect(&relay_a), mail);
        let report = a.recv(&[mailbox_spec(&relay_a)]).await.expect("alice recv");
        assert!(
            report
                .received
                .iter()
                .any(|received| received.body.as_deref() == Ok(b"welcome carol".as_slice()))
        );

        // Then: carol's record was learned with zero manual identity work
        assert!(
            !a.state.learned(&c.public_key()).is_empty(),
            "the auto-query should have learned carol's record from bob"
        );

        // …and reply-to-all reaches carol through the auto-learned route
        let rc_reply = a_net.dial.connect(&relay_c);
        rc_reply.reply(deposited_frame());
        let reply = a.reply_contacts(conv).expect("alice reply contacts");
        assert!(
            reply.unknown.is_empty(),
            "carol resolves via the learned record"
        );
        a.send_in(conv, &reply.contacts, b"hello everyone".to_vec(), vec![])
            .await
            .expect("alice replies to all");
        script_drain(
            &c_net.dial.connect(&relay_c),
            deposited_envelopes(&rc_reply),
        );
        let carol_got = c.recv(&[mailbox_spec(&relay_c)]).await.expect("carol recv");
        assert!(
            carol_got
                .received
                .iter()
                .any(|received| received.body.as_deref() == Ok(b"hello everyone".as_slice()))
        );

        let _ = std::fs::remove_dir_all(temp_root("autoquery"));
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
    async fn who_is__should_learn_a_record_from_a_contact_without_touching_the_contact_store() {
        // Given: B (homed, serving) holds Carol's record; A holds B as a
        // dialable contact and is in B's contact store. Carol herself is
        // unknown to A and offline — the one-way-add shape (design §1).
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("learn", "asker", &wire);
        let (b, _b_net, _b_clock) = loop_client("learn", "responder", &wire);
        befriend(&b.state, a.public_key()); // B's gate serves A
        let carol = DeviceKey::from_seed([21; 32]);
        let carol_record = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "cc@203.0.113.9:9".to_string(),
                relay_url: Some("http://203.0.113.9:10".to_string()),
            }],
        );
        b.state.save_contact("carol", &carol_record).expect("save");
        let b_record = ContactRecord::new(
            vec![b.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "unused@203.0.113.1:1".to_string(),
                relay_url: Some("http://203.0.113.1:1".to_string()),
            }],
        );
        a.add_contact(&b_record, Some("bob".to_string()))
            .expect("add bob");
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("learn", "asker")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);

        // When
        let outcome = a.who_is(carol.public()).await.expect("who_is");
        let answers = outcome.answers;

        // Then: one contact-served answer, persisted with provenance; the
        // honest denominator says so; the contact store byte-identical
        assert_eq!(answers.len(), 1);
        assert_eq!((outcome.asked, outcome.unreachable), (1, 0));
        assert_eq!(answers[0].responder_petname, "bob");
        assert_eq!(answers[0].record, carol_record);
        assert_eq!(dir_bytes(&contacts_dir), before);
        let ResolvedName::Learned(names) = a.resolve_name(carol.public()).expect("resolve") else {
            panic!("expected a learned name");
        };
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].name, "Carol");
        assert_eq!(names[0].held_by, vec!["bob".to_string()]);
        assert!(!names[0].confirmed_by_subject);

        // When: promoted by the one explicit act — reply becomes possible
        let petname = a.add_contact(&answers[0].record, None).expect("promote");

        // Then: petname prefilled from the self-claim; keys + relays ready
        assert_eq!(petname, "Carol");
        let contact = a.resolve_contact("Carol").expect("resolve contact");
        assert_eq!(contact.keys, vec![carol.public()]);
        assert_eq!(contact.relays, vec!["cc@203.0.113.9:9".to_string()]);

        let _ = std::fs::remove_dir_all(temp_root("learn"));
    }

    #[tokio::test]
    async fn who_is__the_subjects_own_answer_should_win_relay_resolution() {
        // Given: Carol is A's contact via a *stale* record (right relay
        // URL, outdated mailbox); Carol is online with a fresh profile and
        // serves A (the record-freshness case, design §7)
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("fresh", "asker", &wire);
        let (c, _c_net, _c_clock) = loop_client("fresh", "carol", &wire);
        // Carol's fresh profile — written straight to state, as `open_homed`
        // did (`build_own_record` reads it when she answers for herself).
        c.state
            .save_profile(
                "carol",
                &[RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            )
            .expect("save profile");
        befriend(&c.state, a.public_key());
        let stale = ContactRecord::new(
            vec![c.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "stale@203.0.113.1:1".to_string(),
                relay_url: Some("http://203.0.113.1:1".to_string()),
            }],
        );
        a.add_contact(&stale, Some("carol".to_string()))
            .expect("add carol");

        // When
        let answers = a.who_is(c.public_key()).await.expect("who_is").answers;

        // Then: the subject's own answer wins relay resolution; the stored
        // record is untouched (freshness is read-time, never a mutation)
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].responder, c.public_key());
        let contact = a.resolve_contact("carol").expect("resolve");
        assert_eq!(
            contact.relays,
            vec!["unused@203.0.113.1:1".to_string()],
            "fresh mailbox from the subject-served answer"
        );
        assert_eq!(a.contacts().expect("contacts")[0].1, stale);

        let _ = std::fs::remove_dir_all(temp_root("fresh"));
    }

    #[tokio::test]
    async fn resolve_contact__should_take_keys_from_the_stored_record_only() {
        // Given: carol stored with relay X; a subject-served learned record
        // with relay Y and a smuggled extra key; *newer* contact-served
        // hearsay with relay Z
        let a = Client::open_or_create(&temp_key("keys", "asker"))
            .await
            .expect("open A");
        let carol = DeviceKey::from_seed([22; 32]);
        let extra = DeviceKey::from_seed([23; 32]).public();
        let stored = ContactRecord::new(
            vec![carol.public()],
            vec![],
            vec![RelayEntry {
                mailbox: "xx@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        a.add_contact(&stored, Some("carol".to_string()))
            .expect("add");
        let served = ContactRecord::new(
            vec![carol.public(), extra],
            vec![],
            vec![RelayEntry {
                mailbox: "yy@203.0.113.2:2".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(&carol.public(), &carol.public(), &served, &[], 1)
            .expect("learn subject-served");
        let hearsay = ContactRecord::new(
            vec![carol.public()],
            vec![],
            vec![RelayEntry {
                mailbox: "zz@203.0.113.3:3".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([24; 32]).public(),
                &hearsay,
                &[],
                2,
            )
            .expect("learn hearsay");

        // When
        let contact = a.resolve_contact("carol").expect("resolve");

        // Then: subject-served relays beat newer hearsay; sealing keys come
        // strictly from the user-added record — the smuggled key is inert
        assert_eq!(contact.relays, vec!["yy@203.0.113.2:2".to_string()]);
        assert_eq!(contact.keys, vec![carol.public()]);

        let _ = std::fs::remove_dir_all(temp_root("keys"));
    }

    #[tokio::test]
    async fn add_contact__should_update_the_overlapping_contact_under_its_own_petname() {
        // Given: bob stored under his original single-key record
        let a = Client::open_or_create(&temp_key("overlap-update", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([31; 32]);
        let laptop = DeviceKey::from_seed([32; 32]);
        let original =
            ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1"));
        a.add_contact(&original, Some("bob".to_string()))
            .expect("add");

        // When: a re-scan with the key set extended and *reordered* — a new
        // first key, so the store stem must re-derive without forking
        let rescanned = ContactRecord::new(
            vec![laptop.public(), bob.public()],
            vec![],
            mailbox_only("bb@203.0.113.2:2"),
        );
        a.add_contact(&rescanned, Some("bob".to_string()))
            .expect("update");

        // Then: still exactly one contact, holding the fresh record
        let contacts = a.contacts().expect("contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].0, "bob");
        assert_eq!(contacts[0].1, rescanned);

        let _ = std::fs::remove_dir_all(temp_root("overlap-update"));
    }

    #[tokio::test]
    async fn add_contact__should_surface_an_overlap_under_a_different_petname() {
        // Given: bob stored; a hostile record smuggling bob's key into its
        // own key list (multi-device.md §4 — the trust-anchor hijack)
        let a = Client::open_or_create(&temp_key("overlap-confirm", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([33; 32]);
        let mallory = DeviceKey::from_seed([34; 32]);
        a.add_contact(
            &ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1")),
            Some("bob".to_string()),
        )
        .expect("add bob");
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("overlap-confirm", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);
        let smuggling = ContactRecord::new(
            vec![mallory.public(), bob.public()],
            vec![],
            mailbox_only("mm@203.0.113.6:6"),
        );

        // When: added as "someone new"
        let result = a.add_contact(&smuggling, Some("mallory".to_string()));

        // Then: surfaced, naming the entry it would rewrite; nothing stored
        assert!(matches!(
            result,
            Err(Error::ContactOverlap { ref existing }) if existing == "bob"
        ));
        assert_eq!(dir_bytes(&contacts_dir), before);

        // And: the same add under the matched petname is the explicit
        // confirm — it updates bob's entry, keeping one contact
        a.add_contact(&smuggling, Some("bob".to_string()))
            .expect("confirmed update");
        let contacts = a.contacts().expect("contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].0, "bob");

        let _ = std::fs::remove_dir_all(temp_root("overlap-confirm"));
    }

    #[tokio::test]
    async fn add_contact__should_refuse_a_record_overlapping_two_contacts() {
        // Given: bob and carol stored as distinct contacts
        let a = Client::open_or_create(&temp_key("overlap-ambiguous", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([35; 32]);
        let carol = DeviceKey::from_seed([36; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("overlap-ambiguous", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);
        let spanning = ContactRecord::new(
            vec![bob.public(), carol.public()],
            vec![],
            mailbox_only("xx@203.0.113.7:7"),
        );

        // When / Then: refused under any petname — even a matching one —
        // and the store is untouched
        for petname in ["dana", "bob"] {
            assert!(matches!(
                a.add_contact(&spanning, Some(petname.to_string())),
                Err(Error::AmbiguousOverlap(_))
            ));
        }
        assert_eq!(dir_bytes(&contacts_dir), before);

        let _ = std::fs::remove_dir_all(temp_root("overlap-ambiguous"));
    }

    #[tokio::test]
    async fn add_contact__should_reject_a_petname_collision_without_key_overlap() {
        // Given: bob stored; an unrelated record wanting the same petname
        let a = Client::open_or_create(&temp_key("overlap-collision", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([37; 32]);
        let other = DeviceKey::from_seed([38; 32]);
        a.add_contact(
            &ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1")),
            Some("bob".to_string()),
        )
        .expect("add bob");

        // When / Then: no shared key = no identity evidence — rejected
        assert!(matches!(
            a.add_contact(
                &ContactRecord::new(
                    vec![other.public()],
                    vec![],
                    mailbox_only("oo@203.0.113.2:2"),
                ),
                Some("bob".to_string()),
            ),
            Err(Error::PetnameCollision(_))
        ));

        let _ = std::fs::remove_dir_all(temp_root("overlap-collision"));
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
    async fn recognize__should_serve_the_recognized_device_like_self_one_way() {
        // Given: A holds a full conversation, B only its tip — and the
        // mirror image for the reverse direction. Neither is the other's
        // contact; recognition is the only thing that will open the gate.
        let a = Client::open_or_create(&temp_key("recognize-gate", "a"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("recognize-gate", "b"))
            .await
            .expect("open B");
        let held_by_a = chain(&DeviceKey::from_seed([42; 32]), a.public_key(), 3);
        let conv_a = held_by_a[0].id();
        for envelope in &held_by_a {
            a.state.store_envelope(conv_a, envelope).unwrap();
        }
        b.state
            .store_envelope(conv_a, held_by_a.last().unwrap())
            .unwrap();
        let held_by_b = chain(&DeviceKey::from_seed([43; 32]), b.public_key(), 3);
        let conv_b = held_by_b[0].id();
        for envelope in &held_by_b {
            b.state.store_envelope(conv_b, envelope).unwrap();
        }
        a.state
            .store_envelope(conv_b, held_by_b.last().unwrap())
            .unwrap();

        // When: B pulls as a stranger
        let refused = b
            .backfill_addr(conv_a, a.transport.peer())
            .await
            .expect("declined, not an error");

        // Then
        assert_eq!(refused, 0, "unrecognized and no contact — nothing served");

        // When: A recognizes B — one signed act, the shown side passive
        a.recognize_device(&ContactRecord::new(
            vec![b.public_key()],
            vec![],
            mailbox_only("bb@203.0.113.5:5"),
        ))
        .expect("recognize");
        let served = b
            .backfill_addr(conv_a, a.transport.peer())
            .await
            .expect("served");

        // Then: B is served like self…
        assert_eq!(served, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conv_a).is_ok());

        // …while the reverse direction stays closed
        let reverse = a
            .backfill_addr(conv_b, b.transport.peer())
            .await
            .expect("declined");
        assert_eq!(reverse, 0, "recognition moved nothing the other way");

        // When: B recognizes A back (the usual two-way pairing)
        b.recognize_device(&ContactRecord::new(
            vec![a.public_key()],
            vec![],
            mailbox_only("aa@203.0.113.6:6"),
        ))
        .expect("recognize back");
        let reverse = a
            .backfill_addr(conv_b, b.transport.peer())
            .await
            .expect("served");

        // Then
        assert_eq!(reverse, 2);

        let _ = std::fs::remove_dir_all(temp_root("recognize-gate"));
    }

    #[tokio::test]
    async fn who_is__should_serve_a_recognized_devices_record_to_a_contact() {
        // Given: A recognized its (offline) laptop. The laptop's record is
        // servable by nobody else — its own contact store is empty and A
        // holds it in the own-devices store, not the contact store — so
        // the §6 mirror rule is the only path an observer has to it.
        let a = Client::open_or_create(&temp_key("recognize-whois", "a"))
            .await
            .expect("open A");
        let c = Client::open_or_create(&temp_key("recognize-whois", "c"))
            .await
            .expect("open C");
        let laptop = DeviceKey::from_seed([44; 32]);
        let laptop_record = signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        );
        a.recognize_device(&laptop_record).expect("recognize");

        // When: a stranger asks about the laptop's key
        let connection = net::connect_peer(
            &c.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            c.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let stranger = net::sync_request(
            &connection,
            SyncOp::WhoIs {
                key: laptop.public(),
            },
        )
        .await
        .expect("round-trip");

        // Then: nothing — the gate is unchanged for strangers
        assert_eq!(stranger, SyncResult::NotHeld);

        // When: the same requester asks as a contact (fresh connection —
        // the gate resolves per connection)
        befriend(&a.state, c.public_key());
        let connection = net::connect_peer(
            &c.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            c.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let known = net::sync_request(
            &connection,
            SyncOp::WhoIs {
                key: laptop.public(),
            },
        )
        .await
        .expect("round-trip");

        // Then: the recognized device's stored record, verbatim
        assert_eq!(
            known,
            SyncResult::Known {
                record: Box::new(laptop_record),
                endorsements: vec![],
            }
        );

        let _ = std::fs::remove_dir_all(temp_root("recognize-whois"));
    }

    #[tokio::test]
    async fn recognize__should_put_the_vouch_in_my_record_and_nowhere_else() {
        // Given: a profiled phone and its laptop's record
        let a = Client::open_or_create(&temp_key("recognize-vouch", "a"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("mårten phone", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let laptop = DeviceKey::from_seed([45; 32]);
        let laptop_record = signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        );

        // When
        a.recognize_device(&laptop_record).expect("recognize");

        // Then: my_record vouches the laptop — an observer trusting A's
        // key tiers the laptop as offerable (the D3a evaluation)…
        let my = a.my_record().expect("record");
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &my.attestations),
            zink_protocol::LinkTier::VouchedFromTrust
        );
        // …while the laptop's record carries only its own (zero) vouches
        assert_eq!(
            zink_protocol::link_tier(
                &[laptop.public()],
                a.public_key(),
                &laptop_record.attestations
            ),
            zink_protocol::LinkTier::None
        );

        // And: once the laptop runs its own act back, an observer holding
        // both records sees the upgrade — aggregation across records is
        // exactly the observer's job (multi-device.md §4)
        let laptop_vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: laptop.public(),
                subject: laptop.public(),
                claim: Claim::SamePersonAs(a.public_key()),
                revision: 0,
            },
            &laptop,
        );
        let mut held = my.attestations.clone();
        held.push(laptop_vouch);
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &held),
            zink_protocol::LinkTier::MutuallyConfirmed
        );

        // And: the recognition persists across a reopen
        a.close().await;
        let a = Client::open(&temp_key("recognize-vouch", "a"))
            .await
            .expect("reopen");
        assert_eq!(a.recognized_devices().len(), 1);
        assert_eq!(
            zink_protocol::link_tier(
                &[a.public_key()],
                laptop.public(),
                &a.my_record().expect("record").attestations
            ),
            zink_protocol::LinkTier::VouchedFromTrust
        );

        let _ = std::fs::remove_dir_all(temp_root("recognize-vouch"));
    }

    #[tokio::test]
    async fn device_evidence__should_tier_from_held_records() {
        // Given: P is a contact whose record vouches an unknown key
        let a = Client::open_or_create(&temp_key("evidence", "a"))
            .await
            .expect("open");
        let p = DeviceKey::from_seed([52; 32]);
        let laptop = DeviceKey::from_seed([53; 32]);
        let vouch = |attester: &DeviceKey, linked: PublicKey| {
            SignedAttestation::new(
                Attestation {
                    version: Attestation::CURRENT,
                    attester: attester.public(),
                    subject: attester.public(),
                    claim: Claim::SamePersonAs(linked),
                    revision: 0,
                },
                attester,
            )
        };
        let p_record = ContactRecord::new(
            vec![p.public()],
            vec![vouch(&p, laptop.public())],
            mailbox_only("pp@203.0.113.1:1"),
        );
        a.add_contact(&p_record, Some("p".to_string()))
            .expect("add");

        // Then: the one-way tier — offerable, labeled as P's claim
        let evidence = a.device_evidence(laptop.public()).expect("evidence");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].petname, "p");
        assert_eq!(evidence[0].tier, zink_protocol::LinkTier::VouchedFromTrust);

        // When: the laptop's own record — learned via the auto-query —
        // carries the reverse vouch
        let laptop_record = ContactRecord::new(
            vec![laptop.public()],
            vec![vouch(&laptop, p.public())],
            mailbox_only("ll@203.0.113.2:2"),
        );
        a.state
            .save_learned(&laptop.public(), &p.public(), &laptop_record, &[], 1)
            .expect("learn");

        // Then: upgraded to mutually confirmed
        assert_eq!(
            a.device_evidence(laptop.public()).expect("evidence")[0].tier,
            zink_protocol::LinkTier::MutuallyConfirmed
        );

        // And: the spoof direction — a stranger's record claiming P's key —
        // is no evidence at all
        let stranger = DeviceKey::from_seed([54; 32]);
        let spoof = ContactRecord::new(
            vec![stranger.public()],
            vec![vouch(&stranger, p.public())],
            mailbox_only("ss@203.0.113.3:3"),
        );
        a.state
            .save_learned(&stranger.public(), &p.public(), &spoof, &[], 2)
            .expect("learn");
        assert!(
            a.device_evidence(stranger.public())
                .expect("evidence")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_root("evidence"));
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

    #[test]
    fn valid_endorsements__should_keep_only_the_responders_own_claims() {
        // Given: responder R answering about subject S — a genuine vouch,
        // a relayed third-party vouch, a forged one, and one about a
        // different subject
        let responder = DeviceKey::from_seed([80; 32]);
        let third_party = DeviceKey::from_seed([81; 32]);
        let subject = DeviceKey::from_seed([82; 32]).public();
        let other = DeviceKey::from_seed([83; 32]).public();
        let claim = |attester: &DeviceKey, about: PublicKey, signer: &DeviceKey| {
            SignedAttestation::new(
                Attestation {
                    version: Attestation::CURRENT,
                    attester: attester.public(),
                    subject: about,
                    claim: Claim::Name("Carol".to_string()),
                    revision: 0,
                },
                signer,
            )
        };
        let genuine = claim(&responder, subject, &responder);
        let relayed = claim(&third_party, subject, &third_party); // hop-2 gossip
        let forged = claim(&responder, subject, &third_party);
        let off_subject = claim(&responder, other, &responder);

        // When
        let kept = valid_endorsements(
            responder.public(),
            subject,
            vec![relayed, forged, off_subject, genuine.clone()],
        );

        // Then: only the responder's own, correctly-subjected, verified claim
        assert_eq!(kept, vec![genuine]);
    }

    #[tokio::test]
    async fn vouch__should_persist_and_supersede_per_revision() {
        // Given
        let a = Client::open_or_create(&temp_key("vouch", "a"))
            .await
            .expect("open");
        let carol = DeviceKey::from_seed([84; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![carol.public()],
                vec![],
                mailbox_only("cc@203.0.113.1:1"),
            ),
            Some("Carrie".to_string()),
        )
        .expect("add");

        // When / Then: the explicit act signs at revision 0; a re-vouch
        // supersedes; withdrawal removes; a non-contact errors
        assert!(!a.vouches(&carol.public()));
        a.vouch("Carrie").expect("vouch");
        let first = a.state.vouch_for(&carol.public()).expect("stored");
        assert_eq!(first.attestation.revision, 0);
        assert_eq!(first.attestation.claim, Claim::Name("Carrie".to_string()));
        assert_eq!(first.verify(), Ok(()));
        a.vouch("Carrie").expect("re-vouch");
        assert_eq!(
            a.state
                .vouch_for(&carol.public())
                .expect("stored")
                .attestation
                .revision,
            1
        );
        a.unvouch("Carrie").expect("unvouch");
        assert!(!a.vouches(&carol.public()));
        assert!(matches!(a.vouch("nobody"), Err(Error::NotAContact(_))));

        let _ = std::fs::remove_dir_all(temp_root("vouch"));
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
    async fn repudiate__should_supersede_the_vouch_and_unrecognize_the_sibling() {
        // Given: a profiled phone that vouched a contact and recognized a
        // laptop
        let a = Client::open_or_create(&temp_key("repudiate", "a"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("mårten phone", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let carol = DeviceKey::from_seed([90; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![carol.public()],
                vec![],
                mailbox_only("cc@203.0.113.1:1"),
            ),
            Some("Carrie".to_string()),
        )
        .expect("add");
        a.vouch("Carrie").expect("vouch");
        let laptop = DeviceKey::from_seed([91; 32]);
        a.recognize_device(&signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        ))
        .expect("recognize");
        let old_record = a.my_record().expect("record");

        // When: both get repudiated
        a.repudiate(carol.public()).expect("repudiate carol");
        a.repudiate(laptop.public()).expect("repudiate laptop");

        // Then: the negative supersedes the vouch (rev 1 over rev 0), the
        // sibling is un-recognized, and the fresh record publishes both
        let carol_stance = a.state.vouch_for(&carol.public()).expect("stance");
        assert!(matches!(carol_stance.attestation.claim, Claim::Negative));
        assert_eq!(carol_stance.attestation.revision, 1);
        assert!(a.recognized_devices().is_empty());
        let fresh = a.my_record().expect("record");
        assert_eq!(
            fresh
                .attestations
                .iter()
                .filter(|signed| matches!(signed.attestation.claim, Claim::Negative))
                .count(),
            2
        );
        // …an observer combining the OLD record (live link) with the fresh
        // negatives sees the device link voided
        let mut held = old_record.attestations.clone();
        held.extend(fresh.attestations.clone());
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &held),
            zink_protocol::LinkTier::None
        );
        // …and a yet-higher re-vouch restores the contact's name stance
        a.vouch("Carrie").expect("re-vouch");
        let restored = a.state.vouch_for(&carol.public()).expect("stance");
        assert_eq!(restored.attestation.revision, 2);
        assert!(matches!(restored.attestation.claim, Claim::Name(_)));

        let _ = std::fs::remove_dir_all(temp_root("repudiate"));
    }

    #[tokio::test]
    async fn disavowals__should_exclude_same_person_only_and_void_endorsed_names() {
        // Given: bob's learned endorsements about carol carry a vouch
        // superseded by his own negative — but bob and carol share no
        // entry and no link: a third-party claim
        let a = Client::open_or_create(&temp_key("disavow", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([92; 32]);
        let carol = DeviceKey::from_seed([93; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let endorse = |claim: Claim, revision: u64| {
            SignedAttestation::new(
                Attestation {
                    version: Attestation::CURRENT,
                    attester: bob.public(),
                    subject: carol.public(),
                    claim,
                    revision,
                },
                &bob,
            )
        };
        let carol_record = signed_record(&carol, "Carol", 0, mailbox_only("cc@203.0.113.2:2"));
        a.state
            .save_learned(
                &carol.public(),
                &bob.public(),
                &carol_record,
                &[
                    endorse(Claim::Name("Caroline".to_string()), 0),
                    endorse(Claim::Negative, 1),
                ],
                1,
            )
            .expect("learn");

        // Then: the endorsed name is voided by its attester's negative…
        let candidates = a.learned_candidates(carol.public()).expect("candidates");
        assert!(
            candidates
                .iter()
                .all(|(name, _)| name.endorsed_by.is_empty()),
            "a name behind the attester's higher negative must not render"
        );
        // …the disavowal renders with WHO — but as third-party it never
        // excludes (the griefing bound, web-of-trust.md §7)
        let disavowals = a.disavowals(carol.public()).expect("disavowals");
        assert_eq!(disavowals.len(), 1);
        assert_eq!(disavowals[0].attester_label, "bob");
        assert!(!disavowals[0].excludes);

        let _ = std::fs::remove_dir_all(temp_root("disavow"));
    }

    #[tokio::test]
    async fn repudiation__should_stop_replies_to_the_lost_device_after_a_pull() {
        // Given: the lost-device drill (web-of-trust.md §5.1). The phone
        // paired a laptop (the laptop's record carries its reverse link —
        // the same-person evidence alice will hold); alice has both as
        // contact entries and a conversation whose membership carries all
        // three keys via the phone's send-to-self.
        let wire = Loopback::new();
        let (phone, _p_net, _p_clock) = loop_client("drill", "phone", &wire);
        let (alice, _a_net, _a_clock) = loop_client("drill", "alice", &wire);
        // The phone's profile — `my_record` reads it (as `open_homed` wrote).
        phone
            .state
            .save_profile(
                "phone",
                &[RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            )
            .expect("save profile");
        befriend(&phone.state, alice.public_key()); // alice is served by the phone
        let laptop = DeviceKey::from_seed([94; 32]);
        let laptop_link = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: laptop.public(),
                subject: laptop.public(),
                claim: Claim::SamePersonAs(phone.public_key()),
                revision: 0,
            },
            &laptop,
        );
        let mut laptop_record = signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        );
        laptop_record.attestations.push(laptop_link);
        phone.recognize_device(&laptop_record).expect("recognize");
        let to_alice = vec![Contact {
            keys: vec![alice.public_key()],
            relays: vec!["aa@203.0.113.9:9".to_string()],
        }];
        // The relay route is fake — the send queues, but the sealed core
        // (with the laptop appended) is stored; hand it to alice directly.
        let result = phone.send(&to_alice, b"hi".to_vec(), vec![]).await;
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        let conversation = phone.state.conversations()[0];
        for envelope in phone.state.load_envelopes(conversation).expect("stored") {
            alice
                .state
                .store_envelope(conversation, &envelope)
                .expect("copy");
        }
        alice
            .add_contact(
                &phone.my_record().expect("record"),
                Some("mårten".to_string()),
            )
            .expect("add phone");
        alice.add_contact(&laptop_record, None).expect("add laptop");

        // Baseline: a reply addresses both of mårten's keys
        let baseline = alice.reply_contacts(conversation).expect("reply");
        assert_eq!(baseline.contacts.len(), 2);
        assert!(baseline.disavowed.is_empty());

        // When: the phone repudiates the lost laptop; alice's next
        // freshness pull on the phone brings the fresh record
        phone.repudiate(laptop.public()).expect("repudiate");
        let outcome = alice.who_is(phone.public_key()).await.expect("pull");
        assert!(!outcome.answers.is_empty());

        // Then: the laptop drops out of the addressed set — the accepted
        // disavowal is the deliberate stop-include — and renders with WHO
        let after = alice.reply_contacts(conversation).expect("reply");
        assert_eq!(after.contacts.len(), 1);
        assert_eq!(after.disavowed, vec![laptop.public()]);
        let disavowals = alice.disavowals(laptop.public()).expect("disavowals");
        assert_eq!(disavowals.len(), 1);
        assert!(disavowals[0].excludes);
        assert_eq!(disavowals[0].attester_label, "mårten");
        // …while the explicit act survives everything: sending to the
        // entry by name is the manual override
        assert!(alice.resolve_contact("mårten laptop").is_ok());

        let _ = std::fs::remove_dir_all(temp_root("drill"));
    }

    #[tokio::test]
    async fn who_is__should_carry_the_responders_vouch_as_an_endorsement() {
        // Given: B holds Carol's record and A as a contact; A holds B as
        // a dialable contact ("bob")
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("endorse", "asker", &wire);
        let (b, _b_net, _b_clock) = loop_client("endorse", "responder", &wire);
        befriend(&b.state, a.public_key());
        let b_record = ContactRecord::new(
            vec![b.public_key()],
            vec![],
            vec![RelayEntry {
                mailbox: "bb@203.0.113.2:2".to_string(),
                relay_url: Some("http://203.0.113.1:1".to_string()),
            }],
        );
        a.add_contact(&b_record, Some("bob".to_string()))
            .expect("add bob");
        let carol = DeviceKey::from_seed([85; 32]);
        let carol_record = signed_record(&carol, "Carol", 0, mailbox_only("cc@203.0.113.9:9"));
        b.add_contact(&carol_record, Some("Carrie".to_string()))
            .expect("add carol");

        // When: A asks before B has vouched
        let outcome = a.who_is(carol.public()).await.expect("who_is");

        // Then: an answer, but no endorsement — nothing auto-broadcasts
        assert_eq!(outcome.answers.len(), 1);
        assert!(outcome.answers[0].endorsements.is_empty());

        // When: B vouches (the explicit act) and A re-asks
        b.vouch("Carrie").expect("vouch");
        let outcome = a.who_is(carol.public()).await.expect("who_is");

        // Then: the endorsement rides the answer; the ranking shows the
        // endorsed name with its voucher, below the verified self-claim
        assert_eq!(outcome.answers[0].endorsements.len(), 1);
        let candidates = a.learned_candidates(carol.public()).expect("candidates");
        let carrie = candidates
            .iter()
            .find(|(name, _)| name.name == "Carrie")
            .expect("endorsed name surfaced");
        assert_eq!(carrie.0.endorsed_by, vec!["bob".to_string()]);
        assert!(carrie.0.held_by.is_empty() && !carrie.0.confirmed_by_subject);
        assert_eq!(
            candidates[0].0.name, "Carol",
            "the self-claimed name outranks the endorsed-only one"
        );

        // When: B withdraws and A re-asks — the per-responder entry
        // replaces wholesale, endorsements included
        b.unvouch("Carrie").expect("unvouch");
        let _ = a.who_is(carol.public()).await.expect("who_is");

        // Then: the withdrawn vouch is gone from the ranking
        let candidates = a.learned_candidates(carol.public()).expect("candidates");
        assert!(
            candidates
                .iter()
                .all(|(name, _)| name.endorsed_by.is_empty()),
            "withdrawal propagates by replacement"
        );

        let _ = std::fs::remove_dir_all(temp_root("endorse"));
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
    async fn who_is__should_dial_contacts_concurrently_not_serially() {
        // Given: one responder that answers and three whose dials hang
        // (the exact field stall diagnosed at D1's close). Every deadline
        // is the TestClock's, so serial dials would park one timer at a
        // time — parking all three at once IS the concurrency assertion.
        let key_path = temp_key("conc", "asker");
        keystore::create(&key_path).expect("create key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        let responder = DeviceKey::from_seed([39; 32]).public();
        net.dial.connect(&responder).reply(
            SyncResponse::new(SyncResult::Known {
                record: Box::new(ContactRecord::new(vec![responder], vec![], vec![])),
                endorsements: vec![],
            })
            .to_bytes(),
        );
        for n in 0..3u8 {
            net.dial.hold(&DeviceKey::from_seed([40 + n; 32]).public());
        }
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            clock.clone(),
            SystemClock,
            net.clone(),
        );
        let dialable = |key: PublicKey, host: u8| {
            ContactRecord::new(
                vec![key],
                vec![],
                vec![RelayEntry {
                    mailbox: format!("unused@203.0.113.{host}:1"),
                    relay_url: Some(format!("http://203.0.113.{host}:1")),
                }],
            )
        };
        a.add_contact(&dialable(responder, 1), Some("bob".to_string()))
            .expect("add bob");
        for n in 0..3u8 {
            a.add_contact(
                &dialable(DeviceKey::from_seed([40 + n; 32]).public(), n + 2),
                Some(format!("offline{n}")),
            )
            .expect("add offline contact");
        }

        // When: asking about the responder's key (it answers with its
        // self-record); time moves only after all three doomed dials are
        // parked together
        let (outcome, ()) = tokio::join!(a.who_is(responder), async {
            clock.wait_for_sleepers(3).await;
            clock.advance(WHO_IS_DIAL_CAP);
        });

        // Then: the answer arrived and the three dead dials are counted
        // honestly
        let outcome = outcome.expect("who_is");
        assert_eq!(outcome.answers.len(), 1);
        assert_eq!((outcome.asked, outcome.unreachable), (4, 3));

        let _ = std::fs::remove_dir_all(temp_root("conc"));
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
    async fn auto_who_is__should_learn_unknown_members_from_the_conversations_participants() {
        // Given: B (A's contact, homed, serving) authored a conversation
        // that includes the unknown key C, and holds C's record. The drain
        // hands A the message — nothing else happens manually.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("autowho", "asker", &wire);
        let (b, _b_net, _b_clock) = loop_client("autowho", "responder", &wire);
        befriend(&b.state, a.public_key());
        let carol = DeviceKey::from_seed([31; 32]);
        let carol_record = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "cc@203.0.113.9:9".to_string(),
                relay_url: Some("http://203.0.113.9:10".to_string()),
            }],
        );
        b.state.save_contact("carol", &carol_record).expect("save");
        a.add_contact(
            &ContactRecord::new(
                vec![b.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            ),
            Some("bob".to_string()),
        )
        .expect("add bob");
        let genesis = message(
            &b.device,
            vec![a.public_key(), carol.public()],
            None,
            vec![],
            0,
            0,
        );
        let conversation = genesis.id();
        a.state
            .store_envelope(conversation, &genesis)
            .expect("store");
        let received = [Received {
            envelope: genesis.clone(),
            relay: None,
            body: Ok(vec![]),
        }];

        // When
        a.auto_who_is(&received).await;

        // Then: C resolves with provenance — learned from a participant,
        // with zero manual action
        let ResolvedName::Learned(names) = a.resolve_name(carol.public()).expect("resolve") else {
            panic!("expected a learned candidate");
        };
        assert_eq!(names[0].name, "Carol");
        assert_eq!(names[0].held_by, vec!["bob".to_string()]);

        // And: the rate limit holds — wipe what was learned and re-run;
        // nothing is re-asked this run
        let learned_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("autowho", "asker")))
                .join("learned");
        std::fs::remove_dir_all(&learned_dir).expect("wipe learned");
        a.auto_who_is(&received).await;
        assert!(
            matches!(
                a.resolve_name(carol.public()).expect("resolve"),
                ResolvedName::Unknown
            ),
            "a second drain must not re-broadcast the query"
        );

        let _ = std::fs::remove_dir_all(temp_root("autowho"));
    }

    #[tokio::test]
    async fn auto_who_is__should_stay_silent_without_a_contributing_contact() {
        // Given: a conversation authored only by a stranger — presence of
        // A in `recipients` is the spammer-controlled part (groups.md §6)
        let a = Client::open_or_create(&temp_key("nogate", "asker"))
            .await
            .expect("open");
        let stranger = DeviceKey::from_seed([32; 32]);
        let carol = DeviceKey::from_seed([33; 32]).public();
        let genesis = message(&stranger, vec![a.public_key(), carol], None, vec![], 0, 0);
        let conversation = genesis.id();
        a.state
            .store_envelope(conversation, &genesis)
            .expect("store");

        // When
        a.auto_who_is(&[Received {
            envelope: genesis.clone(),
            relay: None,
            body: Ok(vec![]),
        }])
        .await;

        // Then: gated before the rate limit — no query was even recorded
        assert!(a.queried.lock().expect("lock").is_empty());
        assert!(matches!(
            a.resolve_name(carol).expect("resolve"),
            ResolvedName::Unknown
        ));

        let _ = std::fs::remove_dir_all(temp_root("nogate"));
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
    async fn learned_candidates__should_pair_each_name_with_the_freshest_record() {
        // Given: two responders serve the same claimed name, but with
        // different records — the later receipt carries fresher relays
        let a = Client::open_or_create(&temp_key("cands", "asker"))
            .await
            .expect("open");
        let carol = DeviceKey::from_seed([25; 32]);
        let older = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "old@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        let newer = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "new@203.0.113.2:2".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([26; 32]).public(),
                &older,
                &[],
                1,
            )
            .expect("learn older");
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([27; 32]).public(),
                &newer,
                &[],
                2,
            )
            .expect("learn newer");

        // When
        let candidates = a.learned_candidates(carol.public()).expect("candidates");

        // Then: one group (agreement of two), paired with the freshest
        // record — the promotable payload
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0.name, "Carol");
        assert_eq!(candidates[0].0.held_by.len(), 2);
        assert_eq!(candidates[0].1.relays[0].mailbox, "new@203.0.113.2:2");

        let _ = std::fs::remove_dir_all(temp_root("cands"));
    }

    #[tokio::test]
    async fn dismiss__should_persist_across_reopens() {
        // Given
        let key_path = temp_key("dismiss", "me");
        let a = Client::open_or_create(&key_path).await.expect("open");
        let noisy = DeviceKey::from_seed([28; 32]).public();

        // When
        a.dismiss(noisy).expect("dismiss");
        a.dismiss(noisy).expect("idempotent");
        drop(a);
        let a = Client::open_or_create(&key_path).await.expect("reopen");

        // Then
        assert!(a.dismissed().contains(&noisy));

        let _ = std::fs::remove_dir_all(temp_root("dismiss"));
    }

    #[tokio::test]
    async fn resolve_name__should_rank_by_revision_and_group_agreement() {
        // Given: two responders hold Carol's old name (revision 0), one
        // holds the rename (revision 1) — a rename caught mid-propagation
        let a = Client::open_or_create(&temp_key("names", "asker"))
            .await
            .expect("open A");
        let carol = DeviceKey::from_seed([25; 32]);
        let old = signed_record(&carol, "Carol", 0, vec![]);
        let new = signed_record(&carol, "Caroline", 1, vec![]);
        for (n, record, at) in [(26u8, &old, 1u64), (27, &old, 2), (28, &new, 3)] {
            a.state
                .save_learned(
                    &carol.public(),
                    &DeviceKey::from_seed([n; 32]).public(),
                    record,
                    &[],
                    at,
                )
                .expect("learn");
        }

        // When
        let ResolvedName::Learned(names) = a.resolve_name(carol.public()).expect("resolve") else {
            panic!("expected learned names");
        };

        // Then: the rename ranks first by revision; the superseded name
        // stays surfaced with its two holders — evidence, not arbitration
        assert_eq!(names.len(), 2);
        assert_eq!((names[0].name.as_str(), names[0].revision), ("Caroline", 1));
        assert_eq!(names[1].name, "Carol");
        assert_eq!(names[1].held_by.len(), 2);

        let _ = std::fs::remove_dir_all(temp_root("names"));
    }

    #[tokio::test]
    async fn who_is__should_serve_a_stored_record_to_contacts_only() {
        // Given: A holds C's record as a user-added contact — the server
        // side of the one-way-add flow (who-is-this.md §1). B asks about
        // C's key, first as a stranger.
        let a = Client::open_or_create(&temp_key("whois", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("whois", "client"))
            .await
            .expect("open B");
        let carol = DeviceKey::from_seed([7; 32]).public();
        let carol_record = ContactRecord::new(
            vec![carol],
            vec![],
            vec![RelayEntry {
                mailbox: "cc@203.0.113.9:9".to_string(),
                relay_url: Some("http://203.0.113.9:10".to_string()),
            }],
        );
        a.state.save_contact("carol", &carol_record).expect("save");

        // When: a stranger asks about a key A demonstrably holds
        let connection = net::connect_peer(
            &b.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            b.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let stranger = net::sync_request(&connection, SyncOp::WhoIs { key: carol })
            .await
            .expect("round-trip");

        // Then: nothing — declining and not-knowing look the same
        assert_eq!(stranger, SyncResult::NotHeld);

        // When: the same requester asks as a contact (fresh connection —
        // the gate is resolved per connection)
        befriend(&a.state, b.public_key());
        let connection = net::connect_peer(
            &b.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            b.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");
        let known = net::sync_request(&connection, SyncOp::WhoIs { key: carol })
            .await
            .expect("round-trip");
        let unknown = net::sync_request(
            &connection,
            SyncOp::WhoIs {
                key: DeviceKey::from_seed([8; 32]).public(),
            },
        )
        .await
        .expect("round-trip");

        // Then: the stored record verbatim; an unknown subject stays
        // NotHeld even for a contact (nothing learned-only or second-hand
        // is ever served)
        assert_eq!(
            known,
            SyncResult::Known {
                record: Box::new(carol_record),
                endorsements: vec![],
            }
        );
        assert_eq!(unknown, SyncResult::NotHeld);

        let _ = std::fs::remove_dir_all(temp_root("whois"));
    }

    #[tokio::test]
    async fn who_is__should_serve_the_fresh_self_record_for_the_own_key() {
        // Given: B is A's contact; A's profile is not yet complete
        let a = Client::open_or_create(&temp_key("whoisself", "server"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("whoisself", "client"))
            .await
            .expect("open B");
        befriend(&a.state, b.public_key());
        let connection = net::connect_peer(
            &b.transport,
            &a.transport.peer(),
            SYNC_ALPN,
            b.config.connect_timeout,
            &SystemClock,
        )
        .await
        .expect("connect");

        // When: asked about A's own key too early
        let early = net::sync_request(
            &connection,
            SyncOp::WhoIs {
                key: a.public_key(),
            },
        )
        .await
        .expect("round-trip");

        // Then: NotHeld — there is no record to serve yet
        assert_eq!(early, SyncResult::NotHeld);

        // When: A completes its profile (served fresh per request — no
        // restart needed, unlike endpoint homing)
        let relay = RelayEntry::from_spec("aa@203.0.113.1:1#http://203.0.113.1:2");
        a.state
            .save_profile("alice", std::slice::from_ref(&relay))
            .expect("save profile");
        let SyncResult::Known { record, .. } = net::sync_request(
            &connection,
            SyncOp::WhoIs {
                key: a.public_key(),
            },
        )
        .await
        .expect("round-trip") else {
            panic!("expected the self-record");
        };

        // Then: a verifiable self-record — key, self-claimed name, relays
        assert_eq!(record.keys, vec![a.public_key()]);
        assert_eq!(record.self_claimed_name(), Some("alice"));
        assert_eq!(record.relays, vec![relay]);

        let _ = std::fs::remove_dir_all(temp_root("whoisself"));
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
