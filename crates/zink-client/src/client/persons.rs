//! Person entries (project 7 S2): the observer's local lens over key
//! clusters — a label spanning one or more contact entries, which is what
//! "write a message to Alice" resolves. Purely client-side belief: entries
//! never travel, never enter the protocol, and reference the per-device
//! contact entries underneath (multi-device.md §7's display-vs-addressing
//! separation, cashed in). **Every contact entry belongs to a person** —
//! the eager invariant: adding a contact creates its person row (label
//! initialized from the petname; independent facts thereafter), record
//! updates re-point the member stem, and `persons()` self-heals an
//! unclaimed entry on sight (the crash-gap net — normally a no-op).

use std::collections::BTreeSet;
use std::fmt;

use zink_protocol::{ContactRecord, PublicKey};

use crate::error::Error;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::{Draw, Mint};
use crate::ports::transport::Transport;

use super::Client;
use super::contacts::Contact;

/// The opaque local person id — what every act and page fetch keys on.
/// **Ids identify; labels display and address**: a label is my mutable
/// lens, so nothing holds a reference by it (labels resolve exactly once,
/// at the human boundary — `person_by_label`). An id is an arbitrary
/// local token: a 128-bit uniqueness draw through the rng port's `Mint`
/// capability (scriptable, unlike crypto randomness — distinctness is the
/// whole contract), never derived from keys or content (clusters merge,
/// split, and rename), and never on the wire. The string form is
/// `person:<32 hex>` — typed, so an id in CLI output or a log never reads
/// as just another hex blob (keys, message ids and blob hashes all are),
/// and parsing rejects everything else, stray bare hex included. It
/// round-trips the app's DTO boundary unread. Storage filenames take the
/// raw `u128` instead: bare `{:032x}` — prefix-free and fs-safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersonId(u128);

/// The display/parse scheme tag. Deliberately not the wire-artifact style
/// (`ZINK:`… is for things that travel); person ids never do.
const PERSON_ID_PREFIX: &str = "person:";

impl PersonId {
    /// Mint a fresh id — creation sites only (add, split, repair).
    pub(crate) fn mint(rng: &impl Mint) -> Self {
        Self(rng.token128())
    }

    /// The raw id the storage layer files under.
    pub(crate) fn to_storage(self) -> u128 {
        self.0
    }
}

impl fmt::Display for PersonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PERSON_ID_PREFIX}{:032x}", self.0)
    }
}

impl std::str::FromStr for PersonId {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self, Error> {
        raw.strip_prefix(PERSON_ID_PREFIX)
            .filter(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u128::from_str_radix(hex, 16).ok())
            .map(Self)
            .ok_or_else(|| Error::InvalidInput(format!("not a person id: {raw:?}")))
    }
}

/// One person as this client currently believes it: the id, the addressing
/// label, and the member contact entries (the per-device layer — each with
/// its own petname, record, and relays).
#[derive(Clone, Debug)]
pub struct PersonEntry {
    pub id: PersonId,
    pub label: String,
    pub members: Vec<(String, ContactRecord)>,
}

impl PersonEntry {
    /// Every key any member's record lists — the cluster, as evidence.
    pub fn keys(&self) -> BTreeSet<PublicKey> {
        self.members
            .iter()
            .flat_map(|(_, record)| record.keys.iter().copied())
            .collect()
    }

