//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use zink_protocol::{
    DeviceKey, MAX_GET_KEYS_IDS, MessageEnvelope, MessageId, PublicKey, SYNC_ALPN, SyncOp,
    SyncResult,
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
use crate::{keystore, net};

mod contacts;
mod history;
mod outbox;
mod profile;
mod recv;
mod send;
mod who_is;

pub use contacts::{Contact, DeviceEvidence, Disavowal, LearnedName, ResolvedName};
pub use history::{ConversationSummary, HistoryMessage, Inbox, MAX_MESSAGE_REQUESTS, triage};
pub use outbox::FlushReport;
pub use profile::AvatarReceipt;
pub use recv::{Received, RecvReport, RelayFailure};
pub use send::{ReplyContacts, SendReceipt, StagedSend};
pub use who_is::{WhoIsAnswer, WhoIsOutcome};

/// `sync.rs` serves this for a who-is about our own key; the path predates
/// the module split.
pub(crate) use profile::build_own_record;

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

#[cfg(test)]
mod test_kit;

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::test_kit::{
        befriend, chain, loop_client, mailbox_only, open_homed, sealed_chain, spawn_test_relay,
        temp_key, temp_root,
    };
    use super::*;
    use crate::hex;
    use crate::ports::transport::{Home, Loopback};
    use zink_protocol::{ContactRecord, RelayEntry};

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
