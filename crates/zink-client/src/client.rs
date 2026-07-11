//! The client: one device key, one endpoint, on-disk state, and the
//! send/recv flows over them. Edges (CLI, app) stay presentation-only.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::Endpoint;
use rand_core::OsRng;
use zink_protocol::{
    BlobDraft, BlobHash, DeviceKey, MailboxOp, MailboxResult, MessageDraft, MessageEnvelope,
    MessageId, OpenError, PublicKey, distinct_relays,
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
        let recipients: Vec<PublicKey> = contacts.iter().map(|c| c.key).collect();
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

    /// Drain every relay: register, fetch, dedup by message id, open, and
    /// remember what verified; ack each relay at its own cursor.
    pub async fn recv(&self, relays: &[String]) -> Result<Vec<Received>, String> {
        let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
        let mut received = Vec::new();
        for relay in relays {
            let connection =
                net::connect(&self.endpoint, relay, zink_protocol::MAILBOX_ALPN).await?;
            net::request(&connection, MailboxOp::Register).await?;
            let items = match net::request(&connection, MailboxOp::Fetch { after: 0 }).await? {
                MailboxResult::Envelopes { items } => items,
                other => return Err(format!("unexpected response from {relay}: {other:?}")),
            };
            // Cursors are relay-local: ack each relay at its own high-water mark.
            let mut last_cursor = None;
            for item in items {
                last_cursor = last_cursor.max(Some(item.cursor));
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
            if let Some(up_to) = last_cursor {
                net::request(&connection, MailboxOp::Ack { up_to }).await?;
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

/// A recipient and the relays hosting their mailbox — the stand-in for the
/// ContactRecord until C2.
pub struct Contact {
    pub key: PublicKey,
    pub relays: Vec<String>,
}

impl Contact {
    /// `<pubkey-hex>@<relay>[,<relay>…]` — hex contains no `@`, so the
    /// first `@` splits key from relay list.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (key_hex, relay_list) = spec
            .split_once('@')
            .ok_or("contact must be <pubkey>@<relay>[,<relay>...]")?;
        let relays: Vec<String> = relay_list.split(',').map(str::to_string).collect();
        for relay in &relays {
            net::parse_relay(relay)?; // validate early, before any network work
        }
        Ok(Contact {
            key: PublicKey(hex::parse32(key_hex)?),
            relays,
        })
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
