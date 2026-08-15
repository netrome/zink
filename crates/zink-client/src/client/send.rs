//! Sending (C4a, D5): seal → stage (store + outbox ledger, so an edge can
//! render before delivery finishes) → deliver — direct to each recipient
//! where reach evidence warrants it (`crate::reach`), then one deposit per
//! distinct relay. See `docs/design/direct-delivery.md` and
//! `docs/design/fast-failure.md`.

use std::collections::BTreeSet;

use rand_core::OsRng;
use zink_protocol::{
    BlobDraft, EncryptedBlob, MessageDraft, MessageEnvelope, MessageId, PublicKey, SYNC_ALPN,
    SyncOp, SyncResult, distinct_relays,
};

use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;
use crate::{blobs, net};

use super::history::participants_of;
use super::{Client, Contact};

/// A message that is sealed, stored and ledgered, but not yet delivered —
/// `stage_send`'s output and `deliver`'s input.
///
/// It exists so an edge can **render before it delivers**: the message is
/// already in the store (so history shows it, flagged `pending` by its outbox
/// entries) while the network work happens off the user's path. Nothing is
/// riding on the handoff succeeding — if the process dies here, the ledger
/// still owes the delivery and any later flush pays it.
pub struct StagedSend {
    pub id: MessageId,
    pub conversation: MessageId,
    pub seq: u64,
    envelope: MessageEnvelope,
    blobs: Vec<EncryptedBlob>,
    /// Recipient key → the relays hosting its mailbox: what the
    /// deposit-skip rule needs (see `deliver`).
    hosted: Vec<(PublicKey, Vec<String>)>,
    /// Distinct relay dial strings this message is owed to.
    relays: Vec<String>,
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
    /// Recipient devices that took the message **directly** (D5): peer-to-peer
    /// over the sync ALPN, with a durable ack, no mailbox involved.
    pub direct_recipients: usize,
    /// Relays skipped entirely because direct delivery discharged them —
    /// the metadata-minimization win: for these, the relay never learned the
    /// message existed.
    pub skipped_relays: usize,
}

