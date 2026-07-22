//! The conversation DAG store (see `docs/design/dag-store.md`). Pure and
//! in-memory: it orders `MessageCore`s the caller has already authenticated.
//! Out-of-order arrival is the normal case; incompleteness is tracked, never
//! hidden (tenets 6 and 7).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::keys::PublicKey;
use crate::message::{MessageCore, MessageId};

/// One conversation's causal DAG, rooted at its genesis message.
#[derive(Debug, Clone)]
pub struct ConversationDag {
    /// The genesis id — the conversation id every later message carries.
    conversation: MessageId,
    messages: BTreeMap<MessageId, MessageCore>,
    /// Child edges, including edges from known children to parents that have
    /// not arrived yet — so heads are correct the moment a late parent lands.
    children: BTreeMap<MessageId, BTreeSet<MessageId>>,
    heads: BTreeSet<MessageId>,
    missing: BTreeSet<MessageId>,
}

impl ConversationDag {
    /// Start a conversation from its genesis (SPEC §4.1: `conversation =
    /// None`, no parents, `seq = 0`, `logical = 0`).
    pub fn new(genesis: MessageCore) -> Result<Self, DagError> {
        let is_genesis_shaped = genesis.conversation.is_none()
            && genesis.parents.is_empty()
            && genesis.seq == 0
            && genesis.logical == 0;
        if !is_genesis_shaped {
            return Err(DagError::InvalidGenesis);
        }
        let id = genesis.id();
        let mut messages = BTreeMap::new();
        messages.insert(id, genesis);
        Ok(Self {
            conversation: id,
            messages,
            children: BTreeMap::new(),
            heads: BTreeSet::from([id]),
            missing: BTreeSet::new(),
        })
    }

    /// Insert an authenticated core. Idempotent; parents may be unknown
    /// (they are tracked as missing until they arrive).
    pub fn insert(&mut self, core: MessageCore) -> Result<InsertOutcome, DagError> {
        if core.conversation.is_none() {
            // A second genesis: ours is idempotent, any other is foreign.
            return if core.id() == self.conversation {
                Ok(InsertOutcome::AlreadyKnown)
            } else {
                Err(DagError::WrongConversation)
            };
        }
        if core.conversation != Some(self.conversation) {
            return Err(DagError::WrongConversation);
        }
        if core.parents.is_empty() {
            return Err(DagError::MissingParents);
        }
        if core.logical == 0 {
            // Lamport is `1 + max(parents)`; 0 would sort before the genesis.
            return Err(DagError::InvalidLogical);
        }

        let id = core.id();
        if self.messages.contains_key(&id) {
            return Ok(InsertOutcome::AlreadyKnown);
        }
        for parent in &core.parents {
            self.children.entry(*parent).or_default().insert(id);
            self.heads.remove(parent);
            if !self.messages.contains_key(parent) {
                self.missing.insert(*parent);
            }
        }
        self.missing.remove(&id);
        let has_known_children = self.children.get(&id).is_some_and(|c| !c.is_empty());
        if !has_known_children {
            self.heads.insert(id);
        }
        self.messages.insert(id, core);
        Ok(InsertOutcome::Inserted)
    }

    pub fn conversation(&self) -> MessageId {
        self.conversation
    }

    pub fn get(&self, id: &MessageId) -> Option<&MessageCore> {
        self.messages.get(id)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        // A dag always holds at least its genesis.
        false
    }

    /// Known messages with no known children — the `parents` of the next
    /// send. Sorted by id (deterministic).
    pub fn heads(&self) -> Vec<MessageId> {
        self.heads.iter().copied().collect()
    }

    /// Referenced parents we do not hold: each one is a known, honest gap.
    pub fn missing_parents(&self) -> Vec<MessageId> {
        self.missing.iter().copied().collect()
    }

