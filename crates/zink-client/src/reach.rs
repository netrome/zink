//! The reach ledger (D5): per-peer evidence about direct reachability and
//! the policy that spends it (`docs/design/direct-delivery.md`,
//! `docs/design/fast-failure.md`).
//!
//! One shared fact table. The send side notes dial outcomes (concurrently —
//! a fan-out's dials race) and reads what a dial is worth; the serving side
//! notes inbound connections, the cheapest evidence there is that a path
//! exists. Evidence is what licenses a send to spend real time on a direct
//! dial instead of just using the mailbox.
//!
//! **Positive** evidence is in memory on purpose: that a peer was reachable
//! is a fact about *now*, so a fresh process starts from "don't know" rather
//! than from a stale opinion that a path exists.
//!
//! **Negative** evidence persists (De6b, `unreachable.keys`): "this dial got
//! nowhere at time T" is falsifiable on its face — past the cooldown it is
//! simply ignored — so it cannot rot into a wrong opinion, and re-learning
//! it costs the full dial deadline every time a process starts.
//!
//! The ledger holds no clock and does no I/O: every method takes `now` as
//! data (the transport rule — no time inside the port — applied to state),
//! `restore` is handed the persisted rows, and `unreachable_snapshot`
//! returns what to write. Policy stays a pure function of the facts,
//! testable with fabricated timestamps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use zink_protocol::PublicKey;

/// How long a failed dial suppresses the next one. Short enough that a peer
/// coming back online is noticed within a minute — until then its messages
/// simply take the mailbox, which is what it is for.
///
/// One number since De6b: the same window decides whether a dial is
/// suppressed, which failures are worth persisting, and which persisted
/// failures are worth restoring — two copies could disagree.
pub(crate) const FAIL_COOLDOWN_MS: u64 = 60 * 1000;

/// Wall-clock ms (0 = never). Copy-small; the map is rewritten in place.
#[derive(Default, Clone, Copy, Debug)]
struct Reach {
    /// Last evidence the peer is reachable: a delivery it took, a push it
    /// declined (declining still proves we reached it), or a connection it
    /// opened to us.
    seen_ms: u64,
    /// Last dial that got nowhere — suppresses re-dialing for a cooldown, so
    /// a recipient that is simply offline costs a send nothing. Persisted
    /// (De6b), so that holds across process starts too.
    failed_ms: u64,
}

/// The shared handle: cheap to clone, one per client, held by both the send
/// flows and the serving router. The lock lives here and only here.
#[derive(Clone, Debug)]
pub(crate) struct ReachLedger(Arc<Mutex<BTreeMap<[u8; 32], Reach>>>);

impl ReachLedger {
    /// Seed from the persisted *negative* evidence (De6b), dropping entries
    /// that have already cooled down — a stale failure must never be the
    /// reason a reachable peer goes undialed — and entries dated in the
    /// future (the wall clock was rewound since they were recorded), which
    /// would otherwise look fresh for the whole rewound span.
    ///
    /// Only failures are restored; `seen_ms` deliberately starts at zero, so
    /// a peer we knew last run gets one cheap probe rather than the full
    /// known-peer budget: a path that existed then may not exist now.
    pub(crate) fn restore(persisted: Vec<([u8; 32], u64)>, now: u64) -> Self {
        let map: BTreeMap<[u8; 32], Reach> = persisted
            .into_iter()
            .filter(|(_, failed_ms)| {
                now.checked_sub(*failed_ms)
                    .is_some_and(|age| age < FAIL_COOLDOWN_MS)
            })
            .map(|(key, failed_ms)| {
                (
                    key,
                    Reach {
                        seen_ms: 0,
                        failed_ms,
                    },
                )
            })
            .collect();
        if !map.is_empty() {
            tracing::debug!(peers = map.len(), "restored recently-unreachable peers");
        }
        Self(Arc::new(Mutex::new(map)))
    }

    /// How long a direct dial to `key` is worth right now — `None` means
    /// don't dial at all.
    pub(crate) fn dial_budget(
        &self,
        key: &PublicKey,
        now: u64,
        connect_timeout: Duration,
    ) -> Option<Duration> {
        let reach = self.table().get(&key.0).copied().unwrap_or_default();
        direct_budget(reach, now, connect_timeout)
    }