/// Whom a reply reaches: the resolvable participants, the keys we hold no
/// record for (unreachable — surfaced, not silently dropped), and the keys
/// excluded by an accepted disavowal (D4b — the deliberate stop-include).
pub struct ReplyContacts {
    pub contacts: Vec<Contact>,
    pub unknown: Vec<PublicKey>,
    pub disavowed: Vec<PublicKey>,
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Seal for all recipients, thread into the participant set's
    /// conversation (or start one), deposit once per distinct relay
    /// (idempotent retry), push blobs to each relay's cache.
    ///
    /// Local work first, network second — see `stage_send` for the half an
    /// edge can render before delivery finishes.
    pub async fn send(
        &self,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<SendReceipt, Error> {
        let staged = self.stage_send(contacts, plaintext, blob_drafts)?;
        self.deliver(&staged).await
    }

    /// `send`'s **local** half: seal, store, index and ledger the message,
    /// then stop. No network. This is everything an edge needs to *show* the
    /// message, so it can render at once and run `deliver` off the user's
    /// path — a send's latency is delivery's, and delivery can be slow for
    /// honest reasons (an unreachable relay costs its whole deadline).
    ///
    /// Safe to hand off: the outbox entry is written before this returns, so
    /// even if the process dies before `deliver` runs, a later flush finishes
    /// the job (live-delivery.md §2). Nothing is lost by not waiting.
    pub fn stage_send(
        &self,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<StagedSend, Error> {
        let draft = self.send_draft(contacts)?;
        self.stage(draft, plaintext, blob_drafts, contacts)
    }

    /// The draft `send` threads: into the participant set's conversation if we
    /// know one, else a fresh genesis.
    fn send_draft(&self, contacts: &[Contact]) -> Result<MessageDraft, Error> {
        if contacts.is_empty() {
            return Err(Error::NoRecipients);
        }
        let recipients: Vec<PublicKey> = contacts.iter().flat_map(|c| c.keys.clone()).collect();
        let participants: BTreeSet<PublicKey> = recipients
            .iter()
            .copied()
            .chain([self.device.public()])
            .collect();
        // Send-to-self makes the *sealed* set the recorded one (D3c), so a
        // send-by-name looks up the device-extended set first — else every
        // post-pairing send would miss its own conversation and fork. The
        // bare set stays as fallback: it finds pre-pairing conversations,
        // which the next send re-records under the grown set.
        let extended: BTreeSet<PublicKey> = participants
            .iter()
            .copied()
            .chain(self.own_keys())
            .collect();
        let existing = self
            .state
            .conversation_for(&extended)
            .or_else(|| self.state.conversation_for(&participants));
        match existing {
            Some(conversation) => self.threaded_draft(conversation, recipients),
            None => Ok(MessageDraft {
                conversation: None,
                parents: vec![],
                recipients,
                seq: 0,
                logical: 0,
                timestamp_ms: self.wall_clock.now_ms(),
                plaintext: vec![],
                blobs: vec![],
            }),
        }
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
    ) -> Result<SendReceipt, Error> {
        let staged = self.stage_send_in(conversation, contacts, plaintext, blob_drafts)?;
        self.deliver(&staged).await
    }

    /// `send_in`'s local half — see `stage_send`.
    pub fn stage_send_in(
        &self,
        conversation: MessageId,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<StagedSend, Error> {
        if contacts.is_empty() {
            return Err(Error::NoRecipients);
        }
        let recipients: Vec<PublicKey> = contacts.iter().flat_map(|c| c.keys.clone()).collect();
        let draft = self.threaded_draft(conversation, recipients)?;
        self.stage(draft, plaintext, blob_drafts, contacts)
    }

    /// Whom a reply in this conversation goes to: the current membership
    /// (heads-based, groups.md §2) minus this device. A member resolves to
    /// a route through their contact record **or learned records** —
    /// address, don't trust (§2): promotion to a contact is never required
    /// to keep a group whole. A member with no route at all is STILL a
    /// recipient (empty relays: sealed to, delivered nowhere) — dropping
    /// them from the signed recipients would shrink membership for
    /// everyone through this reply's head. Membership is not
    /// deliverability: their copy stays fetchable via peer sync once they
    /// have a route. Such keys are also listed in `unknown`, so the edge
    /// can say so.
    pub fn reply_contacts(&self, conversation: MessageId) -> Result<ReplyContacts, Error> {
        let me = self.device.public();
        let mut contacts = Vec::new();
        let mut unknown = Vec::new();
        let mut disavowed = Vec::new();
        for key in self.membership(conversation)? {
            if key == me {
                continue;
            }
            // An accepted disavowal is the deliberate stop-include (D4b,
            // web-of-trust.md §4): the key stays in history, stops being
            // addressed — unlike routelessness, exclusion is the point.
            // Explicit acts (send to the entry by name) still work: that
            // is the manual override.
            if self
                .disavowals(key)?
                .iter()
                .any(|disavowal| disavowal.excludes)
            {
                disavowed.push(key);
                continue;
            }
            // Contact record, or a recognized own device's (D3c): a
            // sibling in the membership routes through the devices store.
            let stored = self.trusted_record_for(&key);
            let relays: Vec<String> = self
                .effective_relays(key, stored.as_ref())
                .into_iter()
                .map(|entry| entry.mailbox)
                .collect();
            if relays.is_empty() {
                unknown.push(key);
            }
            contacts.push(Contact {
                keys: vec![key],
                relays,
            });
        }
        Ok(ReplyContacts {
            contacts,
            unknown,
            disavowed,
        })
    }

    /// A draft threaded onto the stored DAG's heads (body filled by
    /// `finish_send`).
    fn threaded_draft(
        &self,
        conversation: MessageId,
        recipients: Vec<PublicKey>,
    ) -> Result<MessageDraft, Error> {
        let dag = self.state.load_dag(conversation)?;
        Ok(MessageDraft {
            conversation: Some(conversation),
            parents: dag.heads(),
            recipients,
            seq: dag.next_seq(&self.device.public()),
            logical: dag.next_logical(),
            timestamp_ms: self.wall_clock.now_ms(),
            plaintext: vec![],
            blobs: vec![],
        })
    }

    /// The local half of every send: seal, persist (envelope, own-blob cache,
    /// outbox ledger, participant mapping), and stop. Everything here is
    /// filesystem work — no network, nothing that can hang.
    fn stage(
        &self,
        mut draft: MessageDraft,
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
        contacts: &[Contact],
    ) -> Result<StagedSend, Error> {
        draft.plaintext = plaintext;
        draft.blobs = blob_drafts;
        // Send-to-self (D3c, multi-device.md §5): recognized own devices
        // are honest members of every conversation this device speaks in —
        // appended to the signed recipients and deposited to like any
        // recipient. The sending device itself stays unlisted (the C3
        // self-wrap covers its own copy). Appended here, after the
        // participant-set lookup in `send`: the user-addressed set keeps
        // finding the conversation, and the grown set is what gets
        // recorded below — same latest-writer-wins index as any add.
        let mut device_contacts = Vec::new();
        for (key, record) in self.state.recognized_devices() {
            if key == self.device.public() || draft.recipients.contains(&key) {
                continue;
            }
            draft.recipients.push(key);
            device_contacts.push(Contact {
                keys: vec![key],
                relays: self
                    .effective_relays(key, Some(&record))
                    .into_iter()
                    .map(|entry| entry.mailbox)
                    .collect(),
            });
        }
        let seq = draft.seq;
        let existing = draft.conversation;
        let sealed =
            MessageEnvelope::seal(draft, &self.device, &mut OsRng).map_err(Error::Crypto)?;
        let id = sealed.envelope.id();
        let conversation = existing.unwrap_or(id);
        self.state.store_envelope(conversation, &sealed.envelope)?;
        // The participant-set index (groups.md §3): every send maps its
        // message's set -> conversation, exactly like `remember` does on
        // receipt -- sender and receivers agree by construction, so adding
        // a member via a reply can't artifact-fork the adder's next
        // send-by-name. Latest writer wins per set.
        let participants: BTreeSet<PublicKey> = participants_of(&sealed.envelope).collect();
        self.state
            .record_conversation(&participants, conversation)?;
        // Own blobs go straight into the local cache: they get pushed to the
        // *recipients'* relays, so this is the only place we can refetch
        // them from when rendering our own history.
        for blob in &sealed.blobs {
            self.state.save_blob(&blob.hash, &blob.bytes)?;
        }

        // Ledger before network (live-delivery.md §2): a crash or failure
        // from here on leaves entries a later flush retries idempotently.
        // This is also what makes handing delivery off safe (`stage_send`).
        let relays = distinct_relays(
            contacts
                .iter()
                .chain(device_contacts.iter())
                .map(|c| c.relays.clone()),
        );
        let now = self.wall_clock.now_ms();
        for relay in &relays {
            self.state.add_outbox(id, relay, conversation, now)?;
        }
        Ok(StagedSend {
            id,
            conversation,
            seq,
            envelope: sealed.envelope,
            blobs: sealed.blobs,
            hosted: contacts
                .iter()
                .chain(device_contacts.iter())
                .flat_map(|contact| {
                    contact
                        .keys
                        .iter()
                        .map(|&key| (key, contact.relays.clone()))
                        .collect::<Vec<_>>()
                })
                .collect(),
            relays,
        })
    }

    /// The network half: hand the message to the recipients directly where we
    /// can, deposit to the mailboxes that still need it, and discharge the
    /// ledger as each path succeeds. One relay failing never aborts the
    /// others; what failed stays in the outbox. Errors only when *nothing*
    /// took it — the message is stored and queued either way, so the error
    /// means "queued", not "lost".
    ///
    /// Idempotent by construction (deposits dedup by id, blob pushes by hash,
    /// a re-`Deliver` re-acks), so a caller may run it again — or leave it to
    /// `flush_outbox`.
    pub async fn deliver(&self, staged: &StagedSend) -> Result<SendReceipt, Error> {
        // NOTE: the outbox is NOT flushed here. Flushing on the send path
        // coupled a new message's latency to the health of the *backlog* —
        // a slow/stuck queued delivery delayed every fresh send. The backlog
        // is retried off this path (recv, subscription reconnect, and the
        // edge's post-send background flush), so a fresh send pays only for
        // its own delivery.
        let StagedSend {
            id,
            conversation,
            seq,
            envelope,
            blobs,
            hosted,
            relays,
        } = staged;
        let (id, conversation) = (*id, *conversation);

        // Direct delivery (D5): hand the envelope to the recipients
        // themselves first. A `Stored` ack is a durable store, so the
        // mailbox is not needed for that device.
        let direct = self
            .deliver_direct(envelope, &envelope.core.recipients)
            .await;
        // De7: persist the ack before anything else can fail. It is the one
        // fact here that cannot be reconstructed later — an outbox-clear
        // looks identical whether the recipient acked or a relay simply
        // took the deposit. Best-effort: a device that told us it stored
        // the message *has* it, so a failed sidecar write must not fail a
        // send; it costs the confirmation, not the delivery.
        if let Err(error) = self.state.add_acks(conversation, id, &direct) {
            tracing::warn!(%error, "could not record a delivery confirmation");
        }

        // Which relays direct delivery discharges entirely (direct-delivery
        // .md §3): every recipient the relay hosts acked. The outbox ledger
        // is per (message, relay), and one relay can host several recipients
        // — a deposit fans out to all of them — so this must be *all*, not
        // *any*, or a group send silently loses the un-acked members.
        //
        // Blobs keep their relays on the path regardless: a recipient fetches
        // blob bytes from its own relay's cache (C3a), so an image message
        // still needs the push, and pushing while skipping the deposit buys
        // little (the relay sees the sender either way). Peer blob transfer
        // would close that gap — a later slice.
        let discharged = |relay: &String| {
            let here: Vec<&PublicKey> = hosted
                .iter()
                .filter(|(_, relays)| relays.contains(relay))
                .map(|(key, _)| key)
                .collect();
            // A relay hosting no recipient at all is never "discharged" —
            // `all` over nothing is vacuously true, which would be wrong.
            blobs.is_empty() && !here.is_empty() && here.iter().all(|key| direct.contains(*key))
        };

        // Concurrent per relay (De6d): relays are independent, and serially
        // an unreachable one made every *other* relay wait out its deadline
        // first — n down relays cost n × `connect_timeout` instead of one.
        // Same shape as `deliver_direct`'s dials and De3's who-is fan-out.
        let mut skipped_relays = 0;
        let deliveries = relays.iter().filter(|relay| {
            if discharged(relay) {
                // The philosophy win (§1): for this message the relay learns
                // nothing at all — not that it exists, not who it was for.
                tracing::info!(relay, "delivered directly; skipping the deposit");
                self.state.clear_outbox(id, relay);
                skipped_relays += 1;
                return false;
            }
            true
        });
        // Collected before awaiting: `discharged` borrows `direct`, and the
        // filter's side effects (skip logging, outbox clearing) belong to
        // this synchronous pass, not to the concurrent one.
        let deliveries: Vec<&String> = deliveries.collect();
        let outcomes = n0_future::join_all(deliveries.into_iter().map(|relay| async move {
            match self.deliver_to_relay(relay, envelope, blobs).await {
                Ok(()) => {
                    self.state.clear_outbox(id, relay);
                    None
                }
                Err(error) => {
                    tracing::warn!(relay, %error, "delivery failed; queued for retry");
                    Some(error.to_string())
                }
            }
        }))
        .await;
        let mut pending_relays = 0;
        let mut last_error = String::new();
        for error in outcomes.into_iter().flatten() {
            pending_relays += 1;
            last_error = error;
        }
        // "Nothing took it" now includes the direct path: a send whose every
        // relay failed is still delivered if the peers themselves accepted it
        // — that is the whole point of D5 (and what makes a conversation
        // survive the relay going down mid-flight).
        if pending_relays == relays.len() && !relays.is_empty() && direct.is_empty() {
            return Err(Error::AllRelaysPending(last_error));
        }
        Ok(SendReceipt {
            id,
            conversation,
            seq: *seq,
            blob_count: blobs.len(),
            relay_count: relays.len(),
            pending_relays,
            direct_recipients: direct.len(),
            skipped_relays,
        })
    }

    /// Try to hand `envelope` straight to each recipient device (D5,
    /// direct-delivery.md §3): dial by key on the sync ALPN (D0b
    /// connectivity — holepunched direct, relay-routed as fallback) and count
    /// only a durable `Stored` ack. Returns the keys that acked; every other
    /// recipient falls back to its mailbox.
    ///
    /// Concurrent, so one offline recipient never serializes the rest, and
    /// budgeted per recipient by the reach ledger — the speculation has to
    /// stay off the send's critical path, so a recipient in failure cooldown
    /// costs no dial at all (the budget tiers live on `crate::reach`'s
    /// `dial_budget`). Recipients with no dialable route (mailbox-only
    /// knowledge, no relay URL) are skipped without a dial. Reachability is
    /// still the only presence signal (§2) — this just stops us re-asking a
    /// question we already know the answer to.
    async fn deliver_direct(
        &self,
        envelope: &MessageEnvelope,
        recipients: &[PublicKey],
    ) -> BTreeSet<PublicKey> {
        let now = self.wall_clock.now_ms();
        let me = self.device.public();
        let mut targets = Vec::new();
        for &key in recipients {
            if key == me {
                continue;
            }
            let Some(budget) = self
                .reach
                .dial_budget(&key, now, self.config.connect_timeout)
            else {
                tracing::debug!("direct: recently unreachable; mailbox only");
                continue;
            };
            let stored = self.trusted_record_for(&key);
            match self.peer_addr_for(key, stored.as_ref()) {
                Ok(addr) => targets.push((key, addr, budget)),
                Err(_) => tracing::debug!("direct: no dialable route; mailbox only"),
            }
        }
        let dialed = !targets.is_empty();
        let pushes = targets.into_iter().map(|(key, addr, timeout)| async move {
            let connection =
                match net::connect_peer(&self.transport, &addr, SYNC_ALPN, timeout, &self.clock)
                    .await
                {
                    Ok(connection) => connection,
                    Err(error) => {
                        tracing::debug!(%error, "direct: recipient unreachable");
                        self.reach.note_failed(&key, now);
                        return None;
                    }
                };
            let op = SyncOp::Deliver {
                envelope: Box::new(envelope.clone()),
            };
            match net::sync_request(&connection, op).await {
                // The ack that licenses skipping the mailbox: stored, not
                // merely received.
                Ok(SyncResult::Stored) => {
                    self.reach.note_delivered(&key, now);
                    Some(key)
                }
                Ok(SyncResult::NotHeld) => {
                    tracing::debug!("direct: recipient declined; falling back to its mailbox");
                    // Reachable — it answered — so no cooldown: the reasons a
                    // peer declines are indistinguishable on the wire
                    // (SPEC §5.2) and some are *per message* (an envelope it
                    // can't open), so a decline must not suppress the next
                    // message. Reaching a live peer is cheap; only
                    // unreachability is worth remembering.
                    self.reach.note_seen(&key, now);
                    None
                }
                Ok(other) => {
                    tracing::warn!(?other, "direct: unexpected response");
                    self.reach.note_failed(&key, now);
                    None
                }
                Err(error) => {
                    tracing::debug!(%error, "direct: delivery failed");
                    self.reach.note_failed(&key, now);
                    None
                }
            }
        });
        let acked: BTreeSet<PublicKey> = n0_future::join_all(pushes)
            .await
            .into_iter()
            .flatten()
            .collect();
        // One write for the whole fan-out, after every dial has reported
        // (De6b) — it carries the failures *and* the clears. Skipped when we
        // dialed nobody: a send to a peer already in cooldown must cost
        // nothing, and that includes not rewriting the file to say so.
        if dialed {
            self.persist_unreachable(now);
        }
        acked
    }

    /// Write the *negative* half of the reach ledger to disk (De6b), so the
    /// next process doesn't re-pay a dial deadline to learn what this one
    /// learned. Best-effort — a failed write costs one extra dial next time,
    /// which is not worth failing a send over.
    fn persist_unreachable(&self, now: u64) {
        let entries = self.reach.unreachable_snapshot(now);
        if let Err(error) = self.state.save_unreachable(&entries) {
            tracing::debug!(%error, "could not persist unreachable peers");
        }
    }

    /// One relay's full delivery: deposit (idempotent retry inside), then
    /// every blob push. Only a fully-served relay counts as delivered.
    pub(super) async fn deliver_to_relay(
        &self,
        relay: &str,
        envelope: &MessageEnvelope,
        encrypted_blobs: &[zink_protocol::EncryptedBlob],
    ) -> Result<(), Error> {
        net::deposit_with_retry(
            &self.transport,
            relay,
            envelope,
            self.config.connect_timeout,
            &self.clock,
        )
        .await?;
        if !encrypted_blobs.is_empty() {
            blobs::push_blobs(
                &self.transport,
                relay,
                encrypted_blobs,
                self.config.connect_timeout,
                &self.clock,
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapters::system_clock::SystemClock;
    use crate::client::test_kit::{
        deposited_envelopes, deposited_frame, loop_client, mailbox_only, mailbox_spec, message,
        open_homed, open_homed_with, record_with_dead_mailbox, routed_record, script_drain,
        sealed_for, spawn_test_relay, temp_key, temp_root,
    };
    use crate::client::{ClientConfig, Received};
    use crate::ports::clock::{TestClock, TestWallClock};
    use crate::ports::transport::{Home, Loopback, Peer, TestTransport};
    use crate::{hex, keystore};
    use zink_protocol::{ContactRecord, DeviceKey, RelayEntry};

    #[tokio::test]
    async fn send_in__should_record_the_grown_participant_set() {
        // Given: an existing 1:1 A↔B; C joins via a reply. Previously the
        // sender's own index never learned the grown set, so the next
        // send-by-name forked a parallel conversation while everyone
        // else's `remember` threaded the old one (groups.md §3).
        let key_path = temp_key("index", "a");
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
        let b = DeviceKey::from_seed([21; 32]).public();
        let c = DeviceKey::from_seed([22; 32]).public();
        let genesis = message(&a.device, vec![b], None, vec![], 0, 0);
        let conversation = genesis.id();
        a.state
            .store_envelope(conversation, &genesis)
            .expect("store");
        a.state
            .record_conversation(&BTreeSet::from([a.public_key(), b]), conversation)
            .expect("map");
        let contact = |key: PublicKey| Contact {
            keys: vec![key],
            relays: vec![format!("{}@203.0.113.7:1", hex::encode(&key.0))],
        };

        // When: replying with C added — the relay is unreachable, so the
        // send reports "queued", but store + index are written first
        let result = a
            .send_in(
                conversation,
                &[contact(b), contact(c)],
                b"welcome".to_vec(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));

        // Then: the sender's own index maps the grown set — a send-by-name
        // to {B, C} now threads instead of forking
        let grown = BTreeSet::from([a.public_key(), b, c]);
        assert_eq!(a.state.conversation_for(&grown), Some(conversation));

        let _ = std::fs::remove_dir_all(temp_root("index"));
    }

    #[tokio::test]
    async fn send__should_deliver_directly_when_the_mailbox_is_unreachable() {
        // Given: A and B wired directly, each holding the other's record.
        // B's mailbox dial string is dead — but B acks the push, so the
        // deposit is discharged and the dead mailbox is never even dialed
        // (an attempt would panic as unscripted).
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("direct", "a", &wire);
        let (b, _b_net, _b_clock) = loop_client("direct", "b", &wire);
        let relay_a = DeviceKey::from_seed([131; 32]).public();
        let relay_b = DeviceKey::from_seed([132; 32]).public();
        // The serving gate (D0c) covers `Deliver` too: B accepts a push
        // only from a contact.
        b.add_contact(
            &routed_record(a.public_key(), &relay_a),
            Some("a".to_string()),
        )
        .expect("B adds A");
        a.add_contact(
            &routed_record(b.public_key(), &relay_b),
            Some("b".to_string()),
        )
        .expect("A adds B");
        // The live sink is the *only* signal for a direct arrival: no
        // mailbox means no nudge to drain.
        let live: std::sync::Arc<std::sync::Mutex<Vec<Received>>> = Default::default();
        let sink = live.clone();
        b.on_direct_delivery(move |messages| {
            sink.lock().expect("live lock").extend(messages);
        });

        // When
        let receipt = a
            .send(
                &[a.resolve_contact("b").expect("resolve")],
                b"straight to you".to_vec(),
                vec![],
            )
            .await
            .expect("send");

        // Then: delivered peer-to-peer, and the relay never heard about it
        assert_eq!(receipt.direct_recipients, 1);
        assert_eq!(receipt.skipped_relays, 1, "the deposit was skipped");
        assert_eq!(receipt.pending_relays, 0, "nothing owed");
        assert!(
            a.state.outbox().is_empty(),
            "a discharged ledger leaves no entry"
        );

        // …and B has it, readable, live
        let history = b.history(receipt.conversation).expect("B history");
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].body.as_deref(),
            Ok(b"straight to you".as_slice())
        );
        let live = live.lock().expect("live lock");
        assert_eq!(live.len(), 1, "the sink fired");
        assert!(live[0].relay.is_none(), "no relay was on the path");

        let _ = std::fs::remove_dir_all(temp_root("direct"));
    }

    #[tokio::test]
    async fn deliver__should_record_the_recipients_own_ack_as_a_confirmation() {
        // Given: A and B able to reach each other peer-to-peer (D5), so the
        // send earns a `Stored` ack from B's *own device key* — the only
        // party whose word means "delivered" (a relay's `Deposited` doesn't).
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("confirm", "a", &wire);
        let (b, _b_net, _b_clock) = loop_client("confirm", "b", &wire);
        let relay_a = DeviceKey::from_seed([133; 32]).public();
        let relay_b = DeviceKey::from_seed([134; 32]).public();
        b.add_contact(
            &routed_record(a.public_key(), &relay_a),
            Some("a".to_string()),
        )
        .expect("B adds A");
        a.add_contact(
            &routed_record(b.public_key(), &relay_b),
            Some("b".to_string()),
        )
        .expect("A adds B");

        // When
        let receipt = a
            .send(
                &[a.resolve_contact("b").expect("resolve")],
                b"did you get this".to_vec(),
                vec![],
            )
            .await
            .expect("send");
        assert_eq!(receipt.direct_recipients, 1, "the ack came back");

        // Then: the sender's history names the device that confirmed it —
        // the D5 ack, which before De7 was computed and thrown away.
        let history = a.history(receipt.conversation).expect("A history");
        assert_eq!(history[0].confirmed, vec![b.public_key()]);
        assert!(
            history[0].owed_since_ms.is_none(),
            "and nothing is still owed"
        );

        // …and it survives a reopen: the ack is transient, the record isn't.
        drop(a);
        let key_path = temp_key("confirm", "a");
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            TestClock::new(),
            SystemClock,
            TestTransport::new(),
        );
        assert_eq!(
            a.history(receipt.conversation).expect("A history")[0].confirmed,
            vec![b.public_key()],
            "the confirmation is persisted, not in-memory"
        );

        // …while B's own copy claims nothing: a confirmation is something a
        // *sender* holds about a recipient, never a self-report.
        let received = b.history(receipt.conversation).expect("B history");
        assert!(
            received[0].confirmed.is_empty(),
            "the recipient does not confirm to itself"
        );

        let _ = std::fs::remove_dir_all(temp_root("confirm"));
    }

    #[tokio::test]
    async fn stage_send__should_store_and_ledger_before_any_delivery() {
        // Given: a recipient whose relay is unreachable — delivery will take
        // the full deadline, which is exactly what an edge must not wait for
        // before rendering.
        let key_path = temp_key("staged", "a");
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
        let absent = DeviceKey::from_seed([61; 32]).public();
        let contact = Contact {
            keys: vec![absent],
            relays: vec![format!("{}@203.0.113.9:1", hex::encode(&absent.0))],
        };

        // When: only the local half runs
        let staged = a
            .stage_send(&[contact], b"render me now".to_vec(), vec![])
            .expect("stage");

        // Then: it is already readable history, flagged as still owed…
        let history = a.history(staged.conversation).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].body.as_deref(), Ok(b"render me now".as_slice()));
        assert!(
            history[0].owed_since_ms.is_some(),
            "the ledger owes its delivery"
        );
        assert_eq!(a.state.outbox().len(), 1);

        // …and delivery is a separate step that a crash can't lose: even
        // without `deliver`, the flush path owes the same entry.
        let report = a.flush_outbox().await.expect("flush");
        assert_eq!(report.pending, 1, "still owed after a failed retry");
        assert!(
            a.history(staged.conversation).expect("history")[0]
                .owed_since_ms
                .is_some(),
            "and still honestly marked pending"
        );

        let _ = std::fs::remove_dir_all(temp_root("staged"));
    }

    #[tokio::test]
    async fn send__should_keep_delivering_after_the_relays_disappear() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): an established QUIC path surviving relay shutdown.
        // Given: A and B, each homed to its own relay, already talking —
        // the first send rendezvouses through B's relay and establishes a
        // peer path.
        let (relay_a, url_a) = spawn_test_relay().await;
        let (relay_b, url_b) = spawn_test_relay().await;
        let a = open_homed_with("survive", "a", &url_a, Duration::from_millis(300)).await;
        let b = open_homed("survive", "b", &url_b).await;
        b.add_contact(
            &record_with_dead_mailbox(a.public_key(), &url_a),
            Some("a".to_string()),
        )
        .expect("B adds A");
        a.add_contact(
            &record_with_dead_mailbox(b.public_key(), &url_b),
            Some("b".to_string()),
        )
        .expect("A adds B");
        b.transport.online().await;
        let first = a
            .send(
                &[a.resolve_contact("b").expect("resolve")],
                b"before".to_vec(),
                vec![],
            )
            .await
            .expect("first send");
        assert_eq!(first.direct_recipients, 1, "the peer path is established");
        // The ack alone doesn't prove a *direct* path — a `Stored` ack rides
        // relay-routed QUIC just as happily, and under a loaded suite the
        // holepunch sometimes hasn't finished yet. The premise of killing
        // the relays is that a direct path exists to survive on, so wait for
        // that fact itself. Reactive, on the client's own (real) clock: the
        // bound is never paid on success — it fires only when holepunching
        // genuinely failed, the regression this smoke exists to catch.
        a.clock
            .timeout(
                Duration::from_secs(15),
                a.transport.await_direct_path(
                    &Peer {
                        key: b.public_key(),
                        relays: vec![url_b.clone()],
                        sockets: vec![],
                    },
                    SYNC_ALPN,
                ),
            )
            .await
            .expect("holepunch a direct path within 15s");

