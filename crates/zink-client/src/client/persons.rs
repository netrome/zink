//! Person entries (project 7 S2): the observer's local lens over key
//! clusters — a label spanning one or more contact entries, which is what
//! "write a message to Alice" resolves. Purely client-side belief: entries
//! never travel, never enter the protocol, and reference the per-device
//! contact entries underneath (multi-device.md §7's display-vs-addressing
//! separation, cashed in). Reads are pure: a contact entry no person
//! claims materializes as a virtual singleton (label = petname) — the lazy
//! migration; only the explicit acts (merge / split / rename) persist.

use std::collections::BTreeSet;

use zink_protocol::{ContactRecord, PublicKey};

use crate::error::Error;
use crate::hex;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::Transport;

use super::Client;
use super::contacts::Contact;

/// Virtual (not-yet-persisted) person ids are stem-derived and marked so
/// the acts know to mint a real id at first persist. Opaque to callers.
const VIRTUAL: char = '@';

/// One person as this client currently believes it: an opaque local id, the
/// addressing label, and the member contact entries (the per-device layer —
/// each with its own petname, record, and relays).
#[derive(Clone, Debug)]
pub struct PersonEntry {
    pub id: String,
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

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Every person this client believes in, label-sorted. Self-healing,
    /// never self-mutating: persisted entries drop members whose contact
    /// entry is gone (an emptied person hides); contact entries no person
    /// claims render as virtual singletons labeled by their petname.
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
            let members: Vec<(String, ContactRecord)> =
                member_stems.iter().filter_map(entry_for).cloned().collect();
            claimed.extend(members.iter().filter_map(|(_, r)| r.keys.first().copied()));
            if !members.is_empty() {
                persons.push(PersonEntry { id, label, members });
            }
        }
        for (petname, record) in &contacts {
            let Some(&stem) = record.keys.first() else {
                continue;
            };
            if claimed.contains(&stem) {
                continue;
            }
            persons.push(PersonEntry {
                id: format!("{VIRTUAL}{}", hex::encode(&stem.0)),
                label: petname.clone(),
                members: vec![(petname.clone(), record.clone())],
            });
        }
        persons.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(persons)
    }

    /// Resolve a name to send-ready recipients: the person layer first (one
    /// `Contact` per member entry, so every key rides **its own entry's**
    /// relays — relays bind to the publishing device, SPEC §3.6), falling
    /// back to the per-device layer (an entry petname addresses that device
    /// alone — the manual override for "message Alice's phone").
    pub fn resolve_person(&self, name: &str) -> Result<Vec<Contact>, Error> {
        if let Some(person) = self
            .persons()?
            .into_iter()
            .find(|person| person.label == name)
        {
            return Ok(person
                .members
                .iter()
                .map(|(_, record)| self.contact_from(record))
                .collect());
        }
        self.resolve_contact(name).map(|contact| vec![contact])
    }

    /// Merge one person into another — the explicit clustering act (the
    /// evidence popup's accept, or a manual merge). `into` keeps its label
    /// and id; `from` dissolves. Advisory evidence never merges anything:
    /// this act is the only path.
    pub fn merge_persons(&self, into: &str, from: &str) -> Result<PersonEntry, Error> {
        if into == from {
            return Err(Error::InvalidInput(
                "cannot merge a person into itself".into(),
            ));
        }
        let keep = self.person_by_label(into)?;
        let absorb = self.person_by_label(from)?;
        let mut members = keep.member_stems();
        members.extend(absorb.member_stems());
        let id = self.persist_id(&keep)?;
        self.state.save_person(&id, &keep.label, &members)?;
        if !absorb.id.starts_with(VIRTUAL) {
            self.state.remove_person(&absorb.id);
        }
        self.person_by_label(into)
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
        // The split-off person is labeled by the member's petname; both
        // namespaces stay collision-free so resolution stays unambiguous.
        if self
            .persons()?
            .iter()
            .any(|person| person.label == member_petname && person.id != source.id)
        {
            return Err(Error::PetnameCollision(member_petname.to_string()));
        }
        let (kept, split): (Vec<_>, Vec<_>) = source
            .members
            .iter()
            .partition(|(petname, _)| petname != member_petname);
        let source_id = self.persist_id(&source)?;
        self.state.save_person(
            &source_id,
            &source.label,
            &kept
                .iter()
                .filter_map(|(_, record)| record.keys.first().copied())
                .collect::<Vec<_>>(),
        )?;
        let split_id = self.state.next_person_id()?;
        self.state.save_person(
            &split_id,
            member_petname,
            &split
                .iter()
                .filter_map(|(_, record)| record.keys.first().copied())
                .collect::<Vec<_>>(),
        )?;
        self.person_by_label(member_petname)
    }

    /// Rename a person — the addressing label, my lens (like a petname,
    /// scoped to the cluster). Refuses a collision with any other person
    /// label or contact petname, except its own members' petnames: the
    /// person layer resolves first, so shadowing our own member stays
    /// unambiguous.
    pub fn rename_person(&self, current: &str, new: &str) -> Result<(), Error> {
        let new = new.trim();
        if new.is_empty() {
            return Err(Error::InvalidInput("person label cannot be empty".into()));
        }
        if new == current {
            return Ok(());
        }
        let person = self.person_by_label(current)?;
        self.ensure_label_free(new, Some(&person))?;
        let id = self.persist_id(&person)?;
        self.state.save_person(&id, new, &person.member_stems())
    }

    /// The joint-namespace collision check (S2: the collision rule moves to
    /// person labels): a name must not already be another person's label or
    /// a contact petname outside `exempt`'s members.
    pub(super) fn ensure_label_free(
        &self,
        name: &str,
        exempt: Option<&PersonEntry>,
    ) -> Result<(), Error> {
        let exempt_id = exempt.map(|person| person.id.as_str());
        for person in self.persons()? {
            if Some(person.id.as_str()) == exempt_id {
                continue;
            }
            if person.label == name || person.members.iter().any(|(petname, _)| petname == name) {
                return Err(Error::PetnameCollision(name.to_string()));
            }
        }
        Ok(())
    }

    fn person_by_label(&self, label: &str) -> Result<PersonEntry, Error> {
        self.persons()?
            .into_iter()
            .find(|person| person.label == label)
            .ok_or_else(|| Error::NotAContact(format!("no person labeled {label:?}")))
    }

    /// The id to persist under: a virtual singleton materializes with a
    /// freshly minted id at its first act; a persisted person keeps its own.
    fn persist_id(&self, person: &PersonEntry) -> Result<String, Error> {
        if person.id.starts_with(VIRTUAL) {
            self.state.next_person_id()
        } else {
            Ok(person.id.clone())
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, RelayEntry};

    use super::super::Client;
    use super::super::test_kit::{signed_record, temp_key, temp_root};
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

    #[tokio::test]
    async fn persons__should_materialize_one_person_per_contact_entry() {
        // Given: two ordinary contacts, no clustering act ever taken
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

        // When
        let persons = a.persons().expect("persons");

        // Then: the lazy migration — one singleton per entry, labeled by
        // its petname
        let labels: Vec<(&str, usize)> = persons
            .iter()
            .map(|person| (person.label.as_str(), person.members.len()))
            .collect();
        assert_eq!(labels, vec![("alice-phone", 1), ("bob", 1)]);

        let _ = std::fs::remove_dir_all(temp_root("pmat"));
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
        let merged = a.merge_persons("Alice", "alice-laptop").expect("merge");

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
        a.merge_persons("Alice", "alice-laptop").expect("merge");

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
        a.merge_persons("Alice", "alice-laptop").expect("merge");

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

        // When: rename a (virtual) person — it materializes
        a.rename_person("alice-phone", "Alice").expect("rename");

        // Then: resolution follows the new label; the old label now only
        // reaches the member entry (petname layer); collisions refuse
        assert!(
            a.persons()
                .expect("persons")
                .iter()
                .any(|p| p.label == "Alice")
        );
        assert_eq!(a.resolve_person("Alice").expect("resolve").len(), 1);
        assert!(matches!(
            a.rename_person("Alice", "bob"),
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
        a.merge_persons("Alice", "alice-laptop").expect("merge");
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
