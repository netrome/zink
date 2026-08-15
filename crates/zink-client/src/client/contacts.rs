//! Contacts and trust (D1–D4): the local contact store and its policy —
//! petnames, key-overlap identity, device recognition and vouches,
//! repudiation, name resolution over learned records, and turning a stored
//! record into a dialable route. All of it is client-side belief; none of
//! it enters the protocol (identity is local — who-is-this.md,
//! multi-device.md, web-of-trust.md).

use std::collections::{BTreeMap, BTreeSet};

use zink_protocol::{
    Attestation, Claim, ContactRecord, PublicKey, RelayEntry, SignedAttestation, Versioned,
};

use crate::error::Error;
use crate::hex;
use crate::ports::clock::{Clock, WallClock};
use crate::ports::rng::Draw;
use crate::ports::transport::{Peer, Transport};

use super::Client;

/// A resolved recipient: the person's device keys and the relays hosting
/// their mailboxes.
pub struct Contact {
    pub keys: Vec<PublicKey>,
    pub relays: Vec<String>,
}

impl Contact {
    /// `<pubkey-hex>@<relay>[,<relay>…]` — hex contains no `@`, so the
    /// first `@` splits key from relay list. The raw escape hatch next to
    /// named contacts.
    pub fn parse(spec: &str) -> Result<Self, Error> {
        let (key_hex, relay_list) = spec.split_once('@').ok_or_else(|| {
            Error::InvalidInput("contact must be <pubkey>@<relay>[,<relay>...]".into())
        })?;
        let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
        for relay in &relays {
            // Validate early, before any network work.
            crate::adapters::iroh::parse_dial(relay)?;
        }
        Ok(Contact {
            keys: vec![PublicKey(hex::parse32(key_hex)?)],
            relays,
        })
    }
}

/// `preview_contact`'s verdict: what storing a scanned/pasted record would
/// do. Read-only triage — the explicit act stays `add_contact` /
/// `update_contact` (R1, relay-lifecycle).
pub enum RecordMatch {
    /// No key overlap — a genuinely new person.
    New {
        /// The record's verified self-claimed name, if any — the petname
        /// prefill for the add flow.
        suggested_petname: Option<String>,
    },
    /// Shares a key with exactly one contact — storing it is an update of
    /// that entry (multi-device.md §4); the diff is what a confirm shows.
    Update(RecordUpdate),
    /// Shares keys with two or more contacts — storing would be refused.
    Ambiguous { petnames: Vec<String> },
}

/// The confirmable diff for a key-overlapping record: stored → scanned.
/// Relays are full specs (`dial[#relay-url]`); names are the verified
/// self-claims (`None` = no valid claim), compared by the edge.
pub struct RecordUpdate {
    /// The stored entry's petname — an update never renames (petnames are
    /// ours; renaming is `rename_contact`'s job).
    pub petname: String,
    pub old_name: Option<String>,
    pub new_name: Option<String>,
    pub relays_added: Vec<String>,
    pub relays_removed: Vec<String>,
    pub keys_added: usize,
    pub keys_removed: usize,
}

impl RecordUpdate {
    fn diff(petname: &str, stored: &ContactRecord, scanned: &ContactRecord) -> Self {
        let old_relays: BTreeSet<String> = stored.relays.iter().map(RelayEntry::to_spec).collect();
        let new_relays: BTreeSet<String> = scanned.relays.iter().map(RelayEntry::to_spec).collect();
        RecordUpdate {
            petname: petname.to_string(),
            old_name: stored.self_claimed_name().map(str::to_string),
            new_name: scanned.self_claimed_name().map(str::to_string),
            relays_added: new_relays.difference(&old_relays).cloned().collect(),
            relays_removed: old_relays.difference(&new_relays).cloned().collect(),
            keys_added: scanned
                .keys
                .iter()
                .filter(|key| !stored.keys.contains(key))
                .count(),
            keys_removed: stored
                .keys
                .iter()
                .filter(|key| !scanned.keys.contains(key))
                .count(),
        }
    }
}

/// The contacts sharing at least one key with the record — the identity
/// evidence add/update/preview all triage on (multi-device.md §4).
fn overlapping<'a>(
    contacts: &'a [(String, ContactRecord)],
    record: &ContactRecord,
) -> Vec<&'a (String, ContactRecord)> {
    contacts
        .iter()
        .filter(|(_, existing)| existing.keys.iter().any(|key| record.keys.contains(key)))
        .collect()
}

/// `resolve_name`'s verdict (who-is-this.md §6).
pub enum ResolvedName {
    /// The key belongs to a contact — the manual label always wins.
    Petname(String),
    /// Not a contact; what the learned store supports, best first
    /// (highest revision; a genuine tie keeps both, surfaced honestly).
    Learned(Vec<LearnedName>),
    /// Nothing known — the edge renders the key itself.
    Unknown,
}

/// One name the learned store supports, with its provenance.
pub struct LearnedName {
    pub name: String,
    /// The *self-claim's* supersession counter (SPEC §3.2) — orders
    /// conflicting names across answers; 0 for endorsed-only names
    /// (endorsement revisions are the voucher's own counter, a different
    /// scope, never mixed in).
    pub revision: u64,
    /// Petnames of the contacts serving a record with this claim.
    pub held_by: Vec<String>,
    /// The subject itself served a record claiming this name.
    pub confirmed_by_subject: bool,
    /// Petnames of the contacts who *vouch* this name — their own signed
    /// claim, not the subject's (D4a: "your friends call them…").
    pub endorsed_by: Vec<String>,
}

/// One contact's verified link evidence for an unknown key (D3c,
/// multi-device.md §7): the popup's "P says this is their device" line —
/// an offer's provenance, never an instruction.
pub struct DeviceEvidence {
    /// Whose device the evidence says it is.
    pub petname: String,
    /// Vouched-from-trust, or mutually confirmed (the upgrade).
    pub tier: zink_protocol::LinkTier,
}

/// One valid `Negative` about a key, with the observer's verdict (D4b).
pub struct Disavowal {
    pub attester: PublicKey,
    /// The attester rendered (petname / device name / short hex).
    pub attester_label: String,
    /// Whether the MVP policy excludes the key from addressed sets: true
    /// only for this client's own stance or a same-person disavowal;
    /// third-party negatives warn, never exclude.
    pub excludes: bool,
}

impl<C: Clock, W: WallClock, N: Transport, R: Draw> Client<C, W, N, R> {
    /// Store a scanned/pasted record. The petname defaults to the contact's
    /// self-claimed name; the caller may override (petnames are ours, not
    /// theirs). Returns the petname it was stored under.
    ///
    /// **Contact identity is key overlap** (multi-device.md §4): a record
    /// sharing any key with an existing contact is an update *of that
    /// entry* — accepted only under that entry's own petname, which is the
    /// explicit confirm. A `keys` list is unauthenticated per-key, so a
    /// hostile record smuggling a contact's key must never rewrite that
    /// contact's trust anchor as a side effect of adding "someone new";
    /// a record overlapping two or more contacts is refused outright.
    pub fn add_contact(
        &self,
        record: &ContactRecord,
        petname: Option<String>,
    ) -> Result<String, Error> {
        if record.keys.is_empty() {
            return Err(Error::InvalidRecord("record has no keys".into()));
        }
        if record.relays.is_empty() {
            return Err(Error::InvalidRecord(
                "record has no relays — no way to reach them".into(),
            ));
        }
        let petname = petname
            .or_else(|| record.self_claimed_name().map(str::to_string))
            .ok_or_else(|| {
                Error::InvalidRecord(
                    "record has no valid self-claimed name; provide a petname".into(),
                )
            })?;
        let contacts = self.state.contacts()?;
        match overlapping(&contacts, record).as_slice() {
            // A brand-new person; the petname must still resolve to one
            // person (send-by-name stays unambiguous).
            [] => {
                if contacts.iter().any(|(name, _)| *name == petname) {
                    return Err(Error::PetnameCollision(petname));
                }
                self.state.save_contact(&petname, record)?;
            }
            [(existing_name, existing)] => {
                if *existing_name != petname {
                    return Err(Error::ContactOverlap {
                        existing: existing_name.clone(),
                    });
                }
                self.state.replace_contact(existing, &petname, record)?;
            }
            several => {
                let names: Vec<&str> = several.iter().map(|(name, _)| name.as_str()).collect();
                return Err(Error::AmbiguousOverlap(names.join(", ")));
            }
        }
        Ok(petname)
    }

