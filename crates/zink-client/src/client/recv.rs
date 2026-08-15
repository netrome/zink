//! Receiving: drain mailboxes (register → page → dedup → open → remember →
//! ack), subscribe to live nudges, and the post-arrival healing every
//! arrival path runs — auto-sync, scoped who-is, re-wrap. Best-effort per
//! relay (De6a): one relay we cannot reach costs its own mail and nothing
//! else.

use std::collections::BTreeSet;
use std::time::Duration;

use zink_protocol::{MailboxOp, MailboxResult, MessageEnvelope, OpenError};

use crate::error::Error;
use crate::net;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::{AcceptUni, Request, Transport};

use super::Client;

/// A nudge is a zero-length uni stream (live-delivery.md §3); the cap is a
/// backstop against a hostile relay streaming into the signal.
const MAX_NUDGE_BYTES: usize = 64;

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

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
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
            let delay = jittered(backoff, &self.rng);
            tracing::debug!(relay, ?delay, "reconnecting after backoff");
            self.clock.sleep(delay).await;
            backoff = backoff.saturating_mul(2).min(Duration::from_secs(60));
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
}

/// The jittered reconnect delay: uniform over [backoff/2, 3·backoff/2), so
/// a relay restart doesn't get a thundering herd of resubscriptions.
/// Integer millisecond math end to end — no float detour, none of
/// `Duration`'s panic-capable arithmetic; the saturations cap values the
/// 60 s backoff ceiling makes unreachable anyway.
fn jittered(backoff: Duration, rng: &impl Draw) -> Duration {
    let base = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
    let offset = rng.draw(base.max(1));
    Duration::from_millis((base / 2).saturating_add(offset))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::adapters::system_clock::SystemClock;
    use crate::client::ClientConfig;
    use crate::client::test_kit::{mailbox_spec, script_drain, sealed_for, temp_key, temp_root};
    use crate::keystore;
    use crate::ports::clock::TestClock;
    use crate::ports::rng::TestDraw;
    use crate::ports::transport::TestTransport;
    use zink_protocol::DeviceKey;

    #[test]
    fn jittered__should_span_half_to_under_three_halves_of_the_backoff() {
        // Given
        let backoff = Duration::from_secs(1);

        // When / Then: the extreme draws pin the band's endpoints
        assert_eq!(jittered(backoff, &TestDraw(0)), Duration::from_millis(500));
        assert_eq!(
            jittered(backoff, &TestDraw(u64::MAX)),
            Duration::from_millis(1499)
        );
        // A zero backoff stays an immediate retry, not a panic.
        assert_eq!(
            jittered(Duration::ZERO, &TestDraw(u64::MAX)),
            Duration::ZERO
        );
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
}
