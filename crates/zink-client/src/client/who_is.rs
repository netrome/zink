//! The who-is query plane (D1b/D2b, who-is-this.md §5): ask contacts about
//! a key, validate answers like scanned QRs, learn them with provenance —
//! and the auto-query that fires during a drain, rate-limited by
//! `AskedOnce` to one broadcast of interest per (subject, conversation)
//! per run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use zink_protocol::{
    ContactRecord, MessageEnvelope, MessageId, PublicKey, SYNC_ALPN, SignedAttestation, SyncOp,
    SyncResult,
};

use crate::error::Error;
use crate::hex;
use crate::net;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::{Draw, Mint};
use crate::ports::transport::{Request, Transport};

use super::{Client, Received};

/// A who-is query is a burst of speculative dials for display/freshness —
/// it never inherits a send's patience. Effective deadline is
/// `min(connect_timeout, cap)`, so edge tunings only tighten it.
/// Module-level so the tests that fire it reference the same number.
const WHO_IS_DIAL_CAP: Duration = Duration::from_secs(5);

/// The auto-query rate limit (D2b, groups.md §4): interest in a (subject,
/// conversation) pair is broadcast at most once per run — a drain loop must
/// not re-ask about a key it already asked about. In-memory on purpose: the
/// manual `who_is` trigger re-asks.
#[derive(Default)]
pub(super) struct AskedOnce(Mutex<BTreeSet<([u8; 32], [u8; 32])>>);

impl AskedOnce {
    /// True exactly once per pair — the license to broadcast.
    fn first(&self, subject: PublicKey, conversation: MessageId) -> bool {
        self.set().insert((subject.0, conversation.0))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.set().is_empty()
    }