    /// Triage a scanned/pasted record against the store: a brand-new
    /// person, an update of exactly one contact (with the diff a confirm
    /// renders), or an ambiguous span that storing would refuse. Read-only
    /// — this is what lets an edge put the explicit update confirm in
    /// front of a human instead of asking them to retype a petname (R1).
    pub fn preview_contact(&self, record: &ContactRecord) -> Result<RecordMatch, Error> {
        let contacts = self.state.contacts()?;
        Ok(match overlapping(&contacts, record).as_slice() {
            [] => RecordMatch::New {
                suggested_petname: record.self_claimed_name().map(str::to_string),
            },
            [(petname, existing)] => {
                RecordMatch::Update(RecordUpdate::diff(petname, existing, record))
            }
            several => RecordMatch::Ambiguous {
                petnames: several.iter().map(|(name, _)| name.clone()).collect(),
            },
        })
    }

    /// The explicit update act (R1): replace the record of the one contact
    /// this record shares a key with, keeping the stored petname — an
    /// update never renames. The caller has confirmed against
    /// `preview_contact`; a record matching nothing belongs to
    /// `add_contact`, and one spanning two contacts is refused outright,
    /// same as everywhere (multi-device.md §4).
    pub fn update_contact(&self, record: &ContactRecord) -> Result<String, Error> {
        if record.keys.is_empty() {
            return Err(Error::InvalidRecord("record has no keys".into()));
        }
        if record.relays.is_empty() {
            return Err(Error::InvalidRecord(
                "record has no relays — no way to reach them".into(),
            ));
        }
        let contacts = self.state.contacts()?;
        match overlapping(&contacts, record).as_slice() {
            [] => Err(Error::NotAContact(
                "record shares no key with a stored contact — add it instead".into(),
            )),
            [(petname, existing)] => {
                self.state.replace_contact(existing, petname, record)?;
                Ok(petname.clone())
            }
            several => {
                let names: Vec<&str> = several.iter().map(|(name, _)| name.as_str()).collect();
                Err(Error::AmbiguousOverlap(names.join(", ")))
            }
        }
    }

    /// Rename a contact — set *my* petname for them (my lens, U4). Purely
    /// local: the petname is a key-stemmed sibling file, so this rewrites it
    /// in place; nothing is published (sharing a name with friends is the
    /// explicit `vouch`). Rejects an empty name or a collision with another
    /// contact, so send-by-name stays unambiguous.
    pub fn rename(&self, current: &str, new: &str) -> Result<(), Error> {
        let new = new.trim();
        if new.is_empty() {
            return Err(Error::InvalidInput("petname cannot be empty".into()));
        }
        if new == current {
            return Ok(());
        }
        let contacts = self.state.contacts()?;
        let record = contacts
            .iter()
            .find(|(name, _)| name == current)
            .map(|(_, record)| record.clone())
            .ok_or_else(|| Error::NotAContact(format!("no contact named {current:?}")))?;
        if contacts.iter().any(|(name, _)| name == new) {
            return Err(Error::PetnameCollision(new.to_string()));
        }
        self.state.save_contact(new, &record)
    }

