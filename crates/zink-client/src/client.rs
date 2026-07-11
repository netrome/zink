//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::Endpoint;
use rand_core::OsRng;
use zink_protocol::{
    Attestation, BlobDraft, BlobHash, Claim, ContactRecord, DeviceKey, FORMAT_VERSION, MailboxOp,
    MailboxResult, MessageDraft, MessageEnvelope, MessageId, OpenError, PublicKey,
    SignedAttestation, distinct_relays,
};

use crate::state::ClientState;
use crate::{blobs, hex, keystore, net};

pub struct Client {
    device: DeviceKey,
    endpoint: Endpoint,
    state: ClientState,
}

impl Client {
    /// Open with an existing key (the CLI path — `keygen` created it).
    pub async fn open(key_path: &str) -> Result<Self, String> {
        Self::with_device(keystore::load(key_path)?, key_path).await
    }

    /// Open, creating the key on first run (the app path).
    pub async fn open_or_create(key_path: &str) -> Result<Self, String> {
        Self::with_device(keystore::load_or_create(key_path)?, key_path).await
    }

    async fn with_device(device: DeviceKey, key_path: &str) -> Result<Self, String> {
        let endpoint = net::bind_endpoint(&device).await?;
        Ok(Self {
            device,
            endpoint,
            state: ClientState::open(key_path),
        })
    }

    pub fn public_key(&self) -> PublicKey {
        self.device.public()
    }

    /// Seal for all recipients, thread into the participant set's
    /// conversation (or start one), deposit once per distinct relay
    /// (idempotent retry), push blobs to each relay's cache.
    pub async fn send(
        &self,
        contacts: &[Contact],
        plaintext: Vec<u8>,
        blob_drafts: Vec<BlobDraft>,
    ) -> Result<SendReceipt, String> {
        if contacts.is_empty() {
            return Err("at least one recipient required".into());
        }
        let recipients: Vec<PublicKey> = contacts.iter().flat_map(|c| c.keys.clone()).collect();
        let participants: BTreeSet<PublicKey> = recipients
            .iter()
            .copied()
            .chain([self.device.public()])
            .collect();
        let existing = self.state.conversation_for(&participants);
        let draft = match existing {
            Some(conversation) => {
                let dag = self.state.load_dag(conversation)?;
                MessageDraft {
                    conversation: Some(conversation),
                    parents: dag.heads(),
                    recipients,
                    seq: dag.next_seq(&self.device.public()),
                    logical: dag.next_logical(),
                    timestamp_ms: now_ms(),
                    plaintext,
                    blobs: blob_drafts,
                }
            }
            None => MessageDraft {
                conversation: None,
                parents: vec![],
                recipients,
                seq: 0,
                logical: 0,
                timestamp_ms: now_ms(),
                plaintext,
                blobs: blob_drafts,
            },
        };
        let seq = draft.seq;
        let sealed = MessageEnvelope::seal(draft, &self.device, &mut OsRng)
            .map_err(|e| format!("seal message: {e}"))?;
        let id = sealed.envelope.id();
        let conversation = existing.unwrap_or(id);
        self.state.store_envelope(conversation, &sealed.envelope)?;
        if existing.is_none() {
            self.state
                .record_conversation(&participants, conversation)?;
        }

        let relays = distinct_relays(contacts.iter().map(|c| c.relays.clone()));
        let staging = blobs::stage(&sealed.blobs).await?;
        for relay in &relays {
            net::deposit_with_retry(&self.endpoint, relay, &sealed.envelope).await?;
            if !sealed.blobs.is_empty() {
                blobs::push_blobs(&self.endpoint, relay, &staging, &sealed.blobs).await?;
            }
        }
        Ok(SendReceipt {
            id,
            conversation,
            seq,
            blob_count: sealed.blobs.len(),
            relay_count: relays.len(),
        })
    }

    /// Drain every relay: register, then fetch page-by-page (the relay caps
    /// each response, so a large mailbox needs several rounds), dedup by
    /// message id, open, and remember what verified; ack each page at its
    /// own cursor so storage is released as we go.
    pub async fn recv(&self, relays: &[String]) -> Result<Vec<Received>, String> {
        let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut received = Vec::new();
        for relay in relays {
            let connection =
                net::connect(&self.endpoint, relay, zink_protocol::MAILBOX_ALPN).await?;
            net::request(&connection, MailboxOp::Register).await?;
            // Cursors are relay-local: page through this relay, acking each
            // page's high-water mark, until a page comes back empty.
            let mut after = 0u64;
            loop {
                let items = match net::request(&connection, MailboxOp::Fetch { after }).await? {
                    MailboxResult::Envelopes { items } => items,
                    other => return Err(format!("unexpected response from {relay}: {other:?}")),
                };
                if items.is_empty() {
                    break;
                }
                let mut page_cursor = after;
                for item in items {
                    page_cursor = page_cursor.max(item.cursor);
                    if item.envelope.version != zink_protocol::FORMAT_VERSION
                        || item.envelope.core.version != zink_protocol::FORMAT_VERSION
                    {
                        // A future protocol version this client can't parse
                        // (SPEC §10: surfaced, never misparsed). Skipped, and
                        // acked with the page so it doesn't wedge the drain.
                        eprintln!("skipping message with unsupported version");
                        continue;
                    }
                    if !seen.insert(item.envelope.id().0) {
                        continue; // already drained via another relay
                    }
                    let body = item.envelope.open(&self.device);
                    if body.is_ok() {
                        self.remember(&item.envelope)?;
                    }
                    received.push(Received {
                        envelope: item.envelope,
                        relay: relay.clone(),
                        body,
                    });
                }
                net::request(&connection, MailboxOp::Ack { up_to: page_cursor }).await?;
                after = page_cursor;
            }
        }
        Ok(received)
    }

