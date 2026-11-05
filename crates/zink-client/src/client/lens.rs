//! Own-device lens sync (project 7 S6, lens-sync.md): the op-frame
//! carrier, the lens channel, emission from local acts, and the adoption
//! replay. Ops are client policy riding sealed bodies — the protocol
//! carries opaque bytes and relays see ciphertext, as always.

use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use zink_protocol::{ContactRecord, MessageEnvelope, MessageId, PublicKey};

use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::{Draw, Mint};
use crate::ports::store::Store;
use crate::ports::transport::Transport;

use super::{Client, PersonId};

/// The frame magic (lens-sync.md §2): a body beginning with these bytes
/// is an op frame. Parsing is never trust — effects are gated by the
/// envelope's verified author; rendering hides frames from any author.
const OP_MAGIC: &[u8; 4] = b"zop\0";

/// The versioned frame around every op. New op kinds are appended to
/// [`LensOp`] without a bump (the appended-variant norm); a frame this
/// build can't decode is "an op we don't speak" — ignored, never an
/// error (hostile-input rule + forward compatibility in one).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub(super) struct OpFrame {
    pub version: u16,
    pub op: LensOp,
}

impl OpFrame {
    pub(super) const CURRENT: u16 = 1;
}

/// The op vocabulary (lens-sync.md §2/§4) — client policy, never
/// zink-protocol's. Keys, never person ids: "label the cluster
/// containing K" resolves by overlap on every device, so devices that
/// cluster differently each apply it correctly in their own world.
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub(super) enum LensOp {
    /// The channel genesis marker (§3).
    Hello,
    /// Label the person whose cluster overlaps these keys.
    LabelPerson {
        members: Vec<[u8; 32]>,
        label: String,
    },
    /// "This device added a contact" — an offer, never a write (§6).
    OfferContact { record: Vec<u8>, petname: String },
}

/// Encode an op as a sealed-body payload.
fn op_body(op: LensOp) -> Vec<u8> {
    let mut body = OP_MAGIC.to_vec();
    borsh::to_writer(
        &mut body,
        &OpFrame {
            version: OpFrame::CURRENT,
            op,
        },
    )
    .expect("borsh into a Vec cannot fail");
    body
}

/// Whether a body is an op frame — the rendering rule (§2): frames render
/// as nothing in chat surfaces, whatever the author, decodable or not.
pub fn is_op_frame(body: &[u8]) -> bool {
    body.starts_with(OP_MAGIC)
}

/// Decode a frame this build speaks; anything else — unknown future
/// version, unknown appended variant, truncation, hostile bytes — is
/// `None`, an honest no-op.
fn parse_op(body: &[u8]) -> Option<LensOp> {
    let payload = body.strip_prefix(OP_MAGIC)?;
    let frame = OpFrame::try_from_slice(payload).ok()?;
    (frame.version <= OpFrame::CURRENT).then_some(frame.op)
}

/// A pending contact-add offer from a sibling device (lens-sync.md §6).
pub struct LensOffer {
    pub subject: PublicKey,
    pub petname: String,
    pub record: ContactRecord,
    /// The sibling that added them — the provenance line.
    pub author: PublicKey,
}

/// A surfaced label conflict (lens-sync.md §5) — "your phone calls them
/// X"; nothing arbitrates.
pub struct LensConflict {
    pub person: PersonId,
    pub theirs: String,
    pub author: PublicKey,
}