    /// A delivery `key` accepted (`Stored`) — the strongest evidence, so it
    /// also clears any pending cooldown: a concurrent dial's failure must
    /// not suppress the next send to a peer that just took a message.
    pub(crate) fn note_delivered(&self, key: &PublicKey, now: u64) {
        let mut table = self.table();
        let reach = table.entry(key.0).or_default();
        reach.seen_ms = now;
        reach.failed_ms = 0;
    }

    /// Proof a path exists, and nothing more: the peer answered a dial (a
    /// decline counts — reaching a live peer is the fact that matters, and
    /// its reasons are per message) or opened a connection to us. A pending
    /// cooldown is left to expire on its own.
    pub(crate) fn note_seen(&self, key: &PublicKey, now: u64) {
        self.table().entry(key.0).or_default().seen_ms = now;
    }

    /// A dial got nowhere: starts the cooldown that keeps the next sends
    /// mailbox-only until it expires.
    pub(crate) fn note_failed(&self, key: &PublicKey, now: u64) {
        self.table().entry(key.0).or_default().failed_ms = now;
    }

    /// The negative half worth persisting (De6b), pruned: an entry that has
    /// cooled down, or that a success cleared, is simply not written.
    /// Deriving this from the live table makes one write per send instead of
    /// one per failed recipient. Positive evidence is never in it by design
    /// (direct-delivery.md §5) — see the module doc.
    pub(crate) fn unreachable_snapshot(&self, now: u64) -> Vec<([u8; 32], u64)> {
        self.table()
            .iter()
            .filter(|(_, reach)| {
                reach.failed_ms > 0 && now.saturating_sub(reach.failed_ms) < FAIL_COOLDOWN_MS
            })
            .map(|(key, reach)| (*key, reach.failed_ms))
            .collect()
    }