    fn member_stems(&self) -> Vec<PublicKey> {
        self.members
            .iter()
            .filter_map(|(_, record)| record.keys.first().copied())
            .collect()
    }
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw + Mint> Client<C, W, N, R> {
    /// Every person this client believes in, label-sorted. Self-healing:
    /// persisted members whose contact entry is gone drop out (an emptied
    /// person hides), a stem two rows claim (a crash inside a merge)
    /// renders under the first row only, and a contact entry no person
    /// claims — a crash gap, or a store from before the eager invariant —
    /// gets its row minted on sight, so every entry renders exactly once.
    pub fn persons(&self) -> Result<Vec<PersonEntry>, Error> {
        let contacts = self.state.contacts()?;
        let entry_for = |stem: &PublicKey| {
            contacts
                .iter()
                .find(|(_, record)| record.keys.first() == Some(stem))
        };
        let mut persons = Vec::new();
        let mut claimed: BTreeSet<PublicKey> = BTreeSet::new();
        for (id, label, member_stems) in self.state.persons() {
            let members: Vec<(String, ContactRecord)> = member_stems
                .iter()
                .filter(|stem| !claimed.contains(stem))
                .filter_map(entry_for)
                .cloned()
                .collect();
            claimed.extend(members.iter().filter_map(|(_, r)| r.keys.first().copied()));
            if !members.is_empty() {
                persons.push(PersonEntry {
                    id: PersonId(id),
                    label,
                    members,
                });
            }
        }
        for (petname, record) in &contacts {
            let Some(&stem) = record.keys.first() else {
                continue;
            };
            if claimed.contains(&stem) {
                continue;
            }
            let id = self.claim_entry(petname, stem)?;
            persons.push(PersonEntry {
                id,
                label: petname.clone(),
                members: vec![(petname.clone(), record.clone())],
            });
        }
        persons.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(persons)
    }

    /// Mint and persist the person row claiming one contact entry — the
    /// add-time companion of `save_contact`, and what `persons()` runs on
    /// an unclaimed entry. The label starts as the petname; they are
    /// independent facts from here on (rename_person moves one, the
    /// contact-store rename the other).
    pub(super) fn claim_entry(&self, petname: &str, stem: PublicKey) -> Result<PersonId, Error> {
        let id = PersonId::mint(&self.rng);
        self.state.save_person(id.to_storage(), petname, &[stem])?;
        Ok(id)
    }

    /// Resolve a name to send-ready recipients: the person layer first (one
    /// `Contact` per member entry, so every key rides **its own entry's**
    /// relays — relays bind to the publishing device, SPEC §3.6), falling
    /// back to the per-device layer (an entry petname addresses that device
    /// alone — the manual override for "message Alice's phone").
    pub fn resolve_person(&self, name: &str) -> Result<Vec<Contact>, Error> {
        match self.person_by_label(name) {
            Ok(person) => Ok(person
                .members
                .iter()
                .map(|(_, record)| self.contact_from(record))
                .collect()),
            Err(Error::NotAContact(_)) => self.resolve_contact(name).map(|contact| vec![contact]),
            Err(error) => Err(error),
        }
    }

    /// Merge one person into another — the explicit clustering act (the
    /// evidence popup's accept, or a manual merge). `into` keeps its label
    /// and id; `from` dissolves. Advisory evidence never merges anything:
    /// this act is the only path.
    pub fn merge_persons(&self, into: PersonId, from: PersonId) -> Result<PersonEntry, Error> {
        if into == from {
            return Err(Error::InvalidInput(
                "cannot merge a person into itself".into(),
            ));
        }
        let keep = self.person_by_id(into)?;
        let absorb = self.person_by_id(from)?;
        let mut members = keep.member_stems();
        members.extend(absorb.member_stems());
        // Grow first, remove after: a crash in between double-claims the
        // stems, which `persons()` renders once (first row wins) until the
        // next act rewrites — never a lost entry.
        self.state
            .save_person(into.to_storage(), &keep.label, &members)?;
        self.state.remove_person(from.to_storage());
        self.person_by_id(into)
    }

    /// Split a member entry back out to its own person, labeled by its
    /// petname — the undo of a merge (or of a wrong accept). The source
    /// person keeps its label and the rest of its members.
    pub fn split_person(&self, member_petname: &str) -> Result<PersonEntry, Error> {
        let source = self
            .persons()?
            .into_iter()
            .find(|person| {
                person
                    .members
                    .iter()
                    .any(|(petname, _)| petname == member_petname)
            })
            .ok_or_else(|| Error::NotAContact(format!("no contact named {member_petname:?}")))?;
        if source.members.len() == 1 {
            return Err(Error::InvalidInput(
                "already its own person — nothing to split".into(),
            ));
        }
        // The split-off person is labeled by the member's petname, and
        // labels stay unique across persons so send-by-name stays
        // unambiguous. The source's own label counts: a merged person is
        // often labeled by one member's petname (the merge keeps the kept
        // person's label), and splitting that namesake member out must
        // refuse rather than mint a twin — rename the person first.
        if source.label == member_petname
            || self
                .persons()?
                .iter()
                .any(|person| person.id != source.id && person.label == member_petname)
        {
            return Err(Error::PetnameCollision(member_petname.to_string()));
        }
        let (kept, split): (Vec<_>, Vec<_>) = source
            .members
            .iter()
            .partition(|(petname, _)| petname != member_petname);
        // Shrink first, split after: a crash in between leaves the member
        // unclaimed, and the `persons()` self-heal re-mints it as its own
        // singleton — the very outcome this act wanted.
        self.state.save_person(
            source.id.to_storage(),
            &source.label,
            &kept
                .iter()
                .filter_map(|(_, record)| record.keys.first().copied())
                .collect::<Vec<_>>(),
        )?;
        let split_id = PersonId::mint(&self.rng);
        self.state.save_person(
            split_id.to_storage(),
            member_petname,
            &split
                .iter()
                .filter_map(|(_, record)| record.keys.first().copied())
                .collect::<Vec<_>>(),
        )?;
        self.person_by_id(split_id)
    }

    /// Rename a person — the addressing label, my lens (like a petname,
    /// scoped to the cluster). Refuses a collision with any other person
    /// label or contact petname, except its own members' petnames: the
    /// person layer resolves first, so shadowing our own member stays
    /// unambiguous.
    pub fn rename_person(&self, id: PersonId, new: &str) -> Result<(), Error> {
        let new = new.trim();
        if new.is_empty() {
            return Err(Error::InvalidInput("person label cannot be empty".into()));
        }
        let person = self.person_by_id(id)?;
        if new == person.label {
            return Ok(());
        }
        self.ensure_label_free(new, Some(&person))?;
        self.state
            .save_person(id.to_storage(), new, &person.member_stems())
    }

    /// Look a person up by id — the reference every act takes. Total over
    /// everything `persons()` renders: ids are stable for a person's whole
    /// lifetime (only a split's new person, or a merge's dissolved one,
    /// changes the id set).
    pub fn person_by_id(&self, id: PersonId) -> Result<PersonEntry, Error> {
        self.persons()?
            .into_iter()
            .find(|person| person.id == id)
            .ok_or_else(|| Error::NotAContact(format!("no person with id {id}")))
    }

    /// Resolve a label to a person — the human boundary (a CLI argument,
    /// send-by-name), and the only place labels resolve. Labels are unique
    /// by construction (`ensure_label_free`); a duplicate means a damaged
    /// store and errors honestly instead of first-match-wins.
    pub fn person_by_label(&self, label: &str) -> Result<PersonEntry, Error> {
        let mut matching = self
            .persons()?
            .into_iter()
            .filter(|person| person.label == label);
        let person = matching
            .next()
            .ok_or_else(|| Error::NotAContact(format!("no person labeled {label:?}")))?;
        if matching.next().is_some() {
            return Err(Error::InvalidInput(format!(
                "several persons are labeled {label:?} — rename one, or act by id"
            )));
        }
        Ok(person)
    }

    /// The joint-namespace collision check (S2: the collision rule moves to
    /// person labels): a name must not already be another person's label or
    /// a contact petname outside `exempt`'s members.
    pub(super) fn ensure_label_free(
        &self,
        name: &str,
        exempt: Option<&PersonEntry>,
    ) -> Result<(), Error> {
        let exempt_id = exempt.map(|person| person.id);
        for person in self.persons()? {
            if Some(person.id) == exempt_id {
                continue;
            }
            if person.label == name || person.members.iter().any(|(petname, _)| petname == name) {
                return Err(Error::PetnameCollision(name.to_string()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, RelayEntry};

    use super::super::Client;
    use super::super::test_kit::{signed_record, temp_key, temp_root};
    use super::PersonId;
    use crate::error::Error;
    use crate::hex;

    fn device_key(n: u8) -> DeviceKey {
        DeviceKey::from_seed([n; 32])
    }

    fn relay(n: u8) -> Vec<RelayEntry> {
        vec![RelayEntry::from_spec(&format!(
            "{}@203.0.113.{n}:1",
            hex::encode(&device_key(100 + n).public().0)
        ))]
    }

    /// The label→id hop the acts no longer do themselves — tests speak
    /// labels (readable), acts take ids.
    fn id_of<C, W, N, R>(client: &Client<C, W, N, R>, label: &str) -> PersonId
    where
        C: crate::ports::clock::Clock,
        W: crate::ports::clock::WallClock,
        N: crate::ports::transport::Transport,
        R: crate::ports::rng::Draw + crate::ports::rng::Mint,
    {
        client.person_by_label(label).expect("person by label").id
    }

    #[test]
    fn person_id__should_mint_distinct_ids_that_roundtrip_the_boundary() {
        // Given: a scripted mint — the seam the Mint port buys
        let mint = crate::ports::rng::TestMint(std::cell::Cell::new(0));

        // When
        let a = PersonId::mint(&mint);
        let b = PersonId::mint(&mint);

        // Then: distinct; the typed `person:<32 hex>` form parses back to
        // itself; anything else — bare hex included — refuses at the
        // boundary
        assert_ne!(a, b);
        let shown = a.to_string();
        assert!(shown.starts_with("person:"), "typed, not bare hex: {shown}");
        assert_eq!(shown.len(), "person:".len() + 32);
        assert_eq!(shown.parse::<PersonId>().expect("roundtrip"), a);
        assert!(
            shown
                .trim_start_matches("person:")
                .parse::<PersonId>()
                .is_err()
        );
        assert!("not-an-id".parse::<PersonId>().is_err());
        assert!("p1".parse::<PersonId>().is_err());
    }

    #[tokio::test]
    async fn add_contact__should_create_one_person_per_entry() {
        // Given / When: two ordinary contact adds — the eager invariant
        let a = Client::open_or_create(&temp_key("pmat", "me"))
            .await
            .expect("open");
        a.add_contact(
            &signed_record(&device_key(1), "alice-phone", 0, relay(1)),
            None,
        )
        .expect("add");
        a.add_contact(&signed_record(&device_key(2), "bob", 0, relay(2)), None)
            .expect("add");

        // Then: one persisted person row per entry, labeled by its petname
        assert_eq!(a.state.persons().len(), 2, "rows persisted at add time");
        let labels: Vec<(String, usize)> = a
            .persons()
            .expect("persons")
            .iter()
            .map(|person| (person.label.clone(), person.members.len()))
            .collect();
        assert_eq!(
            labels,
            vec![("alice-phone".to_string(), 1), ("bob".to_string(), 1)]
        );

        let _ = std::fs::remove_dir_all(temp_root("pmat"));
    }

    #[tokio::test]
    async fn persons__should_claim_an_entry_that_bypassed_the_add_act() {
        // Given: an entry written below the add act (a crash gap, or a
        // store from before the eager invariant) — no person row
        let a = Client::open_or_create(&temp_key("pheal", "me"))
            .await
            .expect("open");
        a.state
            .save_contact(
                "carol",
                &signed_record(&device_key(3), "carol", 0, relay(3)),
            )
            .expect("save");
        assert_eq!(a.state.persons().len(), 0);

        // When: any read runs
        let persons = a.persons().expect("persons");

        // Then: the invariant self-healed — claimed, rendered, persisted
        assert_eq!(persons.len(), 1);
        assert_eq!(persons[0].label, "carol");
        assert_eq!(a.state.persons().len(), 1);

        let _ = std::fs::remove_dir_all(temp_root("pheal"));
    }

    #[tokio::test]
    async fn merge_persons__should_cluster_and_resolve_per_entry_relays() {
        // Given: Alice's two devices as two contact entries with distinct
        // relay sets
        let a = Client::open_or_create(&temp_key("pmerge", "me"))
            .await
            .expect("open");
        let (phone, laptop) = (device_key(1), device_key(2));
        a.add_contact(&signed_record(&phone, "Alice", 0, relay(1)), None)
            .expect("add phone");
        a.add_contact(&signed_record(&laptop, "alice-laptop", 0, relay(2)), None)
            .expect("add laptop");

        // When: the explicit clustering act
        let merged = a
            .merge_persons(id_of(&a, "Alice"), id_of(&a, "alice-laptop"))
            .expect("merge");

        // Then: one person, two member entries; addressing the person
        // reaches both keys, each riding its own entry's relays (relays
        // bind to the publishing device — SPEC §3.6)
        assert_eq!(merged.members.len(), 2);
        assert_eq!(a.persons().expect("persons").len(), 1);
        let contacts = a.resolve_person("Alice").expect("resolve");
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].keys, vec![phone.public()]);
        assert_eq!(contacts[1].keys, vec![laptop.public()]);
        assert_ne!(contacts[0].relays, contacts[1].relays);
        // The per-device layer stays addressable underneath
        let device_only = a.resolve_person("alice-laptop").expect("member");
        assert_eq!(device_only.len(), 1);
        assert_eq!(device_only[0].keys, vec![laptop.public()]);

        let _ = std::fs::remove_dir_all(temp_root("pmerge"));
    }

    #[tokio::test]
    async fn stage_send__to_a_merged_person_should_reach_both_keys() {
        // Given: a merged two-device person
        let a = Client::open_or_create(&temp_key("psend", "me"))
            .await
            .expect("open");
        let (phone, laptop) = (device_key(1), device_key(2));
        a.add_contact(&signed_record(&phone, "Alice", 0, relay(1)), None)
            .expect("add");
        a.add_contact(&signed_record(&laptop, "alice-laptop", 0, relay(2)), None)
            .expect("add");
        a.merge_persons(id_of(&a, "Alice"), id_of(&a, "alice-laptop"))
            .expect("merge");

        // When: a send addressed by the person label (staged — local only)
        let contacts = a.resolve_person("Alice").expect("resolve");
        let staged = a
            .stage_send(&contacts, b"hi alice".to_vec(), vec![])
            .expect("stage");

        // Then: both device keys are sealed recipients
        let envelopes = a.state.load_envelopes(staged.conversation).expect("stored");
        let recipients = &envelopes
            .first()
            .expect("the staged message")
            .core
            .recipients;
        assert!(recipients.contains(&phone.public()));
        assert!(recipients.contains(&laptop.public()));

        let _ = std::fs::remove_dir_all(temp_root("psend"));
    }

    #[tokio::test]
    async fn split_person__should_return_a_member_to_its_own_person() {
        // Given
        let a = Client::open_or_create(&temp_key("psplit", "me"))
            .await
            .expect("open");
        a.add_contact(&signed_record(&device_key(1), "Alice", 0, relay(1)), None)
            .expect("add");
        a.add_contact(
            &signed_record(&device_key(2), "alice-laptop", 0, relay(2)),
            None,
        )
        .expect("add");
        a.merge_persons(id_of(&a, "Alice"), id_of(&a, "alice-laptop"))
            .expect("merge");

        // When
        let split = a.split_person("alice-laptop").expect("split");

        // Then: two persons again, nothing dangling
        assert_eq!(split.label, "alice-laptop");
        assert_eq!(split.members.len(), 1);
        let persons = a.persons().expect("persons");
        assert_eq!(persons.len(), 2);
        assert!(persons.iter().all(|person| !person.members.is_empty()));

        let _ = std::fs::remove_dir_all(temp_root("psplit"));
    }

    #[tokio::test]
    async fn split_person__should_refuse_a_split_that_would_twin_the_source_label() {
        // Given: a merged person labeled by one member's petname — the
        // shape merge_persons produces
        let a = Client::open_or_create(&temp_key("ptwin", "me"))
            .await
            .expect("open");
        a.add_contact(&signed_record(&device_key(1), "Alice", 0, relay(1)), None)
            .expect("add");
        a.add_contact(
            &signed_record(&device_key(2), "alice-laptop", 0, relay(2)),
            None,
        )
        .expect("add");
        a.merge_persons(id_of(&a, "Alice"), id_of(&a, "alice-laptop"))
            .expect("merge");

        // When: splitting the namesake member out
        let result = a.split_person("Alice");

        // Then: refused — labels stay unique, addressing stays unambiguous
        assert!(matches!(result, Err(Error::PetnameCollision(_))));
        let persons = a.persons().expect("persons");
        assert_eq!(persons.len(), 1);
        assert_eq!(persons[0].members.len(), 2);

        let _ = std::fs::remove_dir_all(temp_root("ptwin"));
    }

    #[tokio::test]
    async fn rename_person__should_move_the_label_and_refuse_collisions() {
        // Given
        let a = Client::open_or_create(&temp_key("prename", "me"))
            .await
            .expect("open");
        a.add_contact(
            &signed_record(&device_key(1), "alice-phone", 0, relay(1)),
            None,
        )
        .expect("add");
        a.add_contact(&signed_record(&device_key(2), "bob", 0, relay(2)), None)
            .expect("add");

        // When: rename by id — the id survives the label move
        let alice = id_of(&a, "alice-phone");
        a.rename_person(alice, "Alice").expect("rename");

        // Then: resolution follows the new label at the same id; the old
        // label now only reaches the member entry (petname layer);
        // collisions refuse
        assert_eq!(id_of(&a, "Alice"), alice);
        assert_eq!(a.resolve_person("Alice").expect("resolve").len(), 1);
        assert!(matches!(
            a.rename_person(alice, "bob"),
            Err(Error::PetnameCollision(_))
        ));
        // …and a new contact can't take a person label either
        assert!(matches!(
            a.add_contact(&signed_record(&device_key(3), "Alice", 0, relay(3)), None),
            Err(Error::PetnameCollision(_))
        ));

        let _ = std::fs::remove_dir_all(temp_root("prename"));
    }

    #[tokio::test]
    async fn update_contact__should_keep_a_rekeyed_member_clustered() {
        // Given: a merged person whose laptop entry is about to re-key —
        // the fresh record leads with the new key, listing the old one
        // (the multi-device.md §4 re-scan shape)
        let a = Client::open_or_create(&temp_key("pdangle", "me"))
            .await
            .expect("open");
        let (phone, laptop, fresh) = (device_key(1), device_key(2), device_key(3));
        a.add_contact(&signed_record(&phone, "Alice", 0, relay(1)), None)
            .expect("add");
        a.add_contact(&signed_record(&laptop, "alice-laptop", 0, relay(2)), None)
            .expect("add");
        a.merge_persons(id_of(&a, "Alice"), id_of(&a, "alice-laptop"))
            .expect("merge");
        let mut rekeyed = signed_record(&fresh, "alice laptop", 1, relay(2));
        rekeyed.keys.push(laptop.public());

        // When: the explicit update act moves the entry's stem
        a.update_contact(&rekeyed).expect("update");

        // Then: the membership followed the stem — nothing dangles
        let persons = a.persons().expect("persons");
        assert_eq!(persons.len(), 1, "the cluster held: {persons:?}");
        assert_eq!(persons[0].label, "Alice");
        assert_eq!(persons[0].members.len(), 2);
        assert!(persons[0].keys().contains(&fresh.public()));

        let _ = std::fs::remove_dir_all(temp_root("pdangle"));
    }
}
