//! The outbox: the per-(message, relay) ledger of deliveries still owed.
//! Staging writes entries before any network work, so a crash loses no
//! message; a flush retries them idempotently (deposits dedup by id, blob
//! pushes by hash) with bounded concurrency. Entries past the give-up
//! window stay surfaced as pending, no longer retried — deleting them is
//! not our call.
//!
//! **The ledger owes recipients, not dial strings** (R2,
//! relay-lifecycle.md): the relay set staged at send time is a snapshot,
//! and records move. Every flush first *reconciles* each message's entries
//! against current knowledge — the sealed recipients re-resolve through
//! `effective_relays`, entries for relays no longer owed are released,
//! newly-owed relays get entries inheriting the message's original age —
//! and a message every recipient has durably acked is settled outright.

use std::collections::{BTreeMap, BTreeSet};

use zink_protocol::{MessageEnvelope, MessageId, PublicKey};

use crate::error::Error;
use crate::hex;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;

use super::Client;

/// Outbox entries older than this stop being retried (but stay surfaced):
/// mirrors the relay's default mailbox retention — past it, recipients'
/// cursors have moved on and the message is socially dead. Public so edges
/// can render the same boundary ("undelivered", R3) the flush enforces.
pub const OUTBOX_GIVE_UP_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// How many owed deliveries a flush has in flight at once (De6d). Concurrency
/// is what stops n dead relays costing n deadlines; the *bound* is because
/// each in-flight entry holds its message's blob bytes twice (loaded from the
/// cache, then staged), and a long backlog of images fanned out without limit
/// would spike memory where the serial version never did.
const FLUSH_CONCURRENCY: usize = 8;