    /// The one lock site, and the one poisoning stance: take the table
    /// anyway. No invariant spans entries — a panic mid-note can at worst
    /// leave one peer's stamp behind, and reach evidence is advisory
    /// (its total loss costs one extra dial per peer), so refusing all
    /// future notes and budgets over it would hurt more than it protects.
    fn table(&self) -> MutexGuard<'_, BTreeMap<[u8; 32], Reach>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How long to spend dialing one recipient directly (D5) — `None` = don't
/// dial at all. Pure, so the policy is pinned by tests rather than by timing.
///
/// The rule that matters: **a recipient that is simply offline must not cost a
/// send anything, however often we send to it.** A blind dial per send put
/// seconds on the send path (measured: ~3 s per unreachable recipient, plus a
/// per-process drain cost at close), which the edge pays before it can render
/// the message. So we spend real time only where evidence says it will land.
fn direct_budget(reach: Reach, now: u64, connect_timeout: Duration) -> Option<Duration> {
    /// Budget for a peer we have recent evidence about — a live conversation.
    const CAP_KNOWN: Duration = Duration::from_secs(3);
    /// Budget for a peer we know nothing about: enough for an already-warm
    /// path or a LAN, too little to delay a message over.
    const CAP_UNKNOWN: Duration = Duration::from_millis(600);
    /// How long evidence of reachability stays worth acting on.
    const EVIDENCE_TTL_MS: u64 = 5 * 60 * 1000;

    if now.saturating_sub(reach.failed_ms) < FAIL_COOLDOWN_MS {
        return None;
    }
    let known = now.saturating_sub(reach.seen_ms) < EVIDENCE_TTL_MS;
    Some(connect_timeout.min(if known { CAP_KNOWN } else { CAP_UNKNOWN }))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn key(byte: u8) -> PublicKey {
        PublicKey([byte; 32])
    }

    const NOW: u64 = 10 * 60 * 1000;
    const PRODUCTION: Duration = Duration::from_secs(10);

    #[test]
    fn direct_budget__should_spend_time_only_where_evidence_says_it_lands() {
        // Given
        let now = NOW;
        let production = PRODUCTION;
        let never = Reach::default();
        let reached = Reach {
            seen_ms: now - 1000,
            failed_ms: 0,
        };
        let stale = Reach {
            seen_ms: now - 6 * 60 * 1000, // past the evidence TTL
            failed_ms: 0,
        };
        let just_failed = Reach {
            seen_ms: now - 1000,
            failed_ms: now - 5000,
        };

        // When / Then: an unreachable recipient costs a send nothing…
        assert_eq!(direct_budget(just_failed, now, production), None);
        // …a live conversation gets a real budget…
        assert_eq!(
            direct_budget(reached, now, production),
            Some(Duration::from_secs(3))
        );
        // …an unknown or stale peer gets one cheap probe…
        assert_eq!(
            direct_budget(never, now, production),
            Some(Duration::from_millis(600))
        );
        assert_eq!(
            direct_budget(stale, now, production),
            Some(Duration::from_millis(600))
        );
        // …and an edge that tightened `connect_timeout` still wins.
        assert_eq!(
            direct_budget(reached, now, Duration::from_millis(200)),
            Some(Duration::from_millis(200))
        );
        // A cooled-down failure is retried again afterwards.
        let recovered = now + 61 * 1000;
        assert!(direct_budget(just_failed, recovered, production).is_some());
    }

    #[test]
    fn restore__should_drop_failures_past_the_cooldown() {
        // Given: one failure from just now and one ancient
        let fresh = key(7);
        let ancient = key(8);
        let ledger = ReachLedger::restore(vec![(fresh.0, NOW), (ancient.0, 1)], NOW);

        // Then: the fresh one still suppresses dialing, the cooled-down one
        // doesn't — persisted negative evidence must never be the reason a
        // *reachable* peer goes undialed…
        assert_eq!(ledger.dial_budget(&fresh, NOW, PRODUCTION), None);
        assert!(ledger.dial_budget(&ancient, NOW, PRODUCTION).is_some());
        // …and nothing restores positive evidence: once the failure cools,
        // last run's peer gets one cheap probe, not the known-peer budget.
        let cooled = NOW + FAIL_COOLDOWN_MS;
        assert_eq!(
            ledger.dial_budget(&fresh, cooled, PRODUCTION),
            Some(Duration::from_millis(600))
        );
    }

    #[test]
    fn restore__should_drop_future_dated_failures_after_a_wall_rewind() {
        // Given: a failure recorded under a wall clock a year ahead of the
        // one we restore with
        const YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
        let peer = key(9);

        // When
        let ledger = ReachLedger::restore(vec![(peer.0, 2 * YEAR_MS)], YEAR_MS);

        // Then: future-dated evidence must not suppress dials
        assert!(ledger.dial_budget(&peer, YEAR_MS, PRODUCTION).is_some());
    }

    #[test]
    fn note_delivered__should_clear_the_cooldown_a_concurrent_dial_set() {
        // Given: a fan-out where one dial failed and another was accepted
        let peer = key(1);
        let ledger = ReachLedger::restore(Vec::new(), NOW);
        ledger.note_failed(&peer, NOW);

        // When
        ledger.note_delivered(&peer, NOW);

        // Then: the accepted delivery wins — full budget, nothing persisted
        assert_eq!(
            ledger.dial_budget(&peer, NOW, PRODUCTION),
            Some(Duration::from_secs(3))
        );
        assert_eq!(ledger.unreachable_snapshot(NOW), Vec::new());
    }

    #[test]
    fn note_seen__should_not_clear_a_pending_cooldown() {
        // Given: a failed dial, then mere proof the peer exists (a decline,
        // or an inbound connection)
        let peer = key(2);
        let ledger = ReachLedger::restore(Vec::new(), NOW);
        ledger.note_failed(&peer, NOW);

        // When
        ledger.note_seen(&peer, NOW + 1);

        // Then: the cooldown holds until it expires on its own…
        assert_eq!(ledger.dial_budget(&peer, NOW + 2, PRODUCTION), None);
        // …and the failure is still worth persisting.
        assert_eq!(ledger.unreachable_snapshot(NOW + 2), vec![(peer.0, NOW)]);
    }

    #[test]
    fn unreachable_snapshot__should_keep_only_failures_inside_the_cooldown() {
        // Given: a fresh failure, a cooled-down one, and a cleared one
        let fresh = key(3);
        let cooled = key(4);
        let cleared = key(5);
        let ledger = ReachLedger::restore(Vec::new(), NOW);
        ledger.note_failed(&fresh, NOW);
        ledger.note_failed(&cooled, NOW - FAIL_COOLDOWN_MS);
        ledger.note_failed(&cleared, NOW);
        ledger.note_delivered(&cleared, NOW);

        // When / Then: only the fresh failure is worth writing
        assert_eq!(ledger.unreachable_snapshot(NOW), vec![(fresh.0, NOW)]);
    }
}
