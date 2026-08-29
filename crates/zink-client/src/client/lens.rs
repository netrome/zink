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
use crate::ports::transport::Transport;
use crate::state::ClientState;

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

impl<C: Clock, W: WallClock, N: Transport, R: Draw + Mint> Client<C, W, N, R> {
    /// The channel this device emits into, created on first use (§3): a
    /// `Hello` genesis with no human recipients — send-to-self appends
    /// the siblings, and single-device it's a self-wrapped note that
    /// backfill + re-wrap hand to future siblings.
    fn lens_emission_channel(&self) -> Result<MessageId, Error> {
        if let Some(id) = self.state.lens_conversation() {
            return Ok(id);
        }
        let staged = self.stage(
            self.genesis_draft(vec![]),
            op_body(LensOp::Hello),
            vec![],
            &[],
        )?;
        self.state.set_lens_conversation(staged.conversation)?;
        self.state.add_lens_channel(staged.conversation)?;
        self.state.add_lens_applied(staged.id)?;
        Ok(staged.conversation)
    }

    /// Emit an op — stage-only (§3): sealed, stored, outboxed; delivery
    /// rides the normal flush. Best-effort by design: emission failure
    /// never fails the local act it mirrors — the devices diverge until
    /// the next act, and the warn says so.
    pub(super) fn emit_lens_op(&self, op: LensOp) {
        let staged = self.lens_emission_channel().and_then(|channel| {
            let draft = self.threaded_draft(channel, vec![])?;
            self.stage(draft, op_body(op), vec![], &[])
        });
        match staged {
            Ok(staged) => {
                let _ = self.state.add_lens_applied(staged.id);
            }
            Err(error) => {
                tracing::warn!(%error, "lens op emission failed; devices diverge until the next act")
            }
        }
    }

    /// The adoption replay (lens-sync.md §5): store-driven and
    /// idempotent — run after drains and after re-wraps land. Errors are
    /// swallowed with a debug line; the next drain retries.
    pub fn adopt_lens_ops(&self) {
        if let Err(error) = self.replay_lens() {
            tracing::debug!(%error, "lens replay failed; retried at the next drain");
        }
    }