    /// The linear default view: every known message sorted by
    /// `(logical, id)`. The key is intrinsic to each message, so any two
    /// partial views agree on the relative order of messages they share.
    pub fn linearize(&self) -> Vec<MessageId> {
        let mut keyed: Vec<(u64, MessageId)> = self
            .messages
            .iter()
            .map(|(id, core)| (core.logical, *id))
            .collect();
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, id)| id).collect()
    }

    /// Ids that are causally incomparable with their `linearize`
    /// predecessor — pairs the deterministic default renders adjacent but
    /// which actually **crossed in flight** (tenet 7: concurrency is real
    /// data; web-of-trust.md §6). Presentation data only; the linear
    /// order itself is untouched.
    pub fn crossed_in_flight(&self) -> BTreeSet<MessageId> {
        let order = self.linearize();
        order
            .windows(2)
            .filter(|pair| !self.is_ancestor(pair[0], pair[1]))
            .map(|pair| pair[1])
            .collect()
    }

    /// Whether `ancestor` is reachable from `descendant` through parent
    /// edges — the causal-order test. A message is not its own ancestor.
    pub fn is_ancestor(&self, ancestor: MessageId, descendant: MessageId) -> bool {
        let mut pending = vec![descendant];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            let Some(core) = self.messages.get(&id) else {
                continue; // a missing parent is an honest gap, not an edge
            };
            for parent in &core.parents {
                if *parent == ancestor {
                    return true;
                }
                if seen.insert(*parent) {
                    pending.push(*parent);
                }
            }
        }
        false
    }

    /// Lamport value for the next message sent from this view.
    pub fn next_logical(&self) -> u64 {
        1 + self
            .heads
            .iter()
            .filter_map(|id| self.messages.get(id))
            .map(|core| core.logical)
            .max()
            .unwrap_or(0)
    }

    /// Next `seq` for `sender` in this conversation (0-based).
    pub fn next_seq(&self, sender: &PublicKey) -> u64 {
        self.messages
            .values()
            .filter(|core| core.sender == *sender)
            .map(|core| core.seq + 1)
            .max()
            .unwrap_or(0)
    }

    /// Per sender, the missing `seq` values below that sender's highest seen
    /// one. Holes only — a sender's missing *newest* messages need the
    /// sync-time head/seq advert (SPEC §11, later slice).
    pub fn seq_gaps(&self) -> Vec<(PublicKey, Vec<u64>)> {
        let mut seen: BTreeMap<PublicKey, BTreeSet<u64>> = BTreeMap::new();
        for core in self.messages.values() {
            seen.entry(core.sender).or_default().insert(core.seq);
        }
        seen.into_iter()
            .filter_map(|(sender, seqs)| {
                let max = *seqs.last().expect("non-empty by construction");
                let gaps: Vec<u64> = (0..max).filter(|seq| !seqs.contains(seq)).collect();
                (!gaps.is_empty()).then_some((sender, gaps))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    AlreadyKnown,
}

/// Structural rejection — the only kind the store makes (design doc:
/// ordering is display policy, not integrity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DagError {
    /// Not shaped like a genesis (SPEC §4.1).
    InvalidGenesis,
    /// The message belongs to a different conversation.
    WrongConversation,
    /// A non-genesis message must reference its parents.
    MissingParents,
    /// A non-genesis message cannot have `logical = 0`.
    InvalidLogical,
}

impl fmt::Display for DagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGenesis => write!(f, "not a valid genesis message"),
            Self::WrongConversation => write!(f, "message belongs to another conversation"),
            Self::MissingParents => write!(f, "non-genesis message without parents"),
            Self::InvalidLogical => write!(f, "non-genesis message with logical = 0"),
        }
    }
}