    /// Fetch + verify + decrypt one blob referenced by a received message,
    /// from the relay it arrived through.
    pub async fn fetch_blob(
        &self,
        received: &Received,
        hash: &BlobHash,
    ) -> Result<Vec<u8>, String> {
        blobs::fetch_blob(
            &self.endpoint,
            &received.relay,
            &received.envelope,
            &self.device,
            hash,
        )
        .await
    }

    /// Set this device's display name and home relays — what `my_record`
    /// publishes and what `recv` drains by default.
    pub fn set_profile(&self, name: &str, relays: &[String]) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        for relay in relays {
            net::parse_relay(relay)?;
        }
        self.state.save_profile(name.trim(), relays)
    }

    pub fn profile_name(&self) -> Option<String> {
        self.state.profile_name()
    }

    pub fn home_relays(&self) -> Vec<String> {
        self.state.home_relays()
    }

    /// This device's ContactRecord: key, self-attested name, home relays.
    /// The QR/paste payload is `record.to_qr_string()`.
    pub fn my_record(&self) -> Result<ContactRecord, String> {
        let name = self
            .state
            .profile_name()
            .ok_or("set a profile name first")?;
        let relays = self.state.home_relays();
        if relays.is_empty() {
            return Err("set a home relay first".into());
        }
        let me = self.device.public();
        let attestation = SignedAttestation::new(
            Attestation {
                version: FORMAT_VERSION,
                attester: me,
                subject: me,
                claim: Claim::Name(name),
                revision: 0,
            },
            &self.device,
        );
        Ok(ContactRecord::new(vec![me], vec![attestation], relays))
    }

    /// Ensure a mailbox exists on every home relay. Called when publishing
    /// a record: anyone who scans it must be able to deposit immediately —
    /// a record that names a relay where you have no mailbox is a lie.
    pub async fn register_at_home_relays(&self) -> Result<(), String> {
        for relay in self.state.home_relays() {
            let connection =
                net::connect(&self.endpoint, &relay, zink_protocol::MAILBOX_ALPN).await?;
            net::request(&connection, MailboxOp::Register).await?;
        }
        Ok(())
    }

    /// Store a scanned/pasted record. The petname defaults to the contact's
    /// self-claimed name; the caller may override (petnames are ours, not
    /// theirs). Returns the petname it was stored under.
    pub fn add_contact(
        &self,
        record: &ContactRecord,
        petname: Option<String>,
    ) -> Result<String, String> {
        if record.keys.is_empty() {
            return Err("record has no keys".into());
        }
        if record.relays.is_empty() {
            return Err("record has no relays — no way to reach them".into());
        }
        let petname = petname
            .or_else(|| record.self_claimed_name().map(str::to_string))
            .ok_or("record has no valid self-claimed name; provide a petname")?;
        // A petname must resolve to one person: reject collisions with a
        // *different* key; re-adding the same person updates their record.
        for (existing_name, existing) in self.state.contacts()? {
            if existing_name == petname && existing.keys.first() != record.keys.first() {
                return Err(format!("a different contact is already named {petname:?}"));
            }
        }
        self.state.save_contact(&petname, record)?;
        Ok(petname)
    }

    /// All stored contacts as `(petname, record)`.
    pub fn contacts(&self) -> Result<Vec<(String, ContactRecord)>, String> {
        self.state.contacts()
    }

    /// Petname → the Contact to send to.
    pub fn resolve_contact(&self, petname: &str) -> Result<Contact, String> {
        self.state
            .contacts()?
            .into_iter()
            .find(|(name, _)| name == petname)
            .map(|(_, record)| Contact::from_record(&record))
            .ok_or_else(|| format!("no contact named {petname:?}"))
    }

    /// Persist a verified envelope and its participant→conversation mapping,
    /// so a later `send` to the same people threads into this conversation.
    fn remember(&self, envelope: &MessageEnvelope) -> Result<(), String> {
        let conversation = envelope.core.conversation.unwrap_or_else(|| envelope.id());
        self.state.store_envelope(conversation, envelope)?;
        let participants: BTreeSet<PublicKey> = envelope
            .core
            .recipients
            .iter()
            .copied()
            .chain([envelope.core.sender])
            .collect();
        self.state.record_conversation(&participants, conversation)
    }
}

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
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (key_hex, relay_list) = spec
            .split_once('@')
            .ok_or("contact must be <pubkey>@<relay>[,<relay>...]")?;
        let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
        for relay in &relays {
            net::parse_relay(relay)?; // validate early, before any network work
        }
        Ok(Contact {
            keys: vec![PublicKey(hex::parse32(key_hex)?)],
            relays,
        })
    }

    fn from_record(record: &ContactRecord) -> Self {
        Contact {
            keys: record.keys.clone(),
            relays: record.relays.clone(),
        }
    }
}

pub struct SendReceipt {
    pub id: MessageId,
    pub conversation: MessageId,
    pub seq: u64,
    pub blob_count: usize,
    pub relay_count: usize,
}

/// One fetched envelope: opened if this device could decrypt it. The edge
/// decides presentation; `envelope.core` has sender, conversation, blob refs.
pub struct Received {
    pub envelope: MessageEnvelope,
    /// The relay it arrived through — where its blobs can be fetched.
    pub relay: String,
    pub body: Result<Vec<u8>, OpenError>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}
