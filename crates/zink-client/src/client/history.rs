//! The conversation read-side: membership as a lens on the DAG heads,
//! conversation listings with the contributing-contact verdict, rendered
//! history with confirmations and membership deltas, the inbox triage —
//! and fetching the blobs a stored or received message references.

use std::collections::{BTreeMap, BTreeSet};

use zink_protocol::{BlobHash, BlobRef, MessageEnvelope, MessageId, OpenError, PublicKey};

use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;
use crate::state::ClientState;
use crate::{blobs, hex};

use super::{Client, Received};

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
pub(super) fn participants_of(envelope: &MessageEnvelope) -> impl Iterator<Item = PublicKey> + '_ {
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

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
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
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::adapters::system_clock::SystemClock;
    use crate::client::test_kit::{
        deposited_envelopes, deposited_frame, loop_client, mailbox_only, mailbox_spec, message,
        routed_record, script_drain, signed_record, summary, temp_key, temp_root,
    };
    use crate::client::{ClientConfig, Contact};
    use crate::keystore;
    use crate::ports::clock::TestClock;
    use crate::ports::transport::{Loopback, TestTransport};
    use zink_protocol::{ContactRecord, DeviceKey, RelayEntry};

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
}
