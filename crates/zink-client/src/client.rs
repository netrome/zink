//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::BTreeSet;
use std::time::Duration;

use zink_protocol::{DeviceKey, MessageEnvelope, PublicKey};

use crate::adapters::iroh::IrohTransport;
use crate::adapters::system_clock::SystemClock;
use crate::adapters::system_rng::SystemRng;
use crate::error::Error;
use crate::keystore;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;
use crate::reach::ReachLedger;
use crate::state::ClientState;

mod backfill;
mod contacts;
mod history;
mod outbox;
mod profile;
mod recv;
mod send;
mod who_is;

pub use contacts::{
    Contact, DeviceEvidence, Disavowal, LearnedName, RecordMatch, RecordUpdate, RelayHealth,
    RelaySource, RelayStatus, ResolvedName,
};
pub use history::{
    ConversationSummary, HistoryMessage, Inbox, LastMessage, MAX_MESSAGE_REQUESTS, triage,
};
pub use outbox::{FlushReport, OUTBOX_GIVE_UP_MS};
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
    /// The R6 subject-refresh rate limit; rationale on
    /// `who_is::RefreshLedger`.
    refreshed: who_is::RefreshLedger,
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
            refreshed: who_is::RefreshLedger::default(),
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
    use std::time::Duration;

    use super::test_kit::{
        befriend, deposited_envelopes, deposited_frame, loop_client, mailbox_spec, message,
        open_homed, spawn_test_relay, temp_key, temp_root,
    };
    use super::*;
    use crate::hex;
    use crate::ports::transport::{Home, Loopback};
    use zink_protocol::{ContactRecord, DeviceKey, RelayEntry};

    #[tokio::test]
    async fn migration_drill__should_heal_the_reinstalled_relay_end_to_end() {
        // The project-5 capstone (5-relay-lifecycle §1, the 2026-08-15
        // story): anna's server is reinstalled — new relay, renamed
        // profile — while bob still holds her old record. Every wall from
        // the real migration must now be a door, each layer proven on the
        // same state.
        const DEADLINE: Duration = Duration::from_secs(10);
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("drill", "anna", &wire);
        let (b, b_net, b_clock) = loop_client("drill", "bob", &wire);
        let relay_old = DeviceKey::from_seed([120; 32]).public();
        let relay_new = DeviceKey::from_seed([121; 32]).public();

        // Given: anna, profiled on the old relay, serves bob; bob added
        // her from her real record — petname defaulted from her self-claim
        a.state
            .save_profile(
                "Anna",
                &[RelayEntry {
                    mailbox: mailbox_spec(&relay_old),
                    relay_url: None,
                }],
            )
            .expect("anna's profile");
        befriend(&a.state, b.public_key());
        let original = a.my_record().expect("original record");
        let petname = b.add_contact(&original, None).expect("bob adds anna");
        assert_eq!(petname, "Anna");

        // When: her relay is gone and bob's send times out — the honest
        // "sending…" (owed, retried, surfaced)
        b_net.dial.hold(&relay_old);
        let recipients = [b.resolve_contact("Anna").expect("resolve")];
        let (result, ()) = tokio::join!(
            b.send(&recipients, b"are you there?".to_vec(), vec![]),
            async {
                b_clock.wait_for_sleepers(1).await;
                b_clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        let (stuck_id, conversation) = {
            let entry = &b.state.outbox()[0];
            (entry.message, entry.conversation)
        };
        assert!(
            b.history(conversation).expect("history")[0]
                .owed_since_ms
                .is_some(),
            "the marker says sending…"
        );

        // …anna reinstalls: new relay, renamed profile (revision bumped,
        // as a real set_profile rename does)
        a.state
            .save_profile(
                "Ann",
                &[RelayEntry {
                    mailbox: mailbox_spec(&relay_new),
                    relay_url: None,
                }],
            )
            .expect("anna migrates");
        a.state.save_profile_revision(1).expect("bump");

        // Layer 1+2 — subject-refresh (R6) + outbox re-target (R2): one
        // message from anna arrives over the live channel. Nothing else.
        let relay_conn = b_net.dial.connect(&relay_new);
        relay_conn.reply(deposited_frame());
        let received = [Received {
            envelope: message(&a.device, vec![b.public_key()], None, vec![], 0, 0),
            relay: None,
            body: Ok(vec![]),
        }];
        b.after_direct(&received).await;

        // Then: resolution follows her fresh profile, the stuck message
        // followed her to the NEW relay, the marker converged — and bob's
        // stored record was never touched by any of it
        assert_eq!(
            b.resolve_contact("Anna").expect("resolve").relays,
            vec![mailbox_spec(&relay_new)]
        );
        assert!(matches!(
            b.relay_status("Anna").expect("status").source,
            RelaySource::SubjectServed { .. }
        ));
        assert!(
            deposited_envelopes(&relay_conn)
                .iter()
                .any(|envelope| envelope.id() == stuck_id),
            "the owed message was deposited to the new relay"
        );
        assert!(b.state.outbox().is_empty(), "the debt is settled");
        assert!(
            b.history(conversation).expect("history")[0]
                .owed_since_ms
                .is_none(),
            "the marker converged to the truth"
        );
        assert_eq!(
            b.contacts().expect("contacts")[0].1,
            original,
            "healing never wrote the trust anchor"
        );

        // Layer 3 — rescan-as-update (R1): her renamed record previews as
        // an update of the same entry — the diff a confirm card renders —
        // and applies while keeping bob's petname
        let renamed = a.my_record().expect("renamed record");
        let RecordMatch::Update(update) = b.preview_contact(&renamed).expect("preview") else {
            panic!("expected an update match");
        };
        assert_eq!(update.petname, "Anna");
        assert_eq!(update.old_name.as_deref(), Some("Anna"));
        assert_eq!(update.new_name.as_deref(), Some("Ann"));
        assert_eq!(update.relays_added, vec![mailbox_spec(&relay_new)]);
        assert_eq!(update.relays_removed, vec![mailbox_spec(&relay_old)]);
        assert_eq!(b.update_contact(&renamed).expect("update"), "Anna");
        assert_eq!(
            b.contacts().expect("contacts")[0].1,
            renamed,
            "the explicit act replaced the anchor"
        );

        let _ = std::fs::remove_dir_all(temp_root("drill"));
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
}