/// One of my `LabelPerson` ops, precomputed for the concurrency check:
/// which keys it spoke about, its message id, and its channel.
struct MyLabelOp {
    members: BTreeSet<[u8; 32]>,
    id: MessageId,
    channel: MessageId,
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw + Mint, S: Store> Client<C, W, N, R, S> {
    /// The channel this device emits into, created on first use (§3): a
    /// `Hello` genesis with no human recipients — send-to-self appends
    /// the siblings, and single-device it's a self-wrapped note that
    /// backfill + re-wrap hand to future siblings.
    async fn lens_emission_channel(&self) -> Result<MessageId, Error> {
        if let Some(id) = self.state.lens_conversation().await {
            return Ok(id);
        }
        let staged = self
            .stage(
                self.genesis_draft(vec![]),
                op_body(LensOp::Hello),
                vec![],
                &[],
            )
            .await?;
        self.state
            .set_lens_conversation(staged.conversation)
            .await?;
        self.state.add_lens_channel(staged.conversation).await?;
        self.state.add_lens_applied(staged.id).await?;
        Ok(staged.conversation)
    }

    /// Emit an op — stage-only (§3): sealed, stored, outboxed; delivery
    /// rides the normal flush. Best-effort by design: emission failure
    /// never fails the local act it mirrors — the devices diverge until
    /// the next act, and the warn says so.
    pub(super) async fn emit_lens_op(&self, op: LensOp) {
        let staged = async {
            let channel = self.lens_emission_channel().await?;
            let draft = self.threaded_draft(channel, vec![]).await?;
            self.stage(draft, op_body(op), vec![], &[]).await
        }
        .await;
        match staged {
            Ok(staged) => {
                let _ = self.state.add_lens_applied(staged.id).await;
            }
            Err(error) => {
                tracing::warn!(%error, "lens op emission failed; devices diverge until the next act")
            }
        }
    }

    /// The adoption replay (lens-sync.md §5): store-driven and
    /// idempotent — run after drains and after re-wraps land. Errors are
    /// swallowed with a debug line; the next drain retries.
    pub async fn adopt_lens_ops(&self) {
        if let Err(error) = self.replay_lens().await {
            tracing::debug!(%error, "lens replay failed; retried at the next drain");
        }
    }

    async fn replay_lens(&self) -> Result<(), Error> {
        let own = self.own_keys().await;
        let me = self.device.public();
        // Classify new channels (§3): genesis authored by an own key,
        // body opening to a `Hello` frame.
        let mut channels = self.state.lens_channels().await;
        for id in self.state.conversations().await {
            if channels.contains(&id) {
                continue;
            }
            let Some(genesis) = self.state.find_envelope(id).await else {
                continue;
            };
            if !own.contains(&genesis.core.sender) {
                continue;
            }
            let Ok(body) = genesis.open(&self.device) else {
                continue;
            };
            if matches!(parse_op(&body), Some(LensOp::Hello)) {
                self.state.add_lens_channel(id).await?;
                channels.insert(id);
            }
        }
        // Emission converges on the smallest classified channel (§3) — a
        // deterministic tiebreak every sibling computes alike.
        if let Some(&smallest) = channels.iter().next()
            && self.state.lens_conversation().await != Some(smallest)
        {
            self.state.set_lens_conversation(smallest).await?;
        }
        // My label ops across every channel, precomputed: an op of mine
        // in a *different* channel can never be a sibling op's ancestor —
        // which is exactly the concurrency that split channels represent.
        let mut mine: Vec<MyLabelOp> = Vec::new();
        for &channel in &channels {
            for envelope in self.state.load_envelopes(channel).await? {
                if envelope.core.sender != me {
                    continue;
                }
                let Ok(body) = envelope.open(&self.device) else {
                    continue;
                };
                if let Some(LensOp::LabelPerson { members, .. }) = parse_op(&body) {
                    mine.push(MyLabelOp {
                        members: members.into_iter().collect(),
                        id: envelope.id(),
                        channel,
                    });
                }
            }
        }
        let applied = self.state.lens_applied().await;
        for &channel in &channels {
            let envelopes = self.state.load_envelopes(channel).await?;
            let by_id: BTreeMap<MessageId, &MessageEnvelope> = envelopes
                .iter()
                .map(|envelope| (envelope.id(), envelope))
                .collect();
            let dag = crate::state::dag_of(&envelopes, channel)?;
            for id in dag.linearize() {
                if applied.contains(&id) {
                    continue;
                }
                let Some(envelope) = by_id.get(&id) else {
                    continue;
                };
                let author = envelope.core.sender;
                if !own.contains(&author) {
                    // Never an effect from a non-sibling (§2); ledgered
                    // so a hostile frame isn't re-inspected every drain.
                    self.state.add_lens_applied(id).await?;
                    continue;
                }
                // An unopenable body is *not* ledgered: the new-device
                // bootstrap opens it once the re-wrap lands (§5).
                let Ok(body) = envelope.open(&self.device) else {
                    continue;
                };
                let Some(op) = parse_op(&body) else {
                    self.state.add_lens_applied(id).await?;
                    continue;
                };
                if author != me {
                    match op {
                        LensOp::Hello => {}
                        LensOp::LabelPerson { members, label } => {
                            self.adopt_label(channel, &by_id, id, author, &members, &label, &mine)
                                .await?;
                        }
                        LensOp::OfferContact { record, petname } => {
                            self.adopt_offer(author, &record, &petname).await?;
                        }
                    }
                }
                self.state.add_lens_applied(id).await?;
            }
        }
        Ok(())
    }

    /// Auto-adopt or surface (§5): the sibling's rename applies iff every
    /// label op this device authored about the same person is an ancestor
    /// of theirs — the sibling demonstrably saw my state, so its edit is
    /// later, not competing. Concurrent edits keep mine and surface
    /// theirs with provenance; so does an adopted label that would
    /// collide with an existing one — never forced. Adoption does not
    /// re-emit: the original op reaches every sibling by itself.
    #[allow(clippy::too_many_arguments)]
    async fn adopt_label(
        &self,
        channel: MessageId,
        by_id: &BTreeMap<MessageId, &MessageEnvelope>,
        op_id: MessageId,
        author: PublicKey,
        members: &[[u8; 32]],
        label: &str,
        mine: &[MyLabelOp],
    ) -> Result<(), Error> {
        let Some(person) = self
            .persons()
            .await?
            .into_iter()
            .find(|person| person.keys().iter().any(|key| members.contains(&key.0)))
        else {
            return Ok(()); // someone this device doesn't hold (§5)
        };
        if person.label == label {
            return Ok(()); // converged
        }
        let person_keys: BTreeSet<[u8; 32]> = person.keys().into_iter().map(|key| key.0).collect();
        let mut ancestors = BTreeSet::new();
        let mut stack = vec![op_id];
        while let Some(current) = stack.pop() {
            let Some(envelope) = by_id.get(&current) else {
                continue;
            };
            for &parent in &envelope.core.parents {
                if ancestors.insert(parent) {
                    stack.push(parent);
                }
            }
        }
        let saw_all_mine = mine
            .iter()
            .filter(|op| !op.members.is_disjoint(&person_keys))
            .all(|op| op.channel == channel && ancestors.contains(&op.id));
        if saw_all_mine && self.set_person_label(&person, label).await.is_ok() {
            self.state.remove_conflict(person.id.to_storage()).await;
        } else {
            self.state
                .save_conflict(person.id.to_storage(), label, &author)
                .await?;
        }
        Ok(())
    }

    /// Store the offer (§6): latest per subject; a record overlapping an
    /// existing contact is dropped — already held, which is also what
    /// breaks the accept→re-offer loop between siblings.
    async fn adopt_offer(
        &self,
        author: PublicKey,
        record: &[u8],
        petname: &str,
    ) -> Result<(), Error> {
        let Ok(record) = ContactRecord::try_from_bytes(record) else {
            return Ok(()); // undecodable — hostile or from a newer build
        };
        let Some(&subject) = record.keys.first() else {
            return Ok(());
        };
        let contacts = self.state.contacts().await?;
        let held = contacts
            .iter()
            .any(|(_, stored)| stored.keys.iter().any(|key| record.keys.contains(key)));
        if held {
            return Ok(());
        }
        self.state
            .save_offer(&subject, &author, petname, &record)
            .await
    }

    /// Pending offers, read-time filtered (§6): the author must still be
    /// a recognized device — a repudiated or un-recognized sibling's
    /// offers void — and subjects meanwhile held drop away.
    pub async fn lens_offers(&self) -> Result<Vec<LensOffer>, Error> {
        let own = self.own_keys().await;
        let contacts = self.state.contacts().await?;
        Ok(self
            .state
            .offers()
            .await
            .into_iter()
            .filter_map(|(subject, author, petname, record)| {
                if !own.contains(&author) {
                    return None;
                }
                let held = contacts
                    .iter()
                    .any(|(_, stored)| stored.keys.iter().any(|key| record.keys.contains(key)));
                if held {
                    return None;
                }
                Some(LensOffer {
                    subject,
                    petname,
                    record,
                    author,
                })
            })
            .collect())
    }

    /// The explicit accept (§6) — the ordinary `add_contact`, the only
    /// write the contact store ever takes from this path. Returns the
    /// stored petname.
    pub async fn accept_offer(&self, subject: &PublicKey) -> Result<String, Error> {
        let offer = self
            .lens_offers()
            .await?
            .into_iter()
            .find(|offer| offer.subject == *subject)
            .ok_or_else(|| Error::InvalidInput("no pending offer for that key".into()))?;
        let petname = self.add_contact(&offer.record, Some(offer.petname)).await?;
        self.state.remove_offer(subject).await;
        Ok(petname)
    }

    /// Decline an offer — dropped, never a stance (declining to add is
    /// not a disavowal).
    pub async fn decline_offer(&self, subject: &PublicKey) {
        self.state.remove_offer(subject).await;
    }

    /// Surfaced label conflicts (§5), with provenance for the edge.
    pub async fn lens_conflicts(&self) -> Vec<LensConflict> {
        self.state
            .conflicts()
            .await
            .into_iter()
            .map(|(person, theirs, author)| LensConflict {
                person: PersonId::from_storage(person),
                theirs,
                author,
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::client::test_kit::{crash_states, loop_client, mailbox_only, signed_record};

    use crate::ports::transport::Loopback;
    use zink_protocol::DeviceKey;

    /// Two live clients recognizing each other as own devices — the
    /// paired-phone-and-laptop setup every S6 scenario starts from.
    async fn paired() -> (
        crate::client::test_kit::LoopClient,
        crate::client::test_kit::LoopClient,
    ) {
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client(&wire).await;
        let (b, _b_net, _b_clock) = loop_client(&wire).await;
        a.recognize_device(&signed_record(
            &b.device,
            "laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        ))
        .await
        .expect("phone recognizes laptop");
        b.recognize_device(&signed_record(
            &a.device,
            "phone",
            0,
            mailbox_only("pp@203.0.113.6:6"),
        ))
        .await
        .expect("laptop recognizes phone");
        (a, b)
    }

    /// Hand one client's lens channel to another, envelope by envelope —
    /// exactly what a drain (or backfill + re-wrap) leaves behind.
    async fn seed_channel(
        from: &crate::client::test_kit::LoopClient,
        to: &crate::client::test_kit::LoopClient,
    ) {
        let channel = from
            .state
            .lens_conversation()
            .await
            .expect("channel exists");
        for envelope in from.state.load_envelopes(channel).await.expect("load") {
            to.state
                .store_envelope(channel, &envelope, 0)
                .await
                .expect("seed envelope");
        }
    }

    fn carol_record() -> ContactRecord {
        let carol = DeviceKey::from_seed([70; 32]);
        signed_record(&carol, "carol", 0, mailbox_only("cc@203.0.113.9:9"))
    }

    #[tokio::test]
    async fn adopt__should_converge_a_rename_made_while_apart() {
        // Given: both devices hold carol; the phone renames her while the
        // laptop is offline
        let (a, b) = paired().await;
        a.add_contact(&carol_record(), None)
            .await
            .expect("phone adds");
        b.add_contact(&carol_record(), None)
            .await
            .expect("laptop adds");
        let person = a.person_by_label("carol").await.expect("person");
        a.rename_person(person.id, "carrie").await.expect("rename");

        // When: the laptop later holds the phone's channel (what a drain
        // leaves behind) and replays
        seed_channel(&a, &b).await;
        b.adopt_lens_ops().await;

        // Then: the laptop converged — no conflict, and the channel never
        // reaches its inbox
        let adopted = b.person_by_label("carrie").await.expect("adopted");
        assert_eq!(adopted.label, "carrie");
        assert!(b.lens_conflicts().await.is_empty());
        let channel = a.state.lens_conversation().await.expect("channel");
        assert!(
            b.conversations()
                .await
                .expect("inbox")
                .iter()
                .all(|summary| summary.id != channel),
            "the lens channel stays out of the inbox"
        );
    }

    #[tokio::test]
    async fn adopt__should_keep_mine_and_surface_theirs_on_concurrent_renames() {
        // Given: both devices hold carol and rename her while apart —
        // neither op saw the other
        let (a, b) = paired().await;
        a.add_contact(&carol_record(), None)
            .await
            .expect("phone adds");
        b.add_contact(&carol_record(), None)
            .await
            .expect("laptop adds");
        let on_a = a.person_by_label("carol").await.expect("person on phone");
        let on_b = b.person_by_label("carol").await.expect("person on laptop");
        a.rename_person(on_a.id, "carrie")
            .await
            .expect("phone rename");
        b.rename_person(on_b.id, "caz")
            .await
            .expect("laptop rename");

        // When
        seed_channel(&a, &b).await;
        b.adopt_lens_ops().await;

        // Then: manual edits win — the laptop keeps its label and
        // surfaces the phone's with provenance; nothing arbitrates
        assert!(b.person_by_label("caz").await.is_ok());
        let conflicts = b.lens_conflicts().await;
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].theirs, "carrie");
        assert_eq!(conflicts[0].author, a.public_key());

        // And: my next rename supersedes and clears the surface
        b.rename_person(on_b.id, "carrie")
            .await
            .expect("take theirs");
        assert!(b.lens_conflicts().await.is_empty());
    }

    #[tokio::test]
    async fn adopt__should_offer_a_sibling_contact_add_and_accept_explicitly() {
        // Given: the phone adds carol; the laptop has never met her
        let (a, b) = paired().await;
        a.add_contact(&carol_record(), None)
            .await
            .expect("phone adds");
        let carol_key = carol_record().keys[0];

        // When: the laptop replays the phone's channel
        seed_channel(&a, &b).await;
        b.adopt_lens_ops().await;

        // Then: an offer with provenance — and no contact write
        let offers = b.lens_offers().await.expect("offers");
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].petname, "carol");
        assert_eq!(offers[0].author, a.public_key());
        assert!(
            b.person_by_label("carol").await.is_err(),
            "nothing wrote itself"
        );

        // When: the explicit accept — the only write
        let petname = b.accept_offer(&carol_key).await.expect("accept");

        // Then: stored under the offered petname; the offer is gone; and
        // the accept's own onward offer is a no-op back on the phone
        assert_eq!(petname, "carol");
        assert!(b.person_by_label("carol").await.is_ok());
        assert!(b.lens_offers().await.expect("offers").is_empty());
        seed_channel(&b, &a).await;
        a.adopt_lens_ops().await;
        assert!(
            a.lens_offers().await.expect("offers").is_empty(),
            "already held — the re-offer loop breaks"
        );
    }

    #[tokio::test]
    async fn repudiate__should_void_the_siblings_pending_offers() {
        // Given: a pending offer from the phone
        let (a, b) = paired().await;
        a.add_contact(&carol_record(), None)
            .await
            .expect("phone adds");
        seed_channel(&a, &b).await;
        b.adopt_lens_ops().await;
        assert_eq!(b.lens_offers().await.expect("offers").len(), 1);

        // When: the laptop repudiates the phone
        b.repudiate(a.public_key()).await.expect("repudiate");

        // Then: its offers are voided — the offer gates the write surface
        assert!(b.lens_offers().await.expect("offers").is_empty());
        assert!(
            b.state.offers().await.is_empty(),
            "removed, not merely filtered"
        );
    }

    #[tokio::test]
    async fn adopt__should_ignore_ops_from_non_siblings() {
        // Given: a stranger's conversation whose genesis is a Hello frame
        // and whose follow-up renames b's carol — hostile mimicry
        let (_, b) = paired().await;
        b.add_contact(&carol_record(), None)
            .await
            .expect("laptop adds");
        // The bodies never even open: author gating rejects first, which
        // is the property under test.
        let stranger = DeviceKey::from_seed([66; 32]);
        let hello =
            crate::client::test_kit::message(&stranger, vec![b.public_key()], None, vec![], 0, 0);
        let genesis_id = hello.id();
        let rename = crate::client::test_kit::message(
            &stranger,
            vec![b.public_key()],
            Some(genesis_id),
            vec![genesis_id],
            1,
            1,
        );
        b.state
            .store_envelope(genesis_id, &hello, 0)
            .await
            .expect("seed");
        b.state
            .store_envelope(genesis_id, &rename, 0)
            .await
            .expect("seed");

        // When
        b.adopt_lens_ops().await;

        // Then: no effect, no conflict — and the stranger's conversation
        // is NOT classified as a lens channel (it stays an inbox request)
        assert!(b.person_by_label("carol").await.is_ok());
        assert!(b.lens_conflicts().await.is_empty());
        assert!(!b.state.lens_channels().await.contains(&genesis_id));
    }

    #[tokio::test]
    async fn adopt__should_converge_after_a_crash_at_any_point_of_a_replay() {
        // Given: the laptop holds the phone's rename op, unreplayed
        let (a, b) = paired().await;
        a.add_contact(&carol_record(), None)
            .await
            .expect("phone adds");
        b.add_contact(&carol_record(), None)
            .await
            .expect("laptop adds");
        let person = a.person_by_label("carol").await.expect("person");
        a.rename_person(person.id, "carrie").await.expect("rename");
        seed_channel(&a, &b).await;
        let since = b.state.store().writes();

        // When: the replay runs — and is cut short at every possible write
        b.adopt_lens_ops().await;

        // Then: the next replay (the next drain) finishes the job — an op
        // is ledgered only after its effect, so none is skipped half-done
        for (landed, crashed) in crash_states(&b, since).await {
            crashed.adopt_lens_ops().await;
            assert!(
                crashed.person_by_label("carrie").await.is_ok(),
                "after {landed} write(s): the rename was lost"
            );
        }
    }
}