/// What one outbox flush accomplished.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlushReport {
    pub delivered: usize,
    pub pending: usize,
    /// Entries past the give-up window: left in place, no longer retried.
    pub expired: usize,
    /// Entries settled without a deposit (R2): the relay is no longer in
    /// any recipient's current record, or every recipient the entry served
    /// has durably acked. Released debt, not delivery — `delivered` stays
    /// honest.
    pub released: usize,
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Retry every outstanding delivery (idempotent: deposits dedup by id,
    /// blob pushes by hash). Entries older than the give-up window are left
    /// in place unretried — the relay's retention has expired, the message
    /// stays surfaced as pending/undelivered (deleting it is not our call).
    pub async fn flush_outbox(&self) -> Result<FlushReport, Error> {
        let mut report = FlushReport::default();
        let now = self.wall_clock.now_ms();
        // Reconcile before retrying (R2): the ledger owes recipients, and
        // records move — follow them, and settle what acks already proved.
        let owed = self.reconcile_outbox(&mut report)?;
        // Cheap triage next: an aged-out entry never touches the network.
        let owed: Vec<crate::state::OutboxEntry> = owed
            .into_iter()
            .filter(|entry| {
                let expired = now.saturating_sub(entry.created_ms) > OUTBOX_GIVE_UP_MS;
                report.expired += usize::from(expired);
                !expired
            })
            .collect();
        // Concurrent per entry (De6d) — serially, one unreachable relay made
        // every later entry wait out its deadline first, so a backlog across
        // n dead relays cost n × `connect_timeout`.
        //
        // **Chunked**, though: an entry in flight holds its message's blob
        // bytes twice (loaded from the cache, then staged), so an unbounded
        // fan-out over a long backlog of images would be a memory spike where
        // the serial version had none. This bounds that at a few entries
        // while still turning n deadlines into ceil(n / N).
        for chunk in owed.chunks(FLUSH_CONCURRENCY) {
            let mut ready = Vec::with_capacity(chunk.len());
            for entry in chunk {
                match self.reload_owed(entry)? {
                    Some(loaded) => ready.push((entry, loaded)),
                    // No stored envelope — nothing a retry could ever send.
                    None => continue,
                }
            }
            let outcomes = n0_future::join_all(ready.iter().map(
                |(entry, (envelope, encrypted))| async move {
                    match self
                        .deliver_to_relay(&entry.relay, envelope, encrypted)
                        .await
                    {
                        Ok(()) => {
                            self.state.clear_outbox(entry.message, &entry.relay);
                            true
                        }
                        Err(error) => {
                            tracing::warn!(relay = %entry.relay, %error, "outbox retry failed");
                            false
                        }
                    }
                },
            ))
            .await;
            for delivered in outcomes {
                if delivered {
                    report.delivered += 1;
                } else {
                    report.pending += 1;
                }
            }
        }
        Ok(report)
    }

    /// Re-derive what each pending message still owes and reshape the
    /// ledger to match (R2, relay-lifecycle.md). Per message: the sealed
    /// recipients resolve through `effective_relays` exactly as a fresh
    /// send would; a recipient with a durable ack needs nothing more
    /// (unless the message carries blobs — recipients fetch blob bytes
    /// from their own relay's cache, so blob messages keep their deposits);
    /// a recipient resolving to *no* relays (a raw-spec send, a record
    /// we've lost) keeps every staged entry alive — dropping the debt on
    /// no evidence would lose the message silently, so staged knowledge is
    /// the honest fallback. Entries for relays no longer owed are released;
    /// newly-owed relays get entries inheriting the message's original age
    /// (a moved record must not reset the give-up clock).
    ///
    /// Membership is never re-litigated: the recipients were sealed at
    /// send time, and a later disavowal changes future addressing, not
    /// deliveries already owed.
    fn reconcile_outbox(
        &self,
        report: &mut FlushReport,
    ) -> Result<Vec<crate::state::OutboxEntry>, Error> {
        let mut groups: BTreeMap<MessageId, Vec<crate::state::OutboxEntry>> = BTreeMap::new();
        for entry in self.state.outbox() {
            groups.entry(entry.message).or_default().push(entry);
        }
        let mut owed = Vec::new();
        for (message, entries) in groups {
            let conversation = entries[0].conversation;
            let created_ms = entries
                .iter()
                .map(|entry| entry.created_ms)
                .min()
                .unwrap_or_default();
            let envelope = match self.state.load_envelope(conversation, message) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // No stored envelope — nothing a retry could ever send
                    // (same stance as `reload_owed`).
                    tracing::warn!(%error, "dropping unfulfillable outbox entries");
                    for entry in &entries {
                        self.state.clear_outbox(message, &entry.relay);
                    }
                    continue;
                }
            };
            let has_blobs = !envelope.core.blob_refs.is_empty();
            let acked: BTreeSet<PublicKey> = self
                .state
                .acks_in(conversation)
                .remove(&message)
                .unwrap_or_default()
                .into_iter()
                .collect();
            let mut current: BTreeSet<String> = BTreeSet::new();
            let mut keep_staged = false;
            for key in &envelope.core.recipients {
                if !has_blobs && acked.contains(key) {
                    continue; // durably held — nothing owed for this device
                }
                let stored = self.trusted_record_for(key);
                let relays = self.effective_relays(*key, stored.as_ref());
                if relays.is_empty() {
                    keep_staged = true;
                } else {
                    current.extend(relays.into_iter().map(|entry| entry.mailbox));
                }
            }
            let staged: BTreeSet<String> =
                entries.iter().map(|entry| entry.relay.clone()).collect();
            for entry in entries {
                if keep_staged || current.contains(&entry.relay) {
                    owed.push(entry);
                } else {
                    tracing::info!(relay = %entry.relay, "releasing an outbox entry no longer owed");
                    self.state.clear_outbox(message, &entry.relay);
                    report.released += 1;
                }
            }
            for relay in current {
                if staged.contains(&relay) {
                    continue;
                }
                self.state
                    .add_outbox(message, &relay, conversation, created_ms)?;
                owed.push(crate::state::OutboxEntry {
                    message,
                    relay,
                    conversation,
                    created_ms,
                });
            }
        }
        Ok(owed)
    }

    /// Reload what one owed delivery needs: the stored envelope and its blob
    /// bytes from the local cache (put there at send). `None` = the envelope
    /// is gone, so no retry could ever fulfil this entry and it is dropped
    /// from the ledger.
    #[allow(clippy::type_complexity)]
    fn reload_owed(
        &self,
        entry: &crate::state::OutboxEntry,
    ) -> Result<Option<(MessageEnvelope, Vec<zink_protocol::EncryptedBlob>)>, Error> {
        let envelope = match self.state.load_envelope(entry.conversation, entry.message) {
            Ok(envelope) => envelope,
            Err(error) => {
                tracing::warn!(%error, "dropping unfulfillable outbox entry");
                self.state.clear_outbox(entry.message, &entry.relay);
                return Ok(None);
            }
        };
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
        Ok(Some((envelope, encrypted)))
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::client::ClientConfig;
    use crate::client::Contact;
    use crate::client::test_kit::{
        chain, deposited_frame, mailbox_only, mailbox_spec, temp_key, temp_root,
    };
    use crate::keystore;
    use crate::ports::clock::{TestClock, TestWallClock};
    use crate::ports::transport::TestTransport;
    use zink_protocol::{
        BlobHash, BlobKind, BlobRef, ContactRecord, DeviceKey, KeyCommitment, MessageCore,
        RelayEntry, Versioned,
    };

    #[tokio::test]
    async fn delivery__should_recover_when_a_dead_relay_returns() {
        // Given: one relay, held silent — the send falls back to the outbox
        // on a deadline only the TestClock moves. The §4 archetype: silence,
        // deterministic timeout, fallback, then recovery — a scenario a real
        // network won't produce on command.
        const DEADLINE: Duration = Duration::from_secs(10);
        const T0: u64 = 1_000_000;
        let relay = DeviceKey::from_seed([81; 32]).public();
        let key_path = temp_key("recover", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        a.add_contact(
            &ContactRecord::new(
                vec![DeviceKey::from_seed([80; 32]).public()],
                vec![],
                vec![RelayEntry {
                    mailbox: mailbox_spec(&relay),
                    relay_url: None,
                }],
            ),
            Some("ghost".to_string()),
        )
        .expect("add the contact");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];

        // When: the send times out and is queued…
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"catch up later".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        assert_eq!(a.state.outbox().len(), 1, "owed to the silent relay");

        // …and the relay comes back: the next dial connects, the deposit
        // is taken
        net.dial.connect(&relay).reply(deposited_frame());
        let report = a.flush_outbox().await.expect("flush");

        // Then: recovered, nothing owed — and no real time passed anywhere
        assert_eq!(report.delivered, 1);
        assert!(a.state.outbox().is_empty(), "the debt is settled");

        let _ = std::fs::remove_dir_all(temp_root("recover"));
    }

    #[tokio::test]
    async fn flush__should_follow_the_recipient_to_their_new_relay() {
        // Given: ghost stored on relay A (held silent) — the send times out
        // and is owed to A. The B3 migration regression (5-relay-lifecycle
        // §3): the relay was reinstalled elsewhere; before R2 this entry
        // stayed pending forever.
        const DEADLINE: Duration = Duration::from_secs(10);
        const T0: u64 = 1_000_000;
        let relay_a = DeviceKey::from_seed([82; 32]).public();
        let relay_b = DeviceKey::from_seed([83; 32]).public();
        let key_path = temp_key("retarget", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay_a);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        let ghost = DeviceKey::from_seed([84; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![ghost.public()],
                vec![],
                mailbox_only(&mailbox_spec(&relay_a)),
            ),
            Some("ghost".to_string()),
        )
        .expect("add");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"see you on the new relay".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));

        // When: ghost's record moves to relay B (the R1 rescan) and the
        // outbox flushes
        a.update_contact(&ContactRecord::new(
            vec![ghost.public()],
            vec![],
            mailbox_only(&mailbox_spec(&relay_b)),
        ))
        .expect("update");
        net.dial.connect(&relay_b).reply(deposited_frame());
        let report = a.flush_outbox().await.expect("flush");

        // Then: the debt followed the record — deposited to B, A released
        // without another dial
        assert_eq!(report.delivered, 1);
        assert_eq!(report.released, 1);
        assert!(a.state.outbox().is_empty(), "the debt is settled");
        assert_eq!(net.dial.dialed(&relay_a), 1, "only the original send");
        assert_eq!(net.dial.dialed(&relay_b), 1);

        let _ = std::fs::remove_dir_all(temp_root("retarget"));
    }

    #[tokio::test]
    async fn flush__should_release_the_debt_once_every_recipient_acked() {
        // Given: a send owed to ghost's silent relay, then a durable ack
        // from ghost's device (a later direct delivery persisted it)
        const DEADLINE: Duration = Duration::from_secs(10);
        const T0: u64 = 1_000_000;
        let relay = DeviceKey::from_seed([85; 32]).public();
        let key_path = temp_key("ack-release", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        let ghost = DeviceKey::from_seed([86; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![ghost.public()],
                vec![],
                mailbox_only(&mailbox_spec(&relay)),
            ),
            Some("ghost".to_string()),
        )
        .expect("add");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"did you get this?".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        let entry = &a.state.outbox()[0];
        a.state
            .add_acks(
                entry.conversation,
                entry.message,
                &BTreeSet::from([ghost.public()]),
            )
            .expect("persist the ack");

        // When
        let report = a.flush_outbox().await.expect("flush");

        // Then: settled without a deposit — the recipient already holds it
        assert_eq!(report.released, 1);
        assert_eq!(report.delivered, 0);
        assert!(a.state.outbox().is_empty(), "the debt is settled");
        assert_eq!(net.dial.dialed(&relay), 1, "only the original send");

        let _ = std::fs::remove_dir_all(temp_root("ack-release"));
    }

    #[tokio::test]
    async fn flush__should_keep_blob_messages_on_their_relays_despite_acks() {
        // Given: an owed message *with a blob*, its one recipient acked —
        // the envelope is held, but blob bytes come from the recipient's
        // relay cache (C3a), so the deposit path is still owed
        const DEADLINE: Duration = Duration::from_secs(10);
        const T0: u64 = 1_000_000;
        let relay = DeviceKey::from_seed([87; 32]).public();
        let key_path = temp_key("blob-guard", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        let ghost = DeviceKey::from_seed([88; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![ghost.public()],
                vec![],
                mailbox_only(&mailbox_spec(&relay)),
            ),
            Some("ghost".to_string()),
        )
        .expect("add");
        let core = MessageCore {
            version: MessageCore::CURRENT,
            conversation: None,
            parents: vec![],
            recipients: vec![ghost.public()],
            sender: a.device.public(),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: vec![],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![BlobRef {
                hash: BlobHash([9; 32]),
                kind: BlobKind::Full,
                key_commit: KeyCommitment([0; 32]),
            }],
        };
        let envelope = MessageEnvelope::new(core, &a.device);
        let id = envelope.id();
        a.state.store_envelope(id, &envelope).expect("store");
        a.state
            .add_outbox(id, &mailbox_spec(&relay), id, T0)
            .expect("owe");
        a.state
            .add_acks(id, id, &BTreeSet::from([ghost.public()]))
            .expect("persist the ack");

        // When: the relay stays silent through the flush
        let (report, ()) = tokio::join!(a.flush_outbox(), async {
            clock.wait_for_sleepers(1).await;
            clock.advance(DEADLINE);
        });

        // Then: not released — the ack proves the envelope, not the bytes
        let report = report.expect("flush");
        assert_eq!(report.released, 0);
        assert_eq!(report.pending, 1);
        assert_eq!(a.state.outbox().len(), 1, "still owed for the blob");

        let _ = std::fs::remove_dir_all(temp_root("blob-guard"));
    }

    #[tokio::test]
    async fn flush__should_keep_staged_relays_for_recipients_without_a_record() {
        // Given: a raw-spec send (`<pubkey>@<relay>`, no stored contact) owed
        // to a silent relay — re-resolution has nothing to say about this
        // recipient, so staged knowledge is all there is
        const DEADLINE: Duration = Duration::from_secs(10);
        const T0: u64 = 1_000_000;
        let relay = DeviceKey::from_seed([89; 32]).public();
        let key_path = temp_key("raw-spec", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        let stranger = DeviceKey::from_seed([90; 32]).public();
        let spec = format!("{}@{}", hex::encode(&stranger.0), mailbox_spec(&relay));
        let recipients = [Contact::parse(&spec).expect("parse")];
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"to a raw address".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(DEADLINE);
            },
        );
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));

        // When: the relay stays silent through the flush too (the hold is a
        // one-shot script; the retry's dial needs its own)
        net.dial.hold(&relay);
        let (report, ()) = tokio::join!(a.flush_outbox(), async {
            clock.wait_for_sleepers(1).await;
            clock.advance(DEADLINE);
        });

        // Then: nothing released — the entry retries the staged dial string
        let report = report.expect("flush");
        assert_eq!(report.released, 0);
        assert_eq!(report.pending, 1);
        assert_eq!(a.state.outbox().len(), 1);
        assert_eq!(net.dial.dialed(&relay), 2, "send, then the retry");

        let _ = std::fs::remove_dir_all(temp_root("raw-spec"));
    }

    #[tokio::test]
    async fn flush__should_inherit_the_original_age_when_retargeting() {
        // Given: an entry owed to relay A since T0, now past the give-up
        // window under a jumped wall clock; ghost's record has moved to
        // relay B — a moved record must not reset the give-up clock
        const T0: u64 = 1_000_000;
        const PAST_GIVE_UP: u64 = T0 + 31 * 24 * 60 * 60 * 1000;
        let relay_a = DeviceKey::from_seed([91; 32]).public();
        let relay_b = DeviceKey::from_seed([92; 32]).public();
        let key_path = temp_key("inherit-age", "a");
        keystore::create(&key_path).expect("key");
        let net = TestTransport::new();
        let wall = TestWallClock::new(PAST_GIVE_UP);
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            TestClock::new(),
            wall,
            net.clone(),
        );
        let ghost = DeviceKey::from_seed([93; 32]);
        let envelope = chain(&a.device, ghost.public(), 1).remove(0);
        let id = envelope.id();
        a.state.store_envelope(id, &envelope).expect("store");
        a.state
            .add_outbox(id, &mailbox_spec(&relay_a), id, T0)
            .expect("owe");
        a.add_contact(
            &ContactRecord::new(
                vec![ghost.public()],
                vec![],
                mailbox_only(&mailbox_spec(&relay_b)),
            ),
            Some("ghost".to_string()),
        )
        .expect("add");

        // When
        let report = a.flush_outbox().await.expect("flush");

        // Then: re-targeted to B at the original age — expired, so no dial
        // anywhere; the entry stays surfaced, not retried
        assert_eq!(report.released, 1);
        assert_eq!(report.expired, 1);
        assert_eq!(report.delivered + report.pending, 0);
        let outbox = a.state.outbox();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].relay, mailbox_spec(&relay_b));
        assert_eq!(outbox[0].created_ms, T0, "age inherited, not reset");
        assert_eq!(net.dial.dialed(&relay_a), 0);
        assert_eq!(net.dial.dialed(&relay_b), 0);

        let _ = std::fs::remove_dir_all(temp_root("inherit-age"));
    }
}