    /// One lock site, one poisoning stance (as `crate::reach`): the set
    /// guards no invariant, and a lost note costs one duplicate broadcast.
    fn set(&self) -> MutexGuard<'_, BTreeSet<([u8; 32], [u8; 32])>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One subject-refresh ask per subject per this while healthy —
/// "order-of-daily" (R6, relay-lifecycle.md §6).
const REFRESH_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// The eager floor while the outbox owes the subject's relays: every
/// arrival may retry the ask, but never more often than this.
const REFRESH_SICK_FLOOR_MS: u64 = 5 * 60 * 1000;

/// The subject-refresh rate limit (R6): a test-and-note ledger, in-memory
/// on purpose like [`AskedOnce`] — a restart re-asks, which costs one
/// round trip on a channel that exists anyway and self-corrects. Timestamps
/// come in as data (`now`), the transport-port rule applied to state.
#[derive(Default)]
pub(super) struct RefreshLedger(Mutex<BTreeMap<[u8; 32], u64>>);

impl RefreshLedger {
    /// Atomic test-and-note: `true` = this caller asks now and the clock
    /// restarts; `false` = asked recently enough. `sick` (deposits to this
    /// subject currently owed) swaps the daily interval for the eager
    /// floor — the stale-relay case is the whole point.
    pub(super) fn due(&self, subject: &PublicKey, now: u64, sick: bool) -> bool {
        let interval = if sick {
            REFRESH_SICK_FLOOR_MS
        } else {
            REFRESH_INTERVAL_MS
        };
        // Same poisoning stance as `AskedOnce`: no invariant guarded, a
        // lost note costs one duplicate ask.
        let mut last = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match last.get(&subject.0) {
            Some(&at) if now.saturating_sub(at) < interval => false,
            _ => {
                last.insert(subject.0, now);
                true
            }
        }
    }
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

/// What a deliberate subject-ask produced — three states the edge words
/// distinctly: an answer landed; they were reached but served nothing
/// (declining and not-holding look the same on the wire, SPEC §5.2); or no
/// route reached them at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectAsk {
    Answered,
    Nothing,
    Unreachable,
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
fn valid_endorsements(
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

impl<C: Clock, W: WallClock, N: Transport, R: Draw + Mint> Client<C, W, N, R> {
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
    pub(super) fn contributed_to(
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
    pub(super) async fn auto_who_is(&self, received: &[Received]) {
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
                if !self.asked.first(subject, conversation) {
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

    /// Opportunistic subject-refresh (R6, relay-lifecycle.md §6): after an
    /// arrival, ask each *contact* sender about themself over the channel
    /// that just proved live — a bare dial-by-key first (no route hints:
    /// it succeeds exactly when the transport still holds the path they
    /// arrived on), the stored route as fallback. Answers land as
    /// subject-served learned records, the class that wins relay
    /// resolution (who-is-this.md §7) — so a moved relay heals while two
    /// people merely keep chatting, and a heal while deposits are owed
    /// flushes the outbox at once (R2 retargets it). Privacy-clean by
    /// construction: the only party ever asked is the subject, about
    /// themself — the §5 no-third-party stance stands untouched.
    pub(super) async fn auto_refresh(&self, received: &[Received]) {
        let Ok(contacts) = self.state.contacts() else {
            return;
        };
        let own = self.own_keys();
        let senders: BTreeSet<PublicKey> = received
            .iter()
            .map(|message| message.envelope.core.sender)
            .filter(|sender| !own.contains(sender))
            .collect();
        let owed: BTreeSet<String> = self
            .state
            .outbox()
            .into_iter()
            .map(|entry| entry.relay)
            .collect();
        for subject in senders {
            // Contacts only: a dial costs; strangers resolve via the D2b
            // scoped auto-query and the manual flows instead.
            let Some((_, record)) = contacts
                .iter()
                .find(|(_, record)| record.keys.contains(&subject))
            else {
                continue;
            };
            self.refresh_subject(subject, record, &owed).await;
        }
    }

    /// The page-open refresh (project 7 S3 — the tracker's queries-from-
    /// the-page rule): the same rate-limited, subject-only ask the arrival
    /// hook runs, for one contact. Asking the subject about themself over
    /// an authenticated channel reveals nothing to any third party; the
    /// ledger is shared with the arrival hook, so page opens never exceed
    /// R6's budget. Contacts only — for a stranger this is a no-op, so a
    /// stranger's page fires no query on open. `true` = a fresh record
    /// landed (the page re-reads its data).
    pub async fn refresh_contact(&self, subject: PublicKey) -> bool {
        let Ok(contacts) = self.state.contacts() else {
            return false;
        };
        let Some((_, record)) = contacts
            .iter()
            .find(|(_, record)| record.keys.contains(&subject))
        else {
            return false;
        };
        let owed: BTreeSet<String> = self
            .state
            .outbox()
            .into_iter()
            .map(|entry| entry.relay)
            .collect();
        self.refresh_subject(subject, record, &owed).await
    }

    /// The deliberate subject ask (project 7 — the stranger bootstrap's
    /// direct rung): `WhoIs(subject)` asked *of the subject*, for a key we
    /// may hold nothing about. A bare dial-by-key first — it succeeds
    /// exactly when the transport still holds the path they arrived on —
    /// then whatever route read-time resolution finds (a prior ask's
    /// learned relays). An explicit act, so no rate limit: a tap must
    /// never be silently swallowed. Its cost is the dial itself — a
    /// liveness receipt to a possible spammer — which is why no drain or
    /// page-open path ever calls this, and the edge's copy states it.
    pub async fn ask_subject(&self, subject: PublicKey) -> SubjectAsk {
        let timeout = self.config.connect_timeout.min(WHO_IS_DIAL_CAP);
        let mut routes = Vec::new();
        if let Ok(bare) = crate::adapters::iroh::validated_peer(subject, Vec::new()) {
            routes.push(bare);
        }
        if let Ok(routed) = self.peer_addr_for(subject, None) {
            routes.push(routed);
        }
        for addr in routes {
            match net::connect_peer(&self.transport, &addr, SYNC_ALPN, timeout, &self.clock).await {
                Ok(connection) => {
                    // Reached them — their answer (or decline) is final.
                    return if self.refresh_on(&connection, subject).await {
                        SubjectAsk::Answered
                    } else {
                        SubjectAsk::Nothing
                    };
                }
                Err(error) => {
                    tracing::debug!(%error, "subject ask: route failed; trying the next")
                }
            }
        }
        SubjectAsk::Unreachable
    }

    /// One subject's refresh, rate-limited and route-fallible — the shared
    /// core of the arrival hook and the page-open trigger. `true` = healed.
    async fn refresh_subject(
        &self,
        subject: PublicKey,
        record: &ContactRecord,
        owed: &BTreeSet<String>,
    ) -> bool {
        let relays = self.effective_relays(subject, Some(record));
        let sick = relays.iter().any(|entry| owed.contains(&entry.mailbox));
        let now = self.wall_clock.now_ms();
        if !self.refreshed.due(&subject, now, sick) {
            return false;
        }
        let timeout = self.config.connect_timeout.min(WHO_IS_DIAL_CAP);
        let mut routes = Vec::new();
        if let Ok(bare) = crate::adapters::iroh::validated_peer(subject, Vec::new()) {
            routes.push(bare);
        }
        if let Ok(routed) = self.peer_addr_for(subject, Some(record)) {
            routes.push(routed);
        }
        let mut healed = false;
        for addr in routes {
            match net::connect_peer(&self.transport, &addr, SYNC_ALPN, timeout, &self.clock).await {
                Ok(connection) => {
                    healed = self.refresh_on(&connection, subject).await;
                    // Reached them — their answer (or decline) is final.
                    break;
                }
                Err(error) => {
                    tracing::debug!(%error, "refresh: route failed; trying the next")
                }
            }
        }
        if healed && sick {
            // Fresh relays with messages owed: retarget right now (R2)
            // instead of waiting for the next drain's flush.
            let _ = self.flush_outbox().await;
        }
        healed
    }

    /// The one-connection core both R6 triggers share (the arrival hook
    /// above; the send path after a `Stored` ack): `WhoIs(subject)` asked
    /// *of the subject*, validated like every who-is answer, stored as the
    /// subject-served class. `true` = a fresh record landed.
    pub(super) async fn refresh_on(&self, connection: &impl Request, subject: PublicKey) -> bool {
        match net::sync_request(connection, SyncOp::WhoIs { key: subject }).await {
            Ok(SyncResult::Known {
                record: served,
                endorsements,
            }) => {
                if !served.keys.contains(&subject) {
                    tracing::warn!("refresh: answer does not name the subject; dropped");
                    return false;
                }
                let endorsements = valid_endorsements(subject, subject, endorsements);
                self.state
                    .save_learned(
                        &subject,
                        &subject,
                        &served,
                        &endorsements,
                        self.wall_clock.now_ms(),
                    )
                    .is_ok()
            }
            Ok(_) => false,
            Err(error) => {
                tracing::debug!(%error, "refresh: request failed");
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::adapters::system_clock::SystemClock;
    use crate::client::test_kit::{
        befriend, deposited_envelopes, deposited_frame, dir_bytes, loop_client, mailbox_only,
        mailbox_spec, message, routed_record, script_drain, signed_record, temp_key, temp_root,
    };
    use crate::client::{ClientConfig, RelaySource, ResolvedName};
    use crate::keystore;
    use crate::ports::clock::{TestClock, TestWallClock};
    use crate::ports::transport::{Loopback, TestTransport};
    use zink_protocol::{Attestation, Claim, DeviceKey, RelayEntry, SyncResponse, Versioned};

    #[test]
    fn refresh_ledger__should_gate_daily_and_floor_when_sick() {
        // Given
        let ledger = RefreshLedger::default();
        let subject = DeviceKey::from_seed([1; 32]).public();

        // When / Then: the first ask is always due; healthy re-asks wait
        // out the daily interval, and a granted ask restarts the clock
        assert!(ledger.due(&subject, 1_000, false));
        assert!(!ledger.due(&subject, 1_000 + REFRESH_INTERVAL_MS - 1, false));
        assert!(ledger.due(&subject, 1_000 + REFRESH_INTERVAL_MS, false));

        // And: sickness swaps in the eager floor — sooner than a day, but
        // still a floor, so arrivals can't turn into an ask storm
        let stamped = 1_000 + REFRESH_INTERVAL_MS;
        assert!(!ledger.due(&subject, stamped + REFRESH_SICK_FLOOR_MS - 1, true));
        assert!(ledger.due(&subject, stamped + REFRESH_SICK_FLOOR_MS, true));
    }

    #[tokio::test]
    async fn auto_refresh__should_heal_a_stale_record_over_a_live_channel() {
        // Given: bob holds anna under a stale record (dead relay); anna's
        // actual profile names a fresh one, and she serves bob (mutual).
        // A message from her arrives — the live channel.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("refresh-heal", "anna", &wire);
        let (b, _b_net, _b_clock) = loop_client("refresh-heal", "bob", &wire);
        a.state
            .save_profile(
                "anna",
                &[RelayEntry {
                    mailbox: "fresh@203.0.113.9:9".to_string(),
                    relay_url: Some("http://203.0.113.9:10".to_string()),
                }],
            )
            .expect("profile");
        befriend(&a.state, b.public_key());
        b.add_contact(
            &ContactRecord::new(
                vec![a.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "stale@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            ),
            Some("anna".to_string()),
        )
        .expect("add anna");
        let received = [Received {
            envelope: message(&a.device, vec![b.public_key()], None, vec![], 0, 0),
            relay: None,
            body: Ok(vec![]),
        }];

        // When: the arrival hooks run, nothing else
        b.after_direct(&received).await;

        // Then: relay resolution follows anna's fresh profile — healed by
        // merely receiving from her, no rescan anywhere; her stored record
        // is untouched (the learned store took the answer)
        let contact = b.resolve_contact("anna").expect("resolve");
        assert_eq!(contact.relays, vec!["fresh@203.0.113.9:9".to_string()]);
        assert!(matches!(
            b.relay_status("anna").expect("status").source,
            RelaySource::SubjectServed { .. }
        ));

        let _ = std::fs::remove_dir_all(temp_root("refresh-heal"));
    }

    #[tokio::test]
    async fn auto_refresh__should_ask_once_and_only_contacts() {
        // Given: the heal setup, plus a stranger among the senders
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("refresh-gate", "anna", &wire);
        let (b, b_net, _b_clock) = loop_client("refresh-gate", "bob", &wire);
        a.state
            .save_profile(
                "anna",
                &[RelayEntry {
                    mailbox: "fresh@203.0.113.9:9".to_string(),
                    relay_url: Some("http://203.0.113.9:10".to_string()),
                }],
            )
            .expect("profile");
        befriend(&a.state, b.public_key());
        b.add_contact(
            &ContactRecord::new(
                vec![a.public_key()],
                vec![],
                vec![RelayEntry {
                    mailbox: "stale@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            ),
            Some("anna".to_string()),
        )
        .expect("add anna");
        let stranger = DeviceKey::from_seed([77; 32]);
        let received = [
            Received {
                envelope: message(&a.device, vec![b.public_key()], None, vec![], 0, 0),
                relay: None,
                body: Ok(vec![]),
            },
            Received {
                envelope: message(&stranger, vec![b.public_key()], None, vec![], 0, 0),
                relay: None,
                body: Ok(vec![]),
            },
        ];

        // When: the same arrivals hook twice in one run
        b.auto_refresh(&received).await;
        b.auto_refresh(&received).await;

        // Then: one ask for the contact, none ever for the stranger — the
        // privacy line (§5) holds even for the subject-only query
        assert_eq!(b_net.dial.dialed(&a.public_key()), 1);
        assert_eq!(b_net.dial.dialed(&stranger.public()), 0);

        let _ = std::fs::remove_dir_all(temp_root("refresh-gate"));
    }

    #[tokio::test]
    async fn ask_subject__should_learn_their_record_when_they_serve_us() {
        // Given: anna added bob from his QR (her gate serves him) and has
        // a complete profile; bob holds nothing about anna — the
        // two-fresh-devices onboarding bootstrap
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("ask-subject", "anna", &wire);
        let (b, _b_net, _b_clock) = loop_client("ask-subject", "bob", &wire);
        a.state
            .save_profile(
                "anna",
                &[RelayEntry {
                    mailbox: "aa@203.0.113.9:9".to_string(),
                    relay_url: Some("http://203.0.113.9:10".to_string()),
                }],
            )
            .expect("profile");
        befriend(&a.state, b.public_key());

        // When: bob deliberately asks anna who she is
        let outcome = b.ask_subject(a.public_key()).await;

        // Then: her self-served record lands as a promotable candidate —
        // the add button's exact input — and the add completes from it
        assert_eq!(outcome, SubjectAsk::Answered);
        let candidates = b.learned_candidates(a.public_key()).expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0.name, "anna");
        let petname = b.add_contact(&candidates[0].1, None).expect("add");
        assert_eq!(petname, "anna");

        let _ = std::fs::remove_dir_all(temp_root("ask-subject"));
    }

    #[tokio::test]
    async fn ask_subject__should_learn_nothing_from_a_responder_that_does_not_serve_us() {
        // Given: anna has a complete profile but never added bob — her
        // contacts-only gate declines, indistinguishable from not holding
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("ask-subject-gate", "anna", &wire);
        let (b, _b_net, _b_clock) = loop_client("ask-subject-gate", "bob", &wire);
        a.state
            .save_profile(
                "anna",
                &[RelayEntry {
                    mailbox: "aa@203.0.113.9:9".to_string(),
                    relay_url: Some("http://203.0.113.9:10".to_string()),
                }],
            )
            .expect("profile");

        // When
        let outcome = b.ask_subject(a.public_key()).await;

        // Then: reached, nothing served, nothing learned
        assert_eq!(outcome, SubjectAsk::Nothing);
        assert!(
            b.learned_candidates(a.public_key())
                .expect("candidates")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_root("ask-subject-gate"));
    }

    #[tokio::test]
    async fn ask_subject__should_report_unreachable_when_no_route_connects() {
        // Given: a subject whose one route (the bare dial) hangs — offline
        let wire = Loopback::new();
        let (b, b_net, b_clock) = loop_client("ask-subject-offline", "bob", &wire);
        let ghost = DeviceKey::from_seed([9; 32]).public();
        b_net.dial.hold(&ghost);

        // When: the dial parks and its deadline passes
        let (outcome, ()) = tokio::join!(b.ask_subject(ghost), async {
            b_clock.wait_for_sleepers(1).await;
            b_clock.advance(WHO_IS_DIAL_CAP);
        });

        // Then
        assert_eq!(outcome, SubjectAsk::Unreachable);
        assert!(b.learned_candidates(ghost).expect("candidates").is_empty());

        let _ = std::fs::remove_dir_all(temp_root("ask-subject-offline"));
    }

    #[tokio::test]
    async fn ask_subject__should_drop_an_answer_that_does_not_name_them() {
        // Given: the subject answers the ask with a record naming someone
        // else — a hostile self-serve
        let key_path = temp_key("ask-subject-hostile", "asker");
        keystore::create(&key_path).expect("create key");
        let net = TestTransport::new();
        let subject = DeviceKey::from_seed([41; 32]).public();
        let other = DeviceKey::from_seed([42; 32]).public();
        net.dial.connect(&subject).reply(
            SyncResponse::new(SyncResult::Known {
                record: Box::new(ContactRecord::new(vec![other], vec![], vec![])),
                endorsements: vec![],
            })
            .to_bytes(),
        );
        let b = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            TestClock::new(),
            TestWallClock::new(1_000),
            net.clone(),
        );

        // When
        let outcome = b.ask_subject(subject).await;

        // Then: reached, but the forged record is dropped, never learned
        assert_eq!(outcome, SubjectAsk::Nothing);
        assert!(
            b.learned_candidates(subject)
                .expect("candidates")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_root("ask-subject-hostile"));
    }

    #[tokio::test]
    async fn deliver_direct__should_refresh_the_recipients_record_on_ack() {
        // Given: anna sends to bob direct (mutual contacts, bob's real
        // handler stores and acks); bob's profile is fresh
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("refresh-send", "anna", &wire);
        let (b, _b_net, _b_clock) = loop_client("refresh-send", "bob", &wire);
        befriend(&b.state, a.public_key());
        b.state
            .save_profile(
                "bob",
                &[RelayEntry {
                    mailbox: "bb@203.0.113.2:2".to_string(),
                    relay_url: Some("http://203.0.113.2:2".to_string()),
                }],
            )
            .expect("profile");
        let relay = DeviceKey::from_seed([78; 32]).public();
        a.add_contact(
            &routed_record(b.public_key(), &relay),
            Some("bob".to_string()),
        )
        .expect("add bob");

        // When
        let receipt = a
            .send(
                &[a.resolve_contact("bob").expect("resolve")],
                b"hey".to_vec(),
                vec![],
            )
            .await
            .expect("send");

        // Then: the ack rode back with bob's self-served record — anna's
        // view of his relays stays current without a rescan
        assert_eq!(receipt.direct_recipients, 1);
        let learned = a.state.learned(&b.public_key());
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].responder, b.public_key());
        assert_eq!(
            learned[0].record.relays[0].mailbox,
            "bb@203.0.113.2:2".to_string()
        );

        let _ = std::fs::remove_dir_all(temp_root("refresh-send"));
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
        assert!(a.asked.is_empty());
        assert!(matches!(
            a.resolve_name(carol).expect("resolve"),
            ResolvedName::Unknown
        ));

        let _ = std::fs::remove_dir_all(temp_root("nogate"));
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
}