    /// All stored contacts as `(petname, record)`.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, Error> {
        self.state.contacts()
    }

    /// The one-way "recognize this device as me" act (multi-device.md §3),
    /// called by the edge *after* its fingerprint confirm: store the
    /// scanned record in the own-devices store and sign the link vouch
    /// that `my_record` carries from now on. One direction only — the
    /// shown side does nothing, and serving/inclusion move only from this
    /// device toward the recognized key. Returns that key.
    pub fn recognize_device(&self, record: &ContactRecord) -> Result<PublicKey, Error> {
        let device_key = *record
            .keys
            .first()
            .ok_or_else(|| Error::InvalidRecord("record has no keys".into()))?;
        if device_key == self.device.public() {
            return Err(Error::InvalidInput(
                "that is this device's own record".into(),
            ));
        }
        if record.relays.is_empty() {
            return Err(Error::InvalidRecord(
                "record has no relays — send-to-self deposits need a mailbox".into(),
            ));
        }
        // Revision 0 is right: supersession scopes per linked key
        // (SPEC §3.2), so the first link per device never contends and a
        // re-recognize re-signs the identical attestation. Withdrawal is
        // the deferred `Negative` flow (D4).
        let vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject: self.device.public(),
                claim: Claim::SamePersonAs(device_key),
                revision: 0,
            },
            &self.device,
        );
        self.state.save_recognized_device(record, &vouch)?;
        Ok(device_key)
    }

    /// Recognized own devices as `(device key, record)` — this device's
    /// recognition set, its own social-graph decision like everything else.
    pub fn recognized_devices(&self) -> Vec<(PublicKey, ContactRecord)> {
        self.state.recognized_devices()
    }

    /// Vouch for a contact (D4a, web-of-trust.md §2): sign "I call this
    /// key <petname>" and serve it as an endorsement with every `WhoIs`
    /// answer about them from now on. **Explicit** — it broadcasts your
    /// petname, which stays private by default (SPEC §3.2); nothing
    /// vouches on add. A re-vouch (after a rename, say) supersedes at the
    /// next revision. Returns the vouched key.
    pub fn vouch(&self, petname: &str) -> Result<PublicKey, Error> {
        let subject = self.contact_key(petname)?;
        let revision = self
            .state
            .vouch_for(&subject)
            .map(|prior| prior.attestation.revision + 1)
            .unwrap_or(0);
        let vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject,
                claim: Claim::Name(petname.to_string()),
                revision,
            },
            &self.device,
        );
        self.state.save_vouch(&subject, &vouch)?;
        Ok(subject)
    }

    /// Withdraw a vouch: it stops being served, and observers'
    /// per-responder learned entries replace it away on their next
    /// freshness pull. The *active* disavowal (`Negative`) is D4b.
    pub fn unvouch(&self, petname: &str) -> Result<(), Error> {
        let subject = self.contact_key(petname)?;
        self.state.remove_vouch(&subject);
        Ok(())
    }

    /// Un-recognize a device, locally only (web-of-trust.md §6): it stops
    /// being served, included, and re-wrapped — but nothing is published.
    /// Losing interest in a sibling is not the same as declaring it
    /// compromised; that is `repudiate`.
    pub fn unrecognize_device(&self, key: &PublicKey) {
        self.state.remove_recognized_device(key);
    }

    /// Repudiate a key (web-of-trust.md §4/§5): sign the `Negative` that
    /// voids our earlier claims about it, publish it (record +
    /// endorsements), and un-recognize a repudiated sibling. Advisory
    /// like every claim — observers weigh it by their own policy, and a
    /// yet-higher re-vouch restores.
    pub fn repudiate(&self, key: PublicKey) -> Result<(), Error> {
        if key == self.device.public() {
            return Err(Error::InvalidInput("that is this device's own key".into()));
        }
        let stance = self
            .state
            .vouch_for(&key)
            .map(|prior| prior.attestation.revision);
        let device_link = self
            .state
            .recognized_devices()
            .iter()
            .find(|(device_key, _)| *device_key == key)
            .and_then(|_| {
                self.state.device_vouches().into_iter().find(|vouch| {
                matches!(vouch.attestation.claim, Claim::SamePersonAs(linked) if linked == key)
            })
            })
            .map(|vouch| vouch.attestation.revision);
        let revision = match stance.into_iter().chain(device_link).max() {
            Some(highest) => highest + 1,
            None => 0,
        };
        let negative = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: self.device.public(),
                subject: key,
                claim: Claim::Negative,
                revision,
            },
            &self.device,
        );
        self.state.save_vouch(&key, &negative)?;
        self.state.remove_recognized_device(&key);
        Ok(())
    }

    /// Valid disavowals of a key across everything held, each saying WHO
    /// and whether it `excludes` the key from addressed sets — true only
    /// for our own stance or a same-person disavowal; third-party
    /// negatives warn, never exclude (web-of-trust.md §4).
    pub fn disavowals(&self, key: PublicKey) -> Result<Vec<Disavowal>, Error> {
        let contacts = self.state.contacts()?;
        let attestations = self.held_attestations(key)?;
        let same_entry = |attester: &PublicKey| {
            contacts
                .iter()
                .any(|(_, record)| record.keys.contains(attester) && record.keys.contains(&key))
        };
        // No voiding here, deliberately: a voided link no longer clusters,
        // but it still proves the keys were one person — which is what
        // makes a disavowal "their own" (web-of-trust.md §4).
        let linked = |attester: &PublicKey| {
            attestations.iter().any(|signed| {
                let attestation = &signed.attestation;
                let Claim::SamePersonAs(to) = attestation.claim else {
                    return false;
                };
                attestation.attester == attestation.subject
                    && signed.verify().is_ok()
                    && ((attestation.attester == *attester && to == key)
                        || (attestation.attester == key && to == *attester))
            })
        };
        let own = self.own_keys();
        let mut disavowals: Vec<Disavowal> = Vec::new();
        for signed in &attestations {
            let Some((attester, disavowed, _)) = zink_protocol::verified_negative(signed) else {
                continue;
            };
            if disavowed != key || disavowals.iter().any(|d| d.attester == attester) {
                continue;
            }
            let excludes = own.contains(&attester) || same_entry(&attester) || linked(&attester);
            disavowals.push(Disavowal {
                attester,
                attester_label: self
                    .participant_labels(&[attester])?
                    .pop()
                    .unwrap_or_default(),
                excludes,
            });
        }
        Ok(disavowals)
    }

    /// Every attestation this client holds that could bear on `key`: its
    /// own stances, stored contact records, and the learned records +
    /// endorsements for the key and for each contact's keys.
    fn held_attestations(&self, key: PublicKey) -> Result<Vec<SignedAttestation>, Error> {
        let mut attestations: Vec<SignedAttestation> = Vec::new();
        attestations.extend(self.state.vouch_for(&key));
        attestations.extend(self.state.issued_negatives());
        for entry in self.state.learned(&key) {
            attestations.extend(entry.record.attestations.clone());
            attestations.extend(entry.endorsements.clone());
        }
        for (_, record) in self.state.contacts()? {
            for contact_key in &record.keys {
                for entry in self.state.learned(contact_key) {
                    attestations.extend(entry.record.attestations.clone());
                    attestations.extend(entry.endorsements.clone());
                }
            }
            attestations.extend(record.attestations);
        }
        Ok(attestations)
    }

    /// Whether this device currently vouches for a key (edge rendering).
    pub fn vouches(&self, subject: &PublicKey) -> bool {
        self.state.vouch_for(subject).is_some()
    }

    /// A contact entry's identity key (its record's first key).
    fn contact_key(&self, petname: &str) -> Result<PublicKey, Error> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .and_then(|(_, record)| record.keys.first().copied())
            .ok_or_else(|| Error::NotAContact(format!("no contact named {petname:?}")))
    }

    /// This device's key cluster as its own client sees it: self plus the
    /// recognized devices (D3c). Edges filter "other participants" with
    /// this — a conversation with a contact is not "with mårten laptop".
    pub fn own_keys(&self) -> BTreeSet<PublicKey> {
        std::iter::once(self.device.public())
            .chain(self.state.recognized_devices().into_iter().map(|(k, _)| k))
            .collect()
    }

    /// The stored record for a key this client trusts: a user-added
    /// contact's, else a recognized own device's (D3c — devices resolve
    /// routes and labels through their own store, never through contacts).
    pub(super) fn trusted_record_for(&self, key: &PublicKey) -> Option<ContactRecord> {
        self.state
            .contacts()
            .unwrap_or_default()
            .into_iter()
            .find(|(_, record)| record.keys.contains(key))
            .map(|(_, record)| record)
            .or_else(|| {
                self.state
                    .recognized_devices()
                    .into_iter()
                    .find(|(device_key, _)| device_key == key)
                    .map(|(_, record)| record)
            })
    }

    /// Petname → the Contact to send to. Keys come from the user-added
    /// record alone; relays resolve at read time (D1b, who-is-this.md §7).
    pub fn resolve_contact(&self, petname: &str) -> Result<Contact, Error> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .map(|(_, record)| self.contact_from(&record))
            .ok_or_else(|| Error::NotAContact(format!("no contact named {petname:?}")))
    }

    /// Keys from the stored record; relays resolved at read time (§7).
    fn contact_from(&self, record: &ContactRecord) -> Contact {
        let relays = match record.keys.first() {
            Some(&key) => self.effective_relays(key, Some(record)),
            None => record.relays.clone(),
        };
        Contact {
            keys: record.keys.clone(),
            relays: relays.into_iter().map(|entry| entry.mailbox).collect(),
        }
    }

    /// The relay entries to reach a person at, resolved at read time
    /// (who-is-this.md §7) — nothing stored is ever mutated. Provenance
    /// classes, first non-empty class wins, latest receipt within one:
    /// **subject-served** (authenticated by the connection key) > the
    /// **user-added record** (authenticated by the scan / explicit add) >
    /// **contact-served** hearsay (only ever decisive in the one-way-add
    /// bootstrap, where it's the whole point). Keys never come from
    /// learned records — sealing stays on the user-added record until D3.
    pub(super) fn effective_relays(
        &self,
        key: PublicKey,
        stored: Option<&ContactRecord>,
    ) -> Vec<RelayEntry> {
        let learned = self.state.learned(&key);
        let best = |from_subject: bool| {
            learned
                .iter()
                .filter(|entry| (entry.responder == key) == from_subject)
                .filter(|entry| !entry.record.relays.is_empty())
                .max_by_key(|entry| entry.received_ms)
                .map(|entry| entry.record.relays.clone())
        };
        best(true)
            .or_else(|| {
                stored
                    .filter(|record| !record.relays.is_empty())
                    .map(|record| record.relays.clone())
            })
            .or_else(|| best(false))
            .unwrap_or_default()
    }

    /// The dialable peer address for a person: their key, routed via the
    /// relay URLs their records resolve to at read time.
    pub(super) fn peer_addr_for(
        &self,
        key: PublicKey,
        stored: Option<&ContactRecord>,
    ) -> Result<Peer, Error> {
        let relay_urls: Vec<String> = self
            .effective_relays(key, stored)
            .iter()
            .filter_map(|entry| entry.relay_url.as_deref().map(str::to_string))
            .collect();
        if relay_urls.is_empty() {
            return Err(Error::NoRelayUrl);
        }
        crate::adapters::iroh::validated_peer(key, relay_urls)
    }

    /// Resolve a key to the best-believed name (who-is-this.md §6):
    /// petname (manual, always wins) > learned self-claims (grouped by
    /// name, highest revision first — a genuine tie surfaces both, never
    /// arbitrated) > unknown (the edge renders the key). Provenance rides
    /// along: which contacts hold a record claiming each name, and whether
    /// the subject itself served one.
    pub fn resolve_name(&self, key: PublicKey) -> Result<ResolvedName, Error> {
        if let Some((petname, _)) = self
            .state
            .contacts()?
            .iter()
            .find(|(_, record)| record.keys.contains(&key))
        {
            return Ok(ResolvedName::Petname(petname.clone()));
        }
        let names: Vec<LearnedName> = self
            .learned_candidates(key)?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        if names.is_empty() {
            return Ok(ResolvedName::Unknown);
        }
        Ok(ResolvedName::Learned(names))
    }

    /// Render-ready candidates for a key from everything learned so far:
    /// `resolve_name`'s groups, each paired with the freshest record
    /// claiming that name (highest revision, then latest receipt) — the
    /// promotable payload behind the wild-key popup's add button (D2c,
    /// groups.md §5). Best first; a genuine tie surfaces both.
    pub fn learned_candidates(
        &self,
        subject: PublicKey,
    ) -> Result<Vec<(LearnedName, ContactRecord)>, Error> {
        let contacts = self.state.contacts()?;
        let petname_of = |responder: PublicKey| {
            contacts
                .iter()
                .find(|(_, record)| record.keys.contains(&responder))
                .map(|(petname, _)| petname.clone())
                // A learned entry can outlive its responder's contact
                // status; fall back to an honest key prefix.
                .unwrap_or_else(|| hex::encode(&responder.0[..4]))
        };
        let mut groups: BTreeMap<String, (LearnedName, ContactRecord, (u64, u64))> =
            BTreeMap::new();
        let entries = self.state.learned(&subject);
        for entry in &entries {
            let Some((name, revision)) = entry.record.self_name_claim() else {
                continue; // no verifiable self-claim — relays-only evidence
            };
            let name = name.to_string();
            let rank = (revision, entry.received_ms);
            let group = groups.entry(name.clone()).or_insert_with(|| {
                (
                    LearnedName {
                        name,
                        revision,
                        held_by: Vec::new(),
                        confirmed_by_subject: false,
                        endorsed_by: Vec::new(),
                    },
                    entry.record.clone(),
                    rank,
                )
            });
            group.0.revision = group.0.revision.max(revision);
            if entry.responder == subject {
                group.0.confirmed_by_subject = true;
            } else {
                group.0.held_by.push(petname_of(entry.responder));
            }
            if rank > group.2 {
                group.1 = entry.record.clone();
                group.2 = rank;
            }
        }
        // Endorsed names (D4a): each responder's own vouch joins its name
        // group — or founds one, paired with that responder's served
        // record as the promotable payload. Endorsement revisions are the
        // voucher's counter (a different supersession scope), so they
        // never mix into the group's self-claim `revision`. The voiding
        // rule applies per voucher (D4b): a name behind the same
        // attester's higher-revision `Negative` is withdrawn, not shown.
        let negatives: Vec<(PublicKey, u64)> = entries
            .iter()
            .flat_map(|entry| entry.endorsements.iter())
            .filter_map(zink_protocol::verified_negative)
            .filter(|(_, disavowed, _)| *disavowed == subject)
            .map(|(attester, _, revision)| (attester, revision))
            .collect();
        for entry in &entries {
            for signed in &entry.endorsements {
                let Claim::Name(name) = &signed.attestation.claim else {
                    continue; // negatives render via `disavowals`, not here
                };
                let voided = negatives.iter().any(|(attester, revision)| {
                    *attester == signed.attestation.attester
                        && *revision > signed.attestation.revision
                });
                if voided || signed.verify().is_err() {
                    continue;
                }
                let name = name.clone();
                let rank = (0, entry.received_ms);
                let group = groups.entry(name.clone()).or_insert_with(|| {
                    (
                        LearnedName {
                            name,
                            revision: 0,
                            held_by: Vec::new(),
                            confirmed_by_subject: false,
                            endorsed_by: Vec::new(),
                        },
                        entry.record.clone(),
                        rank,
                    )
                });
                group.0.endorsed_by.push(petname_of(entry.responder));
            }
        }
        let mut candidates: Vec<(LearnedName, ContactRecord)> = groups
            .into_values()
            .map(|(name, record, _)| (name, record))
            .collect();
        // Ranking (web-of-trust.md §2): names with verified self-claim
        // evidence outrank endorsed-only ones; then self-claim revision,
        // then agreement, then name — a deterministic *default lens*.
        candidates.sort_by(|a, b| {
            let self_claimed =
                |name: &LearnedName| name.confirmed_by_subject || !name.held_by.is_empty();
            let agreement = |name: &LearnedName| name.held_by.len() + name.endorsed_by.len();
            self_claimed(&b.0)
                .cmp(&self_claimed(&a.0))
                .then_with(|| b.0.revision.cmp(&a.0.revision))
                .then_with(|| agreement(&b.0).cmp(&agreement(&a.0)))
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        Ok(candidates)
    }

    /// Link evidence for an unknown key across everything this client
    /// holds — stored contact records plus all learned records for the
    /// subject and for each contact's keys (multi-device.md §7): per
    /// contact whose keys verifiably vouch the subject, the evidence tier.
    /// Strongest first; several contacts claiming the same key all
    /// surface, honestly — the §8 misattribution case is exactly why the
    /// popup says *who* claims, and why nothing here auto-adopts: every
    /// tier only ever produces an offer, accepted via `add_contact`.
    pub fn device_evidence(&self, subject: PublicKey) -> Result<Vec<DeviceEvidence>, Error> {
        let contacts = self.state.contacts()?;
        // Links AND negatives now travel as endorsements too, so the pool
        // is everything held (D4b) — `link_tier` applies the voiding rule.
        let attestations = self.held_attestations(subject)?;
        let mut evidence: Vec<DeviceEvidence> = contacts
            .iter()
            .filter_map(|(petname, record)| {
                match zink_protocol::link_tier(&record.keys, subject, &attestations) {
                    zink_protocol::LinkTier::None => None,
                    tier => Some(DeviceEvidence {
                        petname: petname.clone(),
                        tier,
                    }),
                }
            })
            .collect();
        evidence.sort_by(|a, b| b.tier.cmp(&a.tier).then_with(|| a.petname.cmp(&b.petname)));
        Ok(evidence)
    }

    /// Ignore an unknown key (D2c, groups.md §5): the popup stops
    /// proposing it; the key keeps rendering as hex (honest), and the
    /// manual who-is path stays available. Local presentation policy.
    pub fn dismiss(&self, key: PublicKey) -> Result<(), Error> {
        self.state.dismiss_key(&key)
    }

    pub fn dismissed(&self) -> BTreeSet<PublicKey> {
        self.state.dismissed_keys()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::client::test_kit::{
        befriend, chain, dir_bytes, loop_client, mailbox_only, signed_record, temp_key, temp_root,
    };
    use crate::ports::transport::Loopback;
    use zink_protocol::DeviceKey;

    #[tokio::test]
    async fn resolve_contact__should_take_keys_from_the_stored_record_only() {
        // Given: carol stored with relay X; a subject-served learned record
        // with relay Y and a smuggled extra key; *newer* contact-served
        // hearsay with relay Z
        let a = Client::open_or_create(&temp_key("keys", "asker"))
            .await
            .expect("open A");
        let carol = DeviceKey::from_seed([22; 32]);
        let extra = DeviceKey::from_seed([23; 32]).public();
        let stored = ContactRecord::new(
            vec![carol.public()],
            vec![],
            vec![RelayEntry {
                mailbox: "xx@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        a.add_contact(&stored, Some("carol".to_string()))
            .expect("add");
        let served = ContactRecord::new(
            vec![carol.public(), extra],
            vec![],
            vec![RelayEntry {
                mailbox: "yy@203.0.113.2:2".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(&carol.public(), &carol.public(), &served, &[], 1)
            .expect("learn subject-served");
        let hearsay = ContactRecord::new(
            vec![carol.public()],
            vec![],
            vec![RelayEntry {
                mailbox: "zz@203.0.113.3:3".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([24; 32]).public(),
                &hearsay,
                &[],
                2,
            )
            .expect("learn hearsay");

        // When
        let contact = a.resolve_contact("carol").expect("resolve");

        // Then: subject-served relays beat newer hearsay; sealing keys come
        // strictly from the user-added record — the smuggled key is inert
        assert_eq!(contact.relays, vec!["yy@203.0.113.2:2".to_string()]);
        assert_eq!(contact.keys, vec![carol.public()]);

        let _ = std::fs::remove_dir_all(temp_root("keys"));
    }

    #[tokio::test]
    async fn add_contact__should_update_the_overlapping_contact_under_its_own_petname() {
        // Given: bob stored under his original single-key record
        let a = Client::open_or_create(&temp_key("overlap-update", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([31; 32]);
        let laptop = DeviceKey::from_seed([32; 32]);
        let original =
            ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1"));
        a.add_contact(&original, Some("bob".to_string()))
            .expect("add");

        // When: a re-scan with the key set extended and *reordered* — a new
        // first key, so the store stem must re-derive without forking
        let rescanned = ContactRecord::new(
            vec![laptop.public(), bob.public()],
            vec![],
            mailbox_only("bb@203.0.113.2:2"),
        );
        a.add_contact(&rescanned, Some("bob".to_string()))
            .expect("update");

        // Then: still exactly one contact, holding the fresh record
        let contacts = a.contacts().expect("contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].0, "bob");
        assert_eq!(contacts[0].1, rescanned);

        let _ = std::fs::remove_dir_all(temp_root("overlap-update"));
    }

    #[tokio::test]
    async fn add_contact__should_surface_an_overlap_under_a_different_petname() {
        // Given: bob stored; a hostile record smuggling bob's key into its
        // own key list (multi-device.md §4 — the trust-anchor hijack)
        let a = Client::open_or_create(&temp_key("overlap-confirm", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([33; 32]);
        let mallory = DeviceKey::from_seed([34; 32]);
        a.add_contact(
            &ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1")),
            Some("bob".to_string()),
        )
        .expect("add bob");
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("overlap-confirm", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);
        let smuggling = ContactRecord::new(
            vec![mallory.public(), bob.public()],
            vec![],
            mailbox_only("mm@203.0.113.6:6"),
        );

        // When: added as "someone new"
        let result = a.add_contact(&smuggling, Some("mallory".to_string()));

        // Then: surfaced, naming the entry it would rewrite; nothing stored
        assert!(matches!(
            result,
            Err(Error::ContactOverlap { ref existing }) if existing == "bob"
        ));
        assert_eq!(dir_bytes(&contacts_dir), before);

        // And: the same add under the matched petname is the explicit
        // confirm — it updates bob's entry, keeping one contact
        a.add_contact(&smuggling, Some("bob".to_string()))
            .expect("confirmed update");
        let contacts = a.contacts().expect("contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].0, "bob");

        let _ = std::fs::remove_dir_all(temp_root("overlap-confirm"));
    }

    #[tokio::test]
    async fn add_contact__should_refuse_a_record_overlapping_two_contacts() {
        // Given: bob and carol stored as distinct contacts
        let a = Client::open_or_create(&temp_key("overlap-ambiguous", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([35; 32]);
        let carol = DeviceKey::from_seed([36; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("overlap-ambiguous", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);
        let spanning = ContactRecord::new(
            vec![bob.public(), carol.public()],
            vec![],
            mailbox_only("xx@203.0.113.7:7"),
        );

        // When / Then: refused under any petname — even a matching one —
        // and the store is untouched
        for petname in ["dana", "bob"] {
            assert!(matches!(
                a.add_contact(&spanning, Some(petname.to_string())),
                Err(Error::AmbiguousOverlap(_))
            ));
        }
        assert_eq!(dir_bytes(&contacts_dir), before);

        let _ = std::fs::remove_dir_all(temp_root("overlap-ambiguous"));
    }

    #[tokio::test]
    async fn add_contact__should_reject_a_petname_collision_without_key_overlap() {
        // Given: bob stored; an unrelated record wanting the same petname
        let a = Client::open_or_create(&temp_key("overlap-collision", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([37; 32]);
        let other = DeviceKey::from_seed([38; 32]);
        a.add_contact(
            &ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1")),
            Some("bob".to_string()),
        )
        .expect("add bob");

        // When / Then: no shared key = no identity evidence — rejected
        assert!(matches!(
            a.add_contact(
                &ContactRecord::new(
                    vec![other.public()],
                    vec![],
                    mailbox_only("oo@203.0.113.2:2"),
                ),
                Some("bob".to_string()),
            ),
            Err(Error::PetnameCollision(_))
        ));

        let _ = std::fs::remove_dir_all(temp_root("overlap-collision"));
    }

    #[tokio::test]
    async fn preview_contact__should_report_a_new_record_with_the_self_claim() {
        // Given: an empty store; a record self-claiming "carol"
        let a = Client::open_or_create(&temp_key("preview-new", "a"))
            .await
            .expect("open");
        let carol = DeviceKey::from_seed([40; 32]);
        let record = signed_record(&carol, "carol", 0, mailbox_only("cc@203.0.113.1:1"));

        // When
        let matched = a.preview_contact(&record).expect("preview");

        // Then
        assert!(matches!(
            matched,
            RecordMatch::New { suggested_petname: Some(ref name) } if name == "carol"
        ));

        let _ = std::fs::remove_dir_all(temp_root("preview-new"));
    }

    #[tokio::test]
    async fn preview_contact__should_diff_an_update_of_one_contact() {
        // Given: anna stored under her original record (self-claim "Anna",
        // one relay); a re-scan with a renamed claim, a replaced relay,
        // and a second device key
        let a = Client::open_or_create(&temp_key("preview-diff", "a"))
            .await
            .expect("open");
        let anna = DeviceKey::from_seed([41; 32]);
        let laptop = DeviceKey::from_seed([42; 32]);
        let stored = signed_record(&anna, "Anna", 0, mailbox_only("old@203.0.113.1:1"));
        a.add_contact(&stored, Some("anna".to_string()))
            .expect("add");
        let renamed = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: anna.public(),
                subject: anna.public(),
                claim: Claim::Name("Ann".to_string()),
                revision: 1,
            },
            &anna,
        );
        let rescanned = ContactRecord::new(
            vec![anna.public(), laptop.public()],
            vec![renamed],
            mailbox_only("new@203.0.113.9:9"),
        );

        // When
        let matched = a.preview_contact(&rescanned).expect("preview");

        // Then: an update of anna's entry, with the full diff
        let RecordMatch::Update(update) = matched else {
            panic!("expected an update match");
        };
        assert_eq!(update.petname, "anna");
        assert_eq!(update.old_name.as_deref(), Some("Anna"));
        assert_eq!(update.new_name.as_deref(), Some("Ann"));
        assert_eq!(update.relays_added, vec!["new@203.0.113.9:9".to_string()]);
        assert_eq!(update.relays_removed, vec!["old@203.0.113.1:1".to_string()]);
        assert_eq!(update.keys_added, 1);
        assert_eq!(update.keys_removed, 0);

        let _ = std::fs::remove_dir_all(temp_root("preview-diff"));
    }

    #[tokio::test]
    async fn preview_contact__should_report_a_record_spanning_contacts() {
        // Given: bob and carol stored; a record carrying both their keys
        let a = Client::open_or_create(&temp_key("preview-span", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([43; 32]);
        let carol = DeviceKey::from_seed([44; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let spanning = ContactRecord::new(
            vec![bob.public(), carol.public()],
            vec![],
            mailbox_only("xx@203.0.113.7:7"),
        );

        // When
        let matched = a.preview_contact(&spanning).expect("preview");

        // Then
        assert!(matches!(
            matched,
            RecordMatch::Ambiguous { ref petnames } if *petnames == ["bob", "carol"]
        ));

        let _ = std::fs::remove_dir_all(temp_root("preview-span"));
    }

    #[tokio::test]
    async fn update_contact__should_replace_the_record_and_keep_the_petname() {
        // Given: anna stored, then renamed on her side and re-homed to a
        // new relay — the B1 migration regression (5-relay-lifecycle §3)
        let a = Client::open_or_create(&temp_key("update-keeps", "a"))
            .await
            .expect("open");
        let anna = DeviceKey::from_seed([45; 32]);
        let stored = signed_record(&anna, "Anna", 0, mailbox_only("old@203.0.113.1:1"));
        a.add_contact(&stored, Some("anna".to_string()))
            .expect("add");
        let rescanned = signed_record(&anna, "Ann", 1, mailbox_only("new@203.0.113.9:9"));

        // When: the confirmed update — no petname anywhere in sight
        let petname = a.update_contact(&rescanned).expect("update");

        // Then: same entry, my label kept, her fresh record the new anchor
        assert_eq!(petname, "anna");
        let contacts = a.contacts().expect("contacts");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].0, "anna");
        assert_eq!(contacts[0].1, rescanned);
        let contact = a.resolve_contact("anna").expect("resolve");
        assert_eq!(contact.relays, vec!["new@203.0.113.9:9".to_string()]);

        let _ = std::fs::remove_dir_all(temp_root("update-keeps"));
    }

    #[tokio::test]
    async fn update_contact__should_refuse_a_record_matching_no_contact() {
        // Given: bob stored; a record sharing none of his keys
        let a = Client::open_or_create(&temp_key("update-none", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([46; 32]);
        let stranger = DeviceKey::from_seed([47; 32]);
        a.add_contact(
            &ContactRecord::new(vec![bob.public()], vec![], mailbox_only("bb@203.0.113.1:1")),
            Some("bob".to_string()),
        )
        .expect("add");
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("update-none", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);

        // When / Then: refused — updating what isn't stored is an add
        assert!(matches!(
            a.update_contact(&ContactRecord::new(
                vec![stranger.public()],
                vec![],
                mailbox_only("ss@203.0.113.2:2"),
            )),
            Err(Error::NotAContact(_))
        ));
        assert_eq!(dir_bytes(&contacts_dir), before);

        let _ = std::fs::remove_dir_all(temp_root("update-none"));
    }

    #[tokio::test]
    async fn update_contact__should_refuse_a_record_spanning_contacts() {
        // Given: bob and carol stored; a record carrying both their keys
        let a = Client::open_or_create(&temp_key("update-span", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([48; 32]);
        let carol = DeviceKey::from_seed([49; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let contacts_dir =
            std::path::PathBuf::from(format!("{}.state", temp_key("update-span", "a")))
                .join("contacts");
        let before = dir_bytes(&contacts_dir);

        // When / Then: merging is never silent — refused, store untouched
        assert!(matches!(
            a.update_contact(&ContactRecord::new(
                vec![bob.public(), carol.public()],
                vec![],
                mailbox_only("xx@203.0.113.7:7"),
            )),
            Err(Error::AmbiguousOverlap(_))
        ));
        assert_eq!(dir_bytes(&contacts_dir), before);

        let _ = std::fs::remove_dir_all(temp_root("update-span"));
    }

    #[tokio::test]
    async fn recognize__should_serve_the_recognized_device_like_self_one_way() {
        // Given: A holds a full conversation, B only its tip — and the
        // mirror image for the reverse direction. Neither is the other's
        // contact; recognition is the only thing that will open the gate.
        let a = Client::open_or_create(&temp_key("recognize-gate", "a"))
            .await
            .expect("open A");
        let b = Client::open_or_create(&temp_key("recognize-gate", "b"))
            .await
            .expect("open B");
        let held_by_a = chain(&DeviceKey::from_seed([42; 32]), a.public_key(), 3);
        let conv_a = held_by_a[0].id();
        for envelope in &held_by_a {
            a.state.store_envelope(conv_a, envelope).unwrap();
        }
        b.state
            .store_envelope(conv_a, held_by_a.last().unwrap())
            .unwrap();
        let held_by_b = chain(&DeviceKey::from_seed([43; 32]), b.public_key(), 3);
        let conv_b = held_by_b[0].id();
        for envelope in &held_by_b {
            b.state.store_envelope(conv_b, envelope).unwrap();
        }
        a.state
            .store_envelope(conv_b, held_by_b.last().unwrap())
            .unwrap();

        // When: B pulls as a stranger
        let refused = b
            .backfill_addr(conv_a, a.transport.peer())
            .await
            .expect("declined, not an error");

        // Then
        assert_eq!(refused, 0, "unrecognized and no contact — nothing served");

        // When: A recognizes B — one signed act, the shown side passive
        a.recognize_device(&ContactRecord::new(
            vec![b.public_key()],
            vec![],
            mailbox_only("bb@203.0.113.5:5"),
        ))
        .expect("recognize");
        let served = b
            .backfill_addr(conv_a, a.transport.peer())
            .await
            .expect("served");

        // Then: B is served like self…
        assert_eq!(served, 2, "genesis + the middle message");
        assert!(b.state.load_dag(conv_a).is_ok());

        // …while the reverse direction stays closed
        let reverse = a
            .backfill_addr(conv_b, b.transport.peer())
            .await
            .expect("declined");
        assert_eq!(reverse, 0, "recognition moved nothing the other way");

        // When: B recognizes A back (the usual two-way pairing)
        b.recognize_device(&ContactRecord::new(
            vec![a.public_key()],
            vec![],
            mailbox_only("aa@203.0.113.6:6"),
        ))
        .expect("recognize back");
        let reverse = a
            .backfill_addr(conv_b, b.transport.peer())
            .await
            .expect("served");

        // Then
        assert_eq!(reverse, 2);

        let _ = std::fs::remove_dir_all(temp_root("recognize-gate"));
    }

    #[tokio::test]
    async fn recognize__should_put_the_vouch_in_my_record_and_nowhere_else() {
        // Given: a profiled phone and its laptop's record
        let a = Client::open_or_create(&temp_key("recognize-vouch", "a"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("mårten phone", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let laptop = DeviceKey::from_seed([45; 32]);
        let laptop_record = signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        );

        // When
        a.recognize_device(&laptop_record).expect("recognize");

        // Then: my_record vouches the laptop — an observer trusting A's
        // key tiers the laptop as offerable (the D3a evaluation)…
        let my = a.my_record().expect("record");
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &my.attestations),
            zink_protocol::LinkTier::VouchedFromTrust
        );
        // …while the laptop's record carries only its own (zero) vouches
        assert_eq!(
            zink_protocol::link_tier(
                &[laptop.public()],
                a.public_key(),
                &laptop_record.attestations
            ),
            zink_protocol::LinkTier::None
        );

        // And: once the laptop runs its own act back, an observer holding
        // both records sees the upgrade — aggregation across records is
        // exactly the observer's job (multi-device.md §4)
        let laptop_vouch = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: laptop.public(),
                subject: laptop.public(),
                claim: Claim::SamePersonAs(a.public_key()),
                revision: 0,
            },
            &laptop,
        );
        let mut held = my.attestations.clone();
        held.push(laptop_vouch);
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &held),
            zink_protocol::LinkTier::MutuallyConfirmed
        );

        // And: the recognition persists across a reopen
        a.close().await;
        let a = Client::open(&temp_key("recognize-vouch", "a"))
            .await
            .expect("reopen");
        assert_eq!(a.recognized_devices().len(), 1);
        assert_eq!(
            zink_protocol::link_tier(
                &[a.public_key()],
                laptop.public(),
                &a.my_record().expect("record").attestations
            ),
            zink_protocol::LinkTier::VouchedFromTrust
        );

        let _ = std::fs::remove_dir_all(temp_root("recognize-vouch"));
    }

    #[tokio::test]
    async fn device_evidence__should_tier_from_held_records() {
        // Given: P is a contact whose record vouches an unknown key
        let a = Client::open_or_create(&temp_key("evidence", "a"))
            .await
            .expect("open");
        let p = DeviceKey::from_seed([52; 32]);
        let laptop = DeviceKey::from_seed([53; 32]);
        let vouch = |attester: &DeviceKey, linked: PublicKey| {
            SignedAttestation::new(
                Attestation {
                    version: Attestation::CURRENT,
                    attester: attester.public(),
                    subject: attester.public(),
                    claim: Claim::SamePersonAs(linked),
                    revision: 0,
                },
                attester,
            )
        };
        let p_record = ContactRecord::new(
            vec![p.public()],
            vec![vouch(&p, laptop.public())],
            mailbox_only("pp@203.0.113.1:1"),
        );
        a.add_contact(&p_record, Some("p".to_string()))
            .expect("add");

        // Then: the one-way tier — offerable, labeled as P's claim
        let evidence = a.device_evidence(laptop.public()).expect("evidence");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].petname, "p");
        assert_eq!(evidence[0].tier, zink_protocol::LinkTier::VouchedFromTrust);

        // When: the laptop's own record — learned via the auto-query —
        // carries the reverse vouch
        let laptop_record = ContactRecord::new(
            vec![laptop.public()],
            vec![vouch(&laptop, p.public())],
            mailbox_only("ll@203.0.113.2:2"),
        );
        a.state
            .save_learned(&laptop.public(), &p.public(), &laptop_record, &[], 1)
            .expect("learn");

        // Then: upgraded to mutually confirmed
        assert_eq!(
            a.device_evidence(laptop.public()).expect("evidence")[0].tier,
            zink_protocol::LinkTier::MutuallyConfirmed
        );

        // And: the spoof direction — a stranger's record claiming P's key —
        // is no evidence at all
        let stranger = DeviceKey::from_seed([54; 32]);
        let spoof = ContactRecord::new(
            vec![stranger.public()],
            vec![vouch(&stranger, p.public())],
            mailbox_only("ss@203.0.113.3:3"),
        );
        a.state
            .save_learned(&stranger.public(), &p.public(), &spoof, &[], 2)
            .expect("learn");
        assert!(
            a.device_evidence(stranger.public())
                .expect("evidence")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_root("evidence"));
    }

    #[tokio::test]
    async fn vouch__should_persist_and_supersede_per_revision() {
        // Given
        let a = Client::open_or_create(&temp_key("vouch", "a"))
            .await
            .expect("open");
        let carol = DeviceKey::from_seed([84; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![carol.public()],
                vec![],
                mailbox_only("cc@203.0.113.1:1"),
            ),
            Some("Carrie".to_string()),
        )
        .expect("add");

        // When / Then: the explicit act signs at revision 0; a re-vouch
        // supersedes; withdrawal removes; a non-contact errors
        assert!(!a.vouches(&carol.public()));
        a.vouch("Carrie").expect("vouch");
        let first = a.state.vouch_for(&carol.public()).expect("stored");
        assert_eq!(first.attestation.revision, 0);
        assert_eq!(first.attestation.claim, Claim::Name("Carrie".to_string()));
        assert_eq!(first.verify(), Ok(()));
        a.vouch("Carrie").expect("re-vouch");
        assert_eq!(
            a.state
                .vouch_for(&carol.public())
                .expect("stored")
                .attestation
                .revision,
            1
        );
        a.unvouch("Carrie").expect("unvouch");
        assert!(!a.vouches(&carol.public()));
        assert!(matches!(a.vouch("nobody"), Err(Error::NotAContact(_))));

        let _ = std::fs::remove_dir_all(temp_root("vouch"));
    }

    #[tokio::test]
    async fn repudiate__should_supersede_the_vouch_and_unrecognize_the_sibling() {
        // Given: a profiled phone that vouched a contact and recognized a
        // laptop
        let a = Client::open_or_create(&temp_key("repudiate", "a"))
            .await
            .expect("open");
        let relay = format!("{}@203.0.113.1:1", hex::encode(&a.public_key().0));
        a.set_profile("mårten phone", std::slice::from_ref(&relay))
            .await
            .expect("profile");
        let carol = DeviceKey::from_seed([90; 32]);
        a.add_contact(
            &ContactRecord::new(
                vec![carol.public()],
                vec![],
                mailbox_only("cc@203.0.113.1:1"),
            ),
            Some("Carrie".to_string()),
        )
        .expect("add");
        a.vouch("Carrie").expect("vouch");
        let laptop = DeviceKey::from_seed([91; 32]);
        a.recognize_device(&signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        ))
        .expect("recognize");
        let old_record = a.my_record().expect("record");

        // When: both get repudiated
        a.repudiate(carol.public()).expect("repudiate carol");
        a.repudiate(laptop.public()).expect("repudiate laptop");

        // Then: the negative supersedes the vouch (rev 1 over rev 0), the
        // sibling is un-recognized, and the fresh record publishes both
        let carol_stance = a.state.vouch_for(&carol.public()).expect("stance");
        assert!(matches!(carol_stance.attestation.claim, Claim::Negative));
        assert_eq!(carol_stance.attestation.revision, 1);
        assert!(a.recognized_devices().is_empty());
        let fresh = a.my_record().expect("record");
        assert_eq!(
            fresh
                .attestations
                .iter()
                .filter(|signed| matches!(signed.attestation.claim, Claim::Negative))
                .count(),
            2
        );
        // …an observer combining the OLD record (live link) with the fresh
        // negatives sees the device link voided
        let mut held = old_record.attestations.clone();
        held.extend(fresh.attestations.clone());
        assert_eq!(
            zink_protocol::link_tier(&[a.public_key()], laptop.public(), &held),
            zink_protocol::LinkTier::None
        );
        // …and a yet-higher re-vouch restores the contact's name stance
        a.vouch("Carrie").expect("re-vouch");
        let restored = a.state.vouch_for(&carol.public()).expect("stance");
        assert_eq!(restored.attestation.revision, 2);
        assert!(matches!(restored.attestation.claim, Claim::Name(_)));

        let _ = std::fs::remove_dir_all(temp_root("repudiate"));
    }

    #[tokio::test]
    async fn disavowals__should_exclude_same_person_only_and_void_endorsed_names() {
        // Given: bob's learned endorsements about carol carry a vouch
        // superseded by his own negative — but bob and carol share no
        // entry and no link: a third-party claim
        let a = Client::open_or_create(&temp_key("disavow", "a"))
            .await
            .expect("open");
        let bob = DeviceKey::from_seed([92; 32]);
        let carol = DeviceKey::from_seed([93; 32]);
        for (device, name, mailbox) in [
            (&bob, "bob", "bb@203.0.113.1:1"),
            (&carol, "carol", "cc@203.0.113.2:2"),
        ] {
            a.add_contact(
                &ContactRecord::new(vec![device.public()], vec![], mailbox_only(mailbox)),
                Some(name.to_string()),
            )
            .expect("add");
        }
        let endorse = |claim: Claim, revision: u64| {
            SignedAttestation::new(
                Attestation {
                    version: Attestation::CURRENT,
                    attester: bob.public(),
                    subject: carol.public(),
                    claim,
                    revision,
                },
                &bob,
            )
        };
        let carol_record = signed_record(&carol, "Carol", 0, mailbox_only("cc@203.0.113.2:2"));
        a.state
            .save_learned(
                &carol.public(),
                &bob.public(),
                &carol_record,
                &[
                    endorse(Claim::Name("Caroline".to_string()), 0),
                    endorse(Claim::Negative, 1),
                ],
                1,
            )
            .expect("learn");

        // Then: the endorsed name is voided by its attester's negative…
        let candidates = a.learned_candidates(carol.public()).expect("candidates");
        assert!(
            candidates
                .iter()
                .all(|(name, _)| name.endorsed_by.is_empty()),
            "a name behind the attester's higher negative must not render"
        );
        // …the disavowal renders with WHO — but as third-party it never
        // excludes (the griefing bound, web-of-trust.md §7)
        let disavowals = a.disavowals(carol.public()).expect("disavowals");
        assert_eq!(disavowals.len(), 1);
        assert_eq!(disavowals[0].attester_label, "bob");
        assert!(!disavowals[0].excludes);

        let _ = std::fs::remove_dir_all(temp_root("disavow"));
    }

    #[tokio::test]
    async fn repudiation__should_stop_replies_to_the_lost_device_after_a_pull() {
        // Given: the lost-device drill (web-of-trust.md §5.1). The phone
        // paired a laptop (the laptop's record carries its reverse link —
        // the same-person evidence alice will hold); alice has both as
        // contact entries and a conversation whose membership carries all
        // three keys via the phone's send-to-self.
        let wire = Loopback::new();
        let (phone, _p_net, _p_clock) = loop_client("drill", "phone", &wire);
        let (alice, _a_net, _a_clock) = loop_client("drill", "alice", &wire);
        // The phone's profile — `my_record` reads it (as `open_homed` wrote).
        phone
            .state
            .save_profile(
                "phone",
                &[RelayEntry {
                    mailbox: "unused@203.0.113.1:1".to_string(),
                    relay_url: Some("http://203.0.113.1:1".to_string()),
                }],
            )
            .expect("save profile");
        befriend(&phone.state, alice.public_key()); // alice is served by the phone
        let laptop = DeviceKey::from_seed([94; 32]);
        let laptop_link = SignedAttestation::new(
            Attestation {
                version: Attestation::CURRENT,
                attester: laptop.public(),
                subject: laptop.public(),
                claim: Claim::SamePersonAs(phone.public_key()),
                revision: 0,
            },
            &laptop,
        );
        let mut laptop_record = signed_record(
            &laptop,
            "mårten laptop",
            0,
            mailbox_only("ll@203.0.113.5:5"),
        );
        laptop_record.attestations.push(laptop_link);
        phone.recognize_device(&laptop_record).expect("recognize");
        let to_alice = vec![Contact {
            keys: vec![alice.public_key()],
            relays: vec!["aa@203.0.113.9:9".to_string()],
        }];
        // The relay route is fake — the send queues, but the sealed core
        // (with the laptop appended) is stored; hand it to alice directly.
        let result = phone.send(&to_alice, b"hi".to_vec(), vec![]).await;
        assert!(matches!(result, Err(Error::AllRelaysPending(_))));
        let conversation = phone.state.conversations()[0];
        for envelope in phone.state.load_envelopes(conversation).expect("stored") {
            alice
                .state
                .store_envelope(conversation, &envelope)
                .expect("copy");
        }
        alice
            .add_contact(
                &phone.my_record().expect("record"),
                Some("mårten".to_string()),
            )
            .expect("add phone");
        alice.add_contact(&laptop_record, None).expect("add laptop");

        // Baseline: a reply addresses both of mårten's keys
        let baseline = alice.reply_contacts(conversation).expect("reply");
        assert_eq!(baseline.contacts.len(), 2);
        assert!(baseline.disavowed.is_empty());

        // When: the phone repudiates the lost laptop; alice's next
        // freshness pull on the phone brings the fresh record
        phone.repudiate(laptop.public()).expect("repudiate");
        let outcome = alice.who_is(phone.public_key()).await.expect("pull");
        assert!(!outcome.answers.is_empty());

        // Then: the laptop drops out of the addressed set — the accepted
        // disavowal is the deliberate stop-include — and renders with WHO
        let after = alice.reply_contacts(conversation).expect("reply");
        assert_eq!(after.contacts.len(), 1);
        assert_eq!(after.disavowed, vec![laptop.public()]);
        let disavowals = alice.disavowals(laptop.public()).expect("disavowals");
        assert_eq!(disavowals.len(), 1);
        assert!(disavowals[0].excludes);
        assert_eq!(disavowals[0].attester_label, "mårten");
        // …while the explicit act survives everything: sending to the
        // entry by name is the manual override
        assert!(alice.resolve_contact("mårten laptop").is_ok());

        let _ = std::fs::remove_dir_all(temp_root("drill"));
    }

    #[tokio::test]
    async fn learned_candidates__should_pair_each_name_with_the_freshest_record() {
        // Given: two responders serve the same claimed name, but with
        // different records — the later receipt carries fresher relays
        let a = Client::open_or_create(&temp_key("cands", "asker"))
            .await
            .expect("open");
        let carol = DeviceKey::from_seed([25; 32]);
        let older = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "old@203.0.113.1:1".to_string(),
                relay_url: None,
            }],
        );
        let newer = signed_record(
            &carol,
            "Carol",
            0,
            vec![RelayEntry {
                mailbox: "new@203.0.113.2:2".to_string(),
                relay_url: None,
            }],
        );
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([26; 32]).public(),
                &older,
                &[],
                1,
            )
            .expect("learn older");
        a.state
            .save_learned(
                &carol.public(),
                &DeviceKey::from_seed([27; 32]).public(),
                &newer,
                &[],
                2,
            )
            .expect("learn newer");

        // When
        let candidates = a.learned_candidates(carol.public()).expect("candidates");

        // Then: one group (agreement of two), paired with the freshest
        // record — the promotable payload
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0.name, "Carol");
        assert_eq!(candidates[0].0.held_by.len(), 2);
        assert_eq!(candidates[0].1.relays[0].mailbox, "new@203.0.113.2:2");

        let _ = std::fs::remove_dir_all(temp_root("cands"));
    }

    #[tokio::test]
    async fn dismiss__should_persist_across_reopens() {
        // Given
        let key_path = temp_key("dismiss", "me");
        let a = Client::open_or_create(&key_path).await.expect("open");
        let noisy = DeviceKey::from_seed([28; 32]).public();

        // When
        a.dismiss(noisy).expect("dismiss");
        a.dismiss(noisy).expect("idempotent");
        drop(a);
        let a = Client::open_or_create(&key_path).await.expect("reopen");

        // Then
        assert!(a.dismissed().contains(&noisy));

        let _ = std::fs::remove_dir_all(temp_root("dismiss"));
    }

    #[tokio::test]
    async fn resolve_name__should_rank_by_revision_and_group_agreement() {
        // Given: two responders hold Carol's old name (revision 0), one
        // holds the rename (revision 1) — a rename caught mid-propagation
        let a = Client::open_or_create(&temp_key("names", "asker"))
            .await
            .expect("open A");
        let carol = DeviceKey::from_seed([25; 32]);
        let old = signed_record(&carol, "Carol", 0, vec![]);
        let new = signed_record(&carol, "Caroline", 1, vec![]);
        for (n, record, at) in [(26u8, &old, 1u64), (27, &old, 2), (28, &new, 3)] {
            a.state
                .save_learned(
                    &carol.public(),
                    &DeviceKey::from_seed([n; 32]).public(),
                    record,
                    &[],
                    at,
                )
                .expect("learn");
        }

        // When
        let ResolvedName::Learned(names) = a.resolve_name(carol.public()).expect("resolve") else {
            panic!("expected learned names");
        };

        // Then: the rename ranks first by revision; the superseded name
        // stays surfaced with its two holders — evidence, not arbitration
        assert_eq!(names.len(), 2);
        assert_eq!((names[0].name.as_str(), names[0].revision), ("Caroline", 1));
        assert_eq!(names[1].name, "Carol");
        assert_eq!(names[1].held_by.len(), 2);

        let _ = std::fs::remove_dir_all(temp_root("names"));
    }
}