        // When: both relay services go away completely — no rendezvous, no
        // mailbox, nothing. This is "restart the relay mid-conversation".
        relay_a.shutdown().await.expect("shut down A's relay");
        relay_b.shutdown().await.expect("shut down B's relay");

        // Then: the conversation carries on over the established path
        let second = a
            .send_in(
                first.conversation,
                &[a.resolve_contact("b").expect("resolve")],
                b"after".to_vec(),
                vec![],
            )
            .await
            .expect("second send");
        assert_eq!(second.direct_recipients, 1, "still delivered peer-to-peer");
        assert_eq!(second.skipped_relays, 1);
        let history = b.history(first.conversation).expect("B history");
        assert_eq!(history.len(), 2, "both messages landed at B");
        assert_eq!(history[1].body.as_deref(), Ok(b"after".as_slice()));

        let _ = std::fs::remove_dir_all(temp_root("survive"));
    }

    #[tokio::test]
    async fn send__should_fall_back_to_the_mailbox_when_the_peer_is_offline() {
        // REAL-NETWORK SMOKE (P7, transport.md §8): a real dial failing within its budget.
        // Given: B's record is known (relay URL + dead mailbox) but B is not
        // running at all — the ordinary case, and the one that must not
        // regress: a direct attempt that fails costs a bounded moment and
        // then behaves exactly like pre-D5.
        let (_relay_a, url_a) = spawn_test_relay().await;
        let a = open_homed_with("offline", "a", &url_a, Duration::from_millis(300)).await;
        let absent = DeviceKey::from_seed([41; 32]).public();
        a.add_contact(
            &record_with_dead_mailbox(absent, &url_a),
            Some("ghost".to_string()),
        )
        .expect("A adds the absent peer");

        // When: every path fails — no peer to dial, no mailbox to deposit in
        let result = a
            .send(
                &[a.resolve_contact("ghost").expect("resolve")],
                b"are you there".to_vec(),
                vec![],
            )
            .await;

        // Then: "queued", not "lost" — the C4a semantics, unchanged by D5
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        assert_eq!(a.state.outbox().len(), 1, "the mailbox is still owed");

        let _ = std::fs::remove_dir_all(temp_root("offline"));
    }

    #[tokio::test]
    async fn unreachable_peer__should_persist_so_the_next_process_skips_the_dial() {
        // Given: an absent peer with a dialable record (a relay url licenses
        // the direct path). The first send learns it is offline the expensive
        // way — a held dial that runs out its 600 ms cold-probe budget, fired
        // by the TestClock. Pre-De6b that lesson died with the process, so
        // the CLI re-paid it on *every* invocation.
        const T0: u64 = 1_700_000_000_000;
        let absent = DeviceKey::from_seed([57; 32]).public();
        let relay = DeviceKey::from_seed([58; 32]).public();
        let record = ContactRecord::new(
            vec![absent],
            vec![],
            vec![RelayEntry {
                mailbox: mailbox_spec(&relay),
                relay_url: Some("http://203.0.113.1:1".to_string()),
            }],
        );
        let key_path = temp_key("reachcache", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&absent); // the peer is gone
        net.dial.connect(&relay).reply(deposited_frame()); // its mailbox is fine
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            clock.clone(),
            TestWallClock::new(T0),
            net.clone(),
        );
        a.add_contact(&record, Some("ghost".to_string()))
            .expect("A adds the absent peer");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];

        // When: one send pays the probe (the deposit itself lands)
        let (receipt, ()) = tokio::join!(
            a.send(&recipients, b"anyone home".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(Duration::from_millis(600));
            },
        );
        assert_eq!(receipt.expect("send").direct_recipients, 0);

        // Then: the failure is on disk…
        let persisted = a.state.unreachable();
        assert_eq!(persisted.len(), 1, "one failed peer recorded");
        assert_eq!(persisted[0].0, absent.0, "…and it is the peer we dialed");
        drop(a);

        // …and a fresh process inherits it: within the cooldown, the next
        // send makes NO dial to that peer at all — not a shorter one, none.
        let net = TestTransport::new();
        net.dial.connect(&relay).reply(deposited_frame());
        let reopened = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig::default(),
            TestClock::new(),
            TestWallClock::new(T0 + 5_000),
            net.clone(),
        );
        reopened
            .send(
                &[reopened.resolve_contact("ghost").expect("resolve")],
                b"still there?".to_vec(),
                vec![],
            )
            .await
            .expect("send");
        assert_eq!(
            net.dial.dialed(&absent),
            0,
            "a known-offline peer must cost a fresh process nothing"
        );

        let _ = std::fs::remove_dir_all(temp_root("reachcache"));
    }

    #[tokio::test]
    async fn delivery__should_pay_one_deadline_for_two_dead_relays() {
        // Given: a contact whose record names TWO mailboxes, both held
        // silent by the dial double — each deposit runs until a deadline
        // only the TestClock moves. No relay url, so nothing is dialed
        // directly; the only network cost in this test is the two deposits.
        const DEADLINE: Duration = Duration::from_secs(10);
        let relay_key = |seed: u8| DeviceKey::from_seed([seed; 32]).public();
        let key_path = temp_key("twodead", "a");
        keystore::create(&key_path).expect("key");
        let clock = TestClock::new();
        let net = TestTransport::new();
        net.dial.hold(&relay_key(74));
        net.dial.hold(&relay_key(75));
        let a = Client::with_transport(
            keystore::load(&key_path).expect("load key"),
            &key_path,
            ClientConfig {
                connect_timeout: DEADLINE,
                ..Default::default()
            },
            clock.clone(),
            SystemClock,
            net.clone(),
        );
        a.add_contact(
            &ContactRecord::new(
                vec![DeviceKey::from_seed([73; 32]).public()],
                vec![],
                vec![
                    RelayEntry {
                        mailbox: mailbox_spec(&relay_key(74)),
                        relay_url: None,
                    },
                    RelayEntry {
                        mailbox: mailbox_spec(&relay_key(75)),
                        relay_url: None,
                    },
                ],
            ),
            Some("ghost".to_string()),
        )
        .expect("add the contact");
        let recipients = [a.resolve_contact("ghost").expect("resolve")];

        // When: one send fans out to both, and time moves only after BOTH
        // deadline timers are parked — serial fan-out would park one at a
        // time and hang `wait_for_sleepers(2)` (De6d)
        let (result, ()) = tokio::join!(
            a.send(&recipients, b"into the void".to_vec(), vec![]),
            async {
                clock.wait_for_sleepers(2).await;
                clock.advance(DEADLINE);
            },
        );

        // Then: queued for both
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        assert_eq!(a.state.outbox().len(), 2, "both relays still owed");

        // And: the outbox flush pays the same way, over the same two entries
        net.dial.hold(&relay_key(74));
        net.dial.hold(&relay_key(75));
        let (report, ()) = tokio::join!(a.flush_outbox(), async {
            clock.wait_for_sleepers(2).await;
            clock.advance(DEADLINE);
        });
        assert_eq!(
            report.expect("flush").pending,
            2,
            "still nowhere to deliver"
        );

        let _ = std::fs::remove_dir_all(temp_root("twodead"));
    }

    #[tokio::test]
    async fn send_to_self__should_carry_a_paired_device_into_a_conversation() {
        // Given: alice ↔ phone mutual contacts; the laptop pairs with the
        // phone (two one-way recognize acts — the laptop first, so the
        // record the phone stores and serves carries the reverse vouch).
        let wire = Loopback::new();
        let (alice, a_net, a_clock) = loop_client("multidevice", "alice", &wire);
        let (phone, p_net, p_clock) = loop_client("multidevice", "phone", &wire);
        let (laptop, l_net, l_clock) = loop_client("multidevice", "laptop", &wire);
        let relay_a = DeviceKey::from_seed([121; 32]).public();
        let relay_p = DeviceKey::from_seed([122; 32]).public();
        let relay_l = DeviceKey::from_seed([123; 32]).public();
        let spec = |relay: &PublicKey| format!("{}#http://203.0.113.1:1", mailbox_spec(relay));
        alice
            .set_profile("Alice", &[spec(&relay_a)])
            .await
            .expect("alice profile");
        phone
            .set_profile("mårten phone", &[spec(&relay_p)])
            .await
            .expect("phone profile");
        laptop
            .set_profile("mårten laptop", &[spec(&relay_l)])
            .await
            .expect("laptop profile");
        let record_p = phone.my_record().expect("phone record");
        alice
            .add_contact(&record_p, Some("mårten phone".into()))
            .expect("alice adds phone");
        phone
            .add_contact(
                &alice.my_record().expect("alice record"),
                Some("Alice".into()),
            )
            .expect("phone adds alice");
        laptop
            .recognize_device(&record_p)
            .expect("laptop recognizes phone");
        let record_l = laptop.my_record().expect("laptop record");
        phone
            .recognize_device(&record_l)
            .expect("phone recognizes laptop");
        // The laptop reports what its own device stored directly (D5).
        let (direct_tx, mut direct_rx) = tokio::sync::mpsc::unbounded_channel();
        laptop.on_direct_delivery(move |batch| {
            let _ = direct_tx.send(batch);
        });

        // When: the phone's next organic message to alice — alice held
        // unreachable (her copy takes her mailbox), the laptop acked
        // directly from its first inclusion (the signed recipients ARE the
        // announcement)
        p_net.dial.hold(&alice.public_key());
        let ra_hi = p_net.dial.connect(&relay_a);
        ra_hi.reply(deposited_frame());
        let to_alice = [phone.resolve_contact("Alice").expect("resolve")];
        let (receipt, ()) =
            tokio::join!(phone.send(&to_alice, b"hi alice".to_vec(), vec![]), async {
                p_clock.wait_for_sleepers(1).await;
                p_clock.advance(Duration::from_millis(600));
            },);
        let receipt = receipt.expect("phone sends");
        let conv = receipt.conversation;
        assert_eq!(receipt.direct_recipients, 1, "the laptop acked");

        // …the fresh laptop — empty contact store — bootstraps alice's
        // record through its sibling (the after-direct healing seam)
        let arrived = direct_rx.recv().await.expect("the laptop's direct copy");
        laptop.after_direct(&arrived).await;
        assert!(
            !laptop.state.learned(&alice.public_key()).is_empty(),
            "the sibling should have answered the scoped auto-query"
        );

        // …alice drains: the signed recipients announce the laptop key; her
        // client auto-learns its record from the phone (the D3b mirror rule)
        script_drain(&a_net.dial.connect(&relay_a), deposited_envelopes(&ra_hi));
        let alice_got = alice.recv(&[mailbox_spec(&relay_a)]).await.expect("recv");
        assert!(
            alice_got.received[0].body.as_deref() == Ok(b"hi alice".as_slice()),
            "alice's copy came through her mailbox"
        );
        assert!(
            !alice.state.learned(&laptop.public_key()).is_empty(),
            "the phone should have served its recognized device's record"
        );

        // …alice accepts the offer — one explicit act, nothing auto-adopts
        alice
            .add_contact(&record_l, Some("mårten laptop".into()))
            .expect("alice promotes the laptop");

        // When: the phone goes OFFLINE — everything from here on must work
        // without it. Alice replies once; both devices' mailboxes take a
        // copy (the laptop declines her direct push: she is no contact of
        // the fresh device).
        a_net.dial.hold(&phone.public_key());
        let a_rp = a_net.dial.connect(&relay_p);
        a_rp.reply(deposited_frame());
        let a_rl = a_net.dial.connect(&relay_l);
        a_rl.reply(deposited_frame());
        let reply = alice.reply_contacts(conv).expect("alice reply contacts");
        let (replied, ()) = tokio::join!(
            alice.send_in(conv, &reply.contacts, b"hello both".to_vec(), vec![]),
            async {
                a_clock.wait_for_sleepers(1).await;
                a_clock.advance(Duration::from_millis(600));
            },
        );
        replied.expect("alice replies");

        // Then: BOTH of the person's devices receive the contact's reply
        script_drain(&l_net.dial.connect(&relay_l), deposited_envelopes(&a_rl));
        let laptop_got = laptop.recv(&[mailbox_spec(&relay_l)]).await.expect("recv");
        assert!(
            laptop_got
                .received
                .iter()
                .any(|received| received.body.as_deref() == Ok(b"hello both".as_slice()))
        );

        // When: the new device replies — empty contact store, routes
        // learned entirely through its sibling, the sibling still offline
        l_net.dial.hold(&phone.public_key());
        let l_rp = l_net.dial.connect(&relay_p);
        l_rp.reply(deposited_frame());
        let laptop_reply = laptop.reply_contacts(conv).expect("laptop reply contacts");
        let (replied, ()) = tokio::join!(
            laptop.send_in(
                conv,
                &laptop_reply.contacts,
                b"from the new device".to_vec(),
                vec![],
            ),
            async {
                l_clock.wait_for_sleepers(1).await;
                l_clock.advance(Duration::from_secs(3));
            },
        );

        // Then: the reply reached alice DIRECTLY (she promoted the laptop,
        // so her handler stores and acks it), and the sibling's mailbox got
        // its copy for when the phone returns
        assert_eq!(
            replied.expect("laptop replies").direct_recipients,
            1,
            "alice took the new device's reply peer-to-peer"
        );
        let alice_bodies: Vec<_> = alice
            .history(conv)
            .expect("alice history")
            .into_iter()
            .filter_map(|message| message.body.ok())
            .collect();
        assert!(alice_bodies.contains(&b"from the new device".to_vec()));
        let mut phone_mail = deposited_envelopes(&a_rp);
        phone_mail.extend(deposited_envelopes(&l_rp));
        script_drain(&p_net.dial.connect(&relay_p), phone_mail);
        let phone_got = phone.recv(&[mailbox_spec(&relay_p)]).await.expect("recv");
        let phone_bodies: Vec<_> = phone_got
            .received
            .iter()
            .filter_map(|received| received.body.as_deref().ok())
            .collect();
        assert!(phone_bodies.contains(&b"hello both".as_slice()));
        assert!(phone_bodies.contains(&b"from the new device".as_slice()));

        let _ = std::fs::remove_dir_all(temp_root("multidevice"));
    }

    #[tokio::test]
    async fn send__should_not_skip_a_relay_hosting_a_recipient_that_did_not_ack() {
        // Given: two recipients whose mailboxes live on the SAME relay — B
        // online, C not. The outbox ledger is per (message, relay) and one
        // deposit fans out to every recipient the relay hosts, so skipping on
        // "any recipient acked" would silently lose C's copy. This is the
        // regression test for that hazard.
        let wire = Loopback::new();
        let (a, a_net, a_clock) = loop_client("shared", "a", &wire);
        let (b, _b_net, _b_clock) = loop_client("shared", "b", &wire);
        let relay_a = DeviceKey::from_seed([138; 32]).public();
        b.add_contact(
            &routed_record(a.public_key(), &relay_a),
            Some("a".to_string()),
        )
        .expect("B adds A");
        let carol = DeviceKey::from_seed([42; 32]).public();
        // One shared mailbox for both — what a shared relay looks like. The
        // relay and carol are both silent: held dials, deadlines on the
        // TestClock (carol's cold probe, then the relay's full patience).
        let shared_relay = DeviceKey::from_seed([139; 32]).public();
        let shared = |key: PublicKey| routed_record(key, &shared_relay);
        a.add_contact(&shared(b.public_key()), Some("b".to_string()))
            .expect("A adds B");
        a.add_contact(&shared(carol), Some("c".to_string()))
            .expect("A adds C");
        a_net.dial.hold(&carol);
        a_net.dial.hold(&shared_relay);

        // When
        let recipients = [
            a.resolve_contact("b").expect("resolve b"),
            a.resolve_contact("c").expect("resolve c"),
        ];
        let (result, ()) =
            tokio::join!(a.send(&recipients, b"hello both".to_vec(), vec![]), async {
                a_clock.wait_for_sleepers(1).await;
                a_clock.advance(Duration::from_millis(600)); // carol's probe
                a_clock.wait_for_sleepers(1).await;
                a_clock.advance(ClientConfig::default().connect_timeout);
            },);

        // Then: B took it directly, but the shared relay is still owed —
        // C has no other way to ever see this message
        let receipt = result.expect("B's direct ack means the send landed somewhere");
        assert_eq!(receipt.direct_recipients, 1, "B acked");
        assert_eq!(receipt.skipped_relays, 0, "C's copy still needs the relay");
        assert_eq!(receipt.pending_relays, 1, "and it is owed, not forgotten");
        assert_eq!(a.state.outbox().len(), 1);

        let _ = std::fs::remove_dir_all(temp_root("shared"));
    }

    #[tokio::test]
    async fn deliver__should_decline_a_push_from_a_stranger() {
        // Given: A can reach B by key, but B has not added A — the D0c gate
        // applies to pushes exactly as to history reads, so an unknown
        // sender's first message goes through the mailbox (where the relay's
        // caps and the quarantine view are the policy), never straight to
        // our disk.
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("gate", "a", &wire);
        let (b, _b_net, _b_clock) = loop_client("gate", "b", &wire);
        let relay_b = DeviceKey::from_seed([135; 32]).public();
        a.add_contact(
            &routed_record(b.public_key(), &relay_b),
            Some("b".to_string()),
        )
        .expect("A adds B");
        let for_b = sealed_for(&a.device, b.public_key(), b"psst");

        // When
        let acked = a.deliver_direct(&for_b, &[b.public_key()]).await;

        // Then: declined, and nothing stored
        assert!(acked.is_empty(), "a stranger's push is declined");
        assert!(b.state.find_envelope(for_b.id()).is_none());

        let _ = std::fs::remove_dir_all(temp_root("gate"));
    }

    #[tokio::test]
    async fn deliver__should_decline_an_envelope_not_addressed_to_us() {
        // Given: A and B are contacts, so the connection-level gate is open.
        // Being *allowed to push* must still not mean being allowed to write
        // arbitrary history into our store — not even our own relay can do
        // that (it indexes deposits per recipient key).
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client("addressed", "a", &wire);
        let (b, _b_net, _b_clock) = loop_client("addressed", "b", &wire);
        let relay_a = DeviceKey::from_seed([136; 32]).public();
        let relay_b = DeviceKey::from_seed([137; 32]).public();
        a.add_contact(
            &routed_record(b.public_key(), &relay_b),
            Some("b".to_string()),
        )
        .expect("A adds B");
        b.add_contact(
            &routed_record(a.public_key(), &relay_a),
            Some("a".to_string()),
        )
        .expect("B adds A");
        let elsewhere = DeviceKey::from_seed([43; 32]).public();
        let for_carol = sealed_for(&a.device, elsewhere, b"not for you");

        // When
        let acked = a.deliver_direct(&for_carol, &[b.public_key()]).await;

        // Then: declined and unstored…
        assert!(acked.is_empty(), "not addressed to B");
        assert!(b.state.find_envelope(for_carol.id()).is_none());

        // …while a push addressed to B, over that same open connection, is
        // accepted: the per-request check is independent of the gate.
        let for_b = sealed_for(&a.device, b.public_key(), b"psst");
        let acked = a.deliver_direct(&for_b, &[b.public_key()]).await;
        assert_eq!(acked.len(), 1, "a contact's push for B is stored");
        assert!(b.state.find_envelope(for_b.id()).is_some());

        let _ = std::fs::remove_dir_all(temp_root("addressed"));
    }

    #[tokio::test]
    async fn send_to_self__should_append_recognized_devices_and_not_fork() {
        // Given: a phone that recognized its laptop; alice as a contact.
        // Relays are unreachable — sends queue, and the stored state is
        // what carries the assertions.
        let key = temp_key("sendself", "phone");
        keystore::create(&key).expect("key");
        let phone = Client::open_with(
            &key,
            ClientConfig {
                connect_timeout: Duration::from_millis(300),
                ..Default::default()
            },
        )
        .await
        .expect("open");
        let laptop = DeviceKey::from_seed([50; 32]).public();
        let alice = DeviceKey::from_seed([51; 32]).public();
        phone
            .recognize_device(&ContactRecord::new(
                vec![laptop],
                vec![],
                mailbox_only("ll@203.0.113.5:5"),
            ))
            .expect("recognize");
        let to_alice = || {
            vec![Contact {
                keys: vec![alice],
                relays: vec!["aa@203.0.113.1:1".to_string()],
            }]
        };

        // When: two sends by the same user-addressed set
        for text in [b"one".as_slice(), b"two".as_slice()] {
            let result = phone.send(&to_alice(), text.to_vec(), vec![]).await;
            assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        }

        // Then: exactly ONE conversation — the device-extended lookup
        // keeps post-pairing sends threading instead of forking
        let conversations = phone.state.conversations();
        assert_eq!(conversations.len(), 1, "post-pairing send-by-name forked");
        let envelopes = phone
            .state
            .load_envelopes(conversations[0])
            .expect("envelopes");
        assert_eq!(envelopes.len(), 2);
        // …and every sealed core lists the laptop as an honest member,
        // while the sending device itself stays unlisted (self-wrap)
        for envelope in &envelopes {
            assert!(envelope.core.recipients.contains(&laptop));
            assert!(envelope.core.recipients.contains(&alice));
            assert!(!envelope.core.recipients.contains(&phone.public_key()));
        }
        // …and the laptop's relay is owed the deposits like any recipient
        assert!(
            phone
                .state
                .outbox()
                .iter()
                .any(|entry| entry.relay == "ll@203.0.113.5:5"),
            "no outbox entry for the device's relay"
        );

        let _ = std::fs::remove_dir_all(temp_root("sendself"));
    }
}