    fn replay_lens(&self) -> Result<(), Error> {
        let own = self.own_keys();
        let me = self.device.public();
        // Classify new channels (§3): genesis authored by an own key,
        // body opening to a `Hello` frame.
        let mut channels = self.state.lens_channels();
        for id in self.state.conversations() {
            if channels.contains(&id) {
                continue;
            }
            let Some(genesis) = self.state.find_envelope(id) else {
                continue;
            };
            if !own.contains(&genesis.core.sender) {
                continue;
            }
            let Ok(body) = genesis.open(&self.device) else {
                continue;
            };
            if matches!(parse_op(&body), Some(LensOp::Hello)) {
                self.state.add_lens_channel(id)?;
                channels.insert(id);
            }
        }
        // Emission converges on the smallest classified channel (§3) — a
        // deterministic tiebreak every sibling computes alike.
        if let Some(&smallest) = channels.iter().next()
            && self.state.lens_conversation() != Some(smallest)
        {
            self.state.set_lens_conversation(smallest)?;
        }
        // My label ops across every channel, precomputed: an op of mine
        // in a *different* channel can never be a sibling op's ancestor —
        // which is exactly the concurrency that split channels represent.
        let mut mine: Vec<MyLabelOp> = Vec::new();
        for &channel in &channels {
            for envelope in self.state.load_envelopes(channel)? {
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
        let applied = self.state.lens_applied();
        for &channel in &channels {
            let envelopes = self.state.load_envelopes(channel)?;
            let by_id: BTreeMap<MessageId, &MessageEnvelope> = envelopes
                .iter()
                .map(|envelope| (envelope.id(), envelope))
                .collect();
            let dag = ClientState::dag_of(&envelopes, channel)?;
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
                    self.state.add_lens_applied(id)?;
                    continue;
                }
                // An unopenable body is *not* ledgered: the new-device
                // bootstrap opens it once the re-wrap lands (§5).
                let Ok(body) = envelope.open(&self.device) else {
                    continue;
                };
                let Some(op) = parse_op(&body) else {
                    self.state.add_lens_applied(id)?;
                    continue;
                };
                if author != me {
                    match op {
                        LensOp::Hello => {}
                        LensOp::LabelPerson { members, label } => {
                            self.adopt_label(channel, &by_id, id, author, &members, &label, &mine)?;
                        }
                        LensOp::OfferContact { record, petname } => {
                            self.adopt_offer(author, &record, &petname)?;
                        }
                    }
                }
                self.state.add_lens_applied(id)?;
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
    fn adopt_label(
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
            .persons()?
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
        if saw_all_mine && self.set_person_label(&person, label).is_ok() {
            self.state.remove_conflict(person.id.to_storage());
        } else {
            self.state
                .save_conflict(person.id.to_storage(), label, &author)?;
        }
        Ok(())
    }

    /// Store the offer (§6): latest per subject; a record overlapping an
    /// existing contact is dropped — already held, which is also what
    /// breaks the accept→re-offer loop between siblings.
    fn adopt_offer(&self, author: PublicKey, record: &[u8], petname: &str) -> Result<(), Error> {
        let Ok(record) = ContactRecord::try_from_bytes(record) else {
            return Ok(()); // undecodable — hostile or from a newer build
        };
        let Some(&subject) = record.keys.first() else {
            return Ok(());
        };
        let contacts = self.state.contacts()?;
        let held = contacts
            .iter()
            .any(|(_, stored)| stored.keys.iter().any(|key| record.keys.contains(key)));
        if held {
            return Ok(());
        }
        self.state.save_offer(&subject, &author, petname, &record)
    }

    /// Pending offers, read-time filtered (§6): the author must still be
    /// a recognized device — a repudiated or un-recognized sibling's
    /// offers void — and subjects meanwhile held drop away.
    pub fn lens_offers(&self) -> Result<Vec<LensOffer>, Error> {
        let own = self.own_keys();
        let contacts = self.state.contacts()?;
        Ok(self
            .state
            .offers()
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
    pub fn accept_offer(&self, subject: &PublicKey) -> Result<String, Error> {
        let offer = self
            .lens_offers()?
            .into_iter()
            .find(|offer| offer.subject == *subject)
            .ok_or_else(|| Error::InvalidInput("no pending offer for that key".into()))?;
        let petname = self.add_contact(&offer.record, Some(offer.petname))?;
        self.state.remove_offer(subject);
        Ok(petname)
    }

    /// Decline an offer — dropped, never a stance (declining to add is
    /// not a disavowal).
    pub fn decline_offer(&self, subject: &PublicKey) {
        self.state.remove_offer(subject);
    }

    /// Surfaced label conflicts (§5), with provenance for the edge.
    pub fn lens_conflicts(&self) -> Vec<LensConflict> {
        self.state
            .conflicts()
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
    use crate::client::test_kit::{loop_client, mailbox_only, signed_record, temp_key, temp_root};
    use crate::keystore;
    use crate::ports::transport::Loopback;
    use zink_protocol::DeviceKey;

    /// Two live clients recognizing each other as own devices — the
    /// paired-phone-and-laptop setup every S6 scenario starts from.
    fn paired(
        test: &str,
    ) -> (
        crate::client::test_kit::LoopClient,
        crate::client::test_kit::LoopClient,
    ) {
        let wire = Loopback::new();
        let (a, _a_net, _a_clock) = loop_client(test, "phone", &wire);
        let (b, _b_net, _b_clock) = loop_client(test, "laptop", &wire);
        let a_device = keystore::load(&temp_key(test, "phone")).expect("phone key");
        let b_device = keystore::load(&temp_key(test, "laptop")).expect("laptop key");
        a.recognize_device(&signed_record(
            &b_device,
            "laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        ))
        .expect("phone recognizes laptop");
        b.recognize_device(&signed_record(
            &a_device,
            "phone",
            0,
            mailbox_only("pp@203.0.113.6:6"),
        ))
        .expect("laptop recognizes phone");
        (a, b)
    }

    /// Hand one client's lens channel to another, envelope by envelope —
    /// exactly what a drain (or backfill + re-wrap) leaves behind.
    fn seed_channel(
        from: &crate::client::test_kit::LoopClient,
        to: &crate::client::test_kit::LoopClient,
    ) {
        let channel = from.state.lens_conversation().expect("channel exists");
        for envelope in from.state.load_envelopes(channel).expect("load") {
            to.state
                .store_envelope(channel, &envelope)
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
        let (a, b) = paired("lens-rename");
        a.add_contact(&carol_record(), None).expect("phone adds");
        b.add_contact(&carol_record(), None).expect("laptop adds");
        let person = a.person_by_label("carol").expect("person");
        a.rename_person(person.id, "carrie").expect("rename");

        // When: the laptop later holds the phone's channel (what a drain
        // leaves behind) and replays
        seed_channel(&a, &b);
        b.adopt_lens_ops();

        // Then: the laptop converged — no conflict, and the channel never
        // reaches its inbox
        let adopted = b.person_by_label("carrie").expect("adopted");
        assert_eq!(adopted.label, "carrie");
        assert!(b.lens_conflicts().is_empty());
        let channel = a.state.lens_conversation().expect("channel");
        assert!(
            b.conversations()
                .expect("inbox")
                .iter()
                .all(|summary| summary.id != channel),
            "the lens channel stays out of the inbox"
        );

        let _ = std::fs::remove_dir_all(temp_root("lens-rename"));
    }

    #[tokio::test]
    async fn adopt__should_keep_mine_and_surface_theirs_on_concurrent_renames() {
        // Given: both devices hold carol and rename her while apart —
        // neither op saw the other
        let (a, b) = paired("lens-conflict");
        a.add_contact(&carol_record(), None).expect("phone adds");
        b.add_contact(&carol_record(), None).expect("laptop adds");
        let on_a = a.person_by_label("carol").expect("person on phone");
        let on_b = b.person_by_label("carol").expect("person on laptop");
        a.rename_person(on_a.id, "carrie").expect("phone rename");
        b.rename_person(on_b.id, "caz").expect("laptop rename");

        // When
        seed_channel(&a, &b);
        b.adopt_lens_ops();

        // Then: manual edits win — the laptop keeps its label and
        // surfaces the phone's with provenance; nothing arbitrates
        assert!(b.person_by_label("caz").is_ok());
        let conflicts = b.lens_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].theirs, "carrie");
        assert_eq!(conflicts[0].author, a.public_key());

        // And: my next rename supersedes and clears the surface
        b.rename_person(on_b.id, "carrie").expect("take theirs");
        assert!(b.lens_conflicts().is_empty());

        let _ = std::fs::remove_dir_all(temp_root("lens-conflict"));
    }

    #[tokio::test]
    async fn adopt__should_offer_a_sibling_contact_add_and_accept_explicitly() {
        // Given: the phone adds carol; the laptop has never met her
        let (a, b) = paired("lens-offer");
        a.add_contact(&carol_record(), None).expect("phone adds");
        let carol_key = carol_record().keys[0];

        // When: the laptop replays the phone's channel
        seed_channel(&a, &b);
        b.adopt_lens_ops();

        // Then: an offer with provenance — and no contact write
        let offers = b.lens_offers().expect("offers");
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].petname, "carol");
        assert_eq!(offers[0].author, a.public_key());
        assert!(b.person_by_label("carol").is_err(), "nothing wrote itself");

        // When: the explicit accept — the only write
        let petname = b.accept_offer(&carol_key).expect("accept");

        // Then: stored under the offered petname; the offer is gone; and
        // the accept's own onward offer is a no-op back on the phone
        assert_eq!(petname, "carol");
        assert!(b.person_by_label("carol").is_ok());
        assert!(b.lens_offers().expect("offers").is_empty());
        seed_channel(&b, &a);
        a.adopt_lens_ops();
        assert!(
            a.lens_offers().expect("offers").is_empty(),
            "already held — the re-offer loop breaks"
        );

        let _ = std::fs::remove_dir_all(temp_root("lens-offer"));
    }

    #[tokio::test]
    async fn repudiate__should_void_the_siblings_pending_offers() {
        // Given: a pending offer from the phone
        let (a, b) = paired("lens-void");
        a.add_contact(&carol_record(), None).expect("phone adds");
        seed_channel(&a, &b);
        b.adopt_lens_ops();
        assert_eq!(b.lens_offers().expect("offers").len(), 1);

        // When: the laptop repudiates the phone
        b.repudiate(a.public_key()).expect("repudiate");

        // Then: its offers are voided — the offer gates the write surface
        assert!(b.lens_offers().expect("offers").is_empty());
        assert!(b.state.offers().is_empty(), "removed, not merely filtered");

        let _ = std::fs::remove_dir_all(temp_root("lens-void"));
    }

    #[tokio::test]
    async fn adopt__should_ignore_ops_from_non_siblings() {
        // Given: a stranger's conversation whose genesis is a Hello frame
        // and whose follow-up renames b's carol — hostile mimicry
        let (_, b) = paired("lens-hostile");
        b.add_contact(&carol_record(), None).expect("laptop adds");
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
        b.state.store_envelope(genesis_id, &hello).expect("seed");
        b.state.store_envelope(genesis_id, &rename).expect("seed");

        // When
        b.adopt_lens_ops();

        // Then: no effect, no conflict — and the stranger's conversation
        // is NOT classified as a lens channel (it stays an inbox request)
        assert!(b.person_by_label("carol").is_ok());
        assert!(b.lens_conflicts().is_empty());
        assert!(!b.state.lens_channels().contains(&genesis_id));

        let _ = std::fs::remove_dir_all(temp_root("lens-hostile"));
    }
}