impl std::error::Error for DagError {}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::FORMAT_VERSION;
    use crate::keys::DeviceKey;
    use crate::message::KeyCommitment;

    fn sender(n: u8) -> PublicKey {
        DeviceKey::from_seed([n; 32]).public()
    }

    fn genesis() -> MessageCore {
        MessageCore {
            version: FORMAT_VERSION,
            conversation: None,
            parents: vec![],
            recipients: vec![],
            sender: sender(1),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: vec![],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        }
    }

    /// A non-genesis message; `marker` varies the body so ids are distinct.
    fn message(
        dag: &ConversationDag,
        parents: Vec<MessageId>,
        logical: u64,
        seq: u64,
        from: u8,
        marker: u8,
    ) -> MessageCore {
        MessageCore {
            version: FORMAT_VERSION,
            conversation: Some(dag.conversation()),
            parents,
            recipients: vec![],
            sender: sender(from),
            seq,
            logical,
            timestamp_ms: 0,
            body: vec![marker],
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        }
    }

    #[test]
    fn crossed_in_flight__should_mark_concurrent_neighbors_and_only_them() {
        // Given: genesis → A, and B concurrent with A (same parent); then
        // C merging both — the classic crossed-in-flight fork
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let root = dag.conversation();
        let a = message(&dag, vec![root], 1, 0, 2, 1);
        let b = message(&dag, vec![root], 1, 0, 3, 2);
        dag.insert(a.clone()).unwrap();
        dag.insert(b.clone()).unwrap();
        let c = message(&dag, vec![a.id(), b.id()], 2, 1, 2, 3);
        dag.insert(c.clone()).unwrap();

        // When
        let crossed = dag.crossed_in_flight();

        // Then: exactly the linearized-second of the concurrent pair is
        // marked — the chain edges (genesis→first, merge) are not
        let order = dag.linearize();
        assert_eq!(order.len(), 4);
        assert_eq!(crossed.len(), 1);
        assert!(crossed.contains(&order[2]), "the second of the A/B pair");
        assert!(!crossed.contains(&order[1]));
        assert!(!crossed.contains(&c.id()), "the merge follows both");
    }

    #[test]
    fn crossed_in_flight__should_be_empty_for_a_linear_chain() {
        // Given
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let a = message(&dag, vec![dag.conversation()], 1, 0, 2, 1);
        dag.insert(a.clone()).unwrap();
        let b = message(&dag, vec![a.id()], 2, 1, 2, 2);
        dag.insert(b).unwrap();

        // When / Then
        assert!(dag.crossed_in_flight().is_empty());
    }

    #[test]
    fn is_ancestor__should_follow_parent_edges_and_nothing_else() {
        // Given: genesis → A → B, with X concurrent to both
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let root = dag.conversation();
        let a = message(&dag, vec![root], 1, 0, 2, 1);
        dag.insert(a.clone()).unwrap();
        let b = message(&dag, vec![a.id()], 2, 1, 2, 2);
        dag.insert(b.clone()).unwrap();
        let x = message(&dag, vec![root], 1, 0, 3, 3);
        dag.insert(x.clone()).unwrap();

        // Then
        assert!(dag.is_ancestor(root, b.id()));
        assert!(dag.is_ancestor(a.id(), b.id()));
        assert!(!dag.is_ancestor(b.id(), a.id()), "never backwards");
        assert!(
            !dag.is_ancestor(x.id(), b.id()),
            "concurrency is not descent"
        );
        assert!(!dag.is_ancestor(b.id(), b.id()), "not its own ancestor");
    }

    #[test]
    fn new__should_reject_a_core_that_is_not_genesis_shaped() {
        // Given: a genesis with a nonzero logical
        let mut core = genesis();
        core.logical = 1;

        // When / Then
        assert_eq!(
            ConversationDag::new(core).unwrap_err(),
            DagError::InvalidGenesis
        );
    }

    #[test]
    fn insert__should_reject_a_message_from_another_conversation() {
        // Given
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let mut foreign = message(&dag, vec![dag.conversation()], 1, 0, 2, 0);
        foreign.conversation = Some(MessageId([9; 32]));

        // When / Then
        assert_eq!(dag.insert(foreign), Err(DagError::WrongConversation));
    }

    #[test]
    fn insert__should_reject_a_non_genesis_without_parents() {
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let orphan = message(&dag, vec![], 1, 0, 2, 0);
        assert_eq!(dag.insert(orphan), Err(DagError::MissingParents));
    }

    #[test]
    fn insert__should_be_idempotent() {
        // Given
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let m = message(&dag, vec![dag.conversation()], 1, 0, 2, 0);

        // When / Then
        assert_eq!(dag.insert(m.clone()), Ok(InsertOutcome::Inserted));
        assert_eq!(dag.insert(m), Ok(InsertOutcome::AlreadyKnown));
        assert_eq!(dag.insert(genesis()), Ok(InsertOutcome::AlreadyKnown));
        assert_eq!(dag.len(), 2);
    }

    #[test]
    fn heads__should_track_a_chain_and_a_fork() {
        // Given
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();

        // When: a chain g <- a, then a fork a <- b / a <- c
        let a = message(&dag, vec![g], 1, 0, 2, b'a');
        let a_id = a.id();
        dag.insert(a).unwrap();
        assert_eq!(dag.heads(), vec![a_id]);

        let b = message(&dag, vec![a_id], 2, 1, 2, b'b');
        let c = message(&dag, vec![a_id], 2, 0, 3, b'c');
        let (b_id, c_id) = (b.id(), c.id());
        dag.insert(b).unwrap();
        dag.insert(c).unwrap();

        // Then: both fork tips are heads, sorted by id
        let mut expected = vec![b_id, c_id];
        expected.sort();
        assert_eq!(dag.heads(), expected);
    }

    #[test]
    fn heads__should_stay_correct_when_a_parent_arrives_after_its_child() {
        // Given: child b (parent a) arrives before a
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();
        let a = message(&dag, vec![g], 1, 0, 2, b'a');
        let a_id = a.id();
        let b = message(&dag, vec![a_id], 2, 1, 2, b'b');
        let b_id = b.id();

        // When
        dag.insert(b).unwrap();
        assert_eq!(dag.missing_parents(), vec![a_id]);

        dag.insert(a).unwrap();

        // Then: the late parent is not a head — its child was already known
        assert_eq!(dag.missing_parents(), vec![]);
        assert_eq!(dag.heads(), vec![b_id]);
    }

    #[test]
    fn linearize__should_order_concurrent_messages_deterministically() {
        // Given: two concurrent messages (same logical, different senders)
        let dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();
        let b = message(&dag, vec![g], 1, 0, 2, b'b');
        let c = message(&dag, vec![g], 1, 0, 3, b'c');
        let (b_id, c_id) = (b.id(), c.id());

        // When: inserted in both orders
        let mut forward = dag.clone();
        forward.insert(b.clone()).unwrap();
        forward.insert(c.clone()).unwrap();
        let mut reverse = dag.clone();
        reverse.insert(c).unwrap();
        reverse.insert(b).unwrap();

        // Then: identical order — genesis first, tie broken by id
        assert_eq!(forward.linearize(), reverse.linearize());
        assert_eq!(forward.linearize()[0], g);
        assert_eq!(forward.linearize()[1..], [b_id.min(c_id), b_id.max(c_id)]);
    }

    #[test]
    fn linearize__should_agree_with_a_partial_view_on_shared_messages() {
        // Given: a full view with a fork, and a partial view missing one branch
        let mut full = ConversationDag::new(genesis()).unwrap();
        let g = full.conversation();
        let a = message(&full, vec![g], 1, 0, 2, b'a');
        let a_id = a.id();
        let b = message(&full, vec![a_id], 2, 1, 2, b'b');
        let c = message(&full, vec![a_id], 2, 0, 3, b'c');
        let d = message(&full, vec![b.id(), c.id()], 3, 1, 3, b'd');

        let mut partial = ConversationDag::new(genesis()).unwrap();
        for m in [&a, &c, &d] {
            partial.insert(m.clone()).unwrap();
        }
        for m in [a, b, c, d] {
            full.insert(m).unwrap();
        }

        // When
        let full_order = full.linearize();
        let partial_order = partial.linearize();

        // Then: the partial view's order is the full order restricted to it
        let restricted: Vec<_> = full_order
            .into_iter()
            .filter(|id| partial_order.contains(id))
            .collect();
        assert_eq!(partial_order, restricted);
    }

    #[test]
    fn seq_gaps__should_detect_holes_per_sender() {
        // Given: sender 2 sent seq 0,1,3 — seq 2 never arrived
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();
        let m0 = message(&dag, vec![g], 1, 0, 2, b'0');
        let m1 = message(&dag, vec![m0.id()], 2, 1, 2, b'1');
        let m3 = message(&dag, vec![m1.id()], 3, 3, 2, b'3');
        for m in [m0, m1, m3] {
            dag.insert(m).unwrap();
        }

        // When / Then
        assert_eq!(dag.seq_gaps(), vec![(sender(2), vec![2])]);
    }

    #[test]
    fn seq_gaps__should_be_empty_for_contiguous_senders() {
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();
        let m0 = message(&dag, vec![g], 1, 0, 2, b'0');
        let m1 = message(&dag, vec![m0.id()], 2, 1, 2, b'1');
        for m in [m0, m1] {
            dag.insert(m).unwrap();
        }
        assert_eq!(dag.seq_gaps(), vec![]);
    }

    #[test]
    fn drafting_inputs__should_reflect_heads_and_history() {
        // Given: a fork after one message
        let mut dag = ConversationDag::new(genesis()).unwrap();
        let g = dag.conversation();
        let a = message(&dag, vec![g], 1, 0, 2, b'a');
        let a_id = a.id();
        let b = message(&dag, vec![a_id], 2, 1, 2, b'b');
        let c = message(&dag, vec![a_id], 2, 0, 3, b'c');
        for m in [a, b, c] {
            dag.insert(m).unwrap();
        }

        // Then: next message points at both heads, above both logicals
        assert_eq!(dag.heads().len(), 2);
        assert_eq!(dag.next_logical(), 3);
        assert_eq!(dag.next_seq(&sender(2)), 2);
        assert_eq!(dag.next_seq(&sender(3)), 1);
        assert_eq!(dag.next_seq(&sender(9)), 0);
    }
}
