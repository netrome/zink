//! Mailbox storage: the port and the in-memory adapter (persistence is B5).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

use zink_protocol::{MessageEnvelope, PublicKey};

/// What the mailbox domain needs from storage. Async trait (per STYLE.md)
/// so an on-disk adapter can implement it later without touching the domain.
pub trait MailboxStore: Send + Sync + 'static {
    /// Create or refresh a mailbox.
    fn register(&self, mailbox: PublicKey) -> impl Future<Output = ()> + Send;

    /// Append an envelope to a mailbox. No-op if the mailbox is not
    /// registered (no storage for keys that never asked) or if the message
    /// id is already present (idempotent sender retries).
    fn append(
        &self,
        mailbox: PublicKey,
        envelope: MessageEnvelope,
    ) -> impl Future<Output = ()> + Send;

    /// Envelopes with cursor > `after`, oldest first, each with its cursor.
    fn fetch(
        &self,
        mailbox: PublicKey,
        after: u64,
    ) -> impl Future<Output = Vec<(u64, MessageEnvelope)>> + Send;

    /// Drop envelopes with cursor ≤ `up_to`.
    fn ack(&self, mailbox: PublicKey, up_to: u64) -> impl Future<Output = ()> + Send;
}

#[derive(Debug, Default)]
pub struct InMemoryStore {
    mailboxes: Mutex<HashMap<PublicKey, Mailbox>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Default)]
struct Mailbox {
    last_cursor: u64,
    items: Vec<StoredItem>,
}

#[derive(Debug)]
struct StoredItem {
    cursor: u64,
    envelope: MessageEnvelope,
}

impl MailboxStore for InMemoryStore {
    async fn register(&self, mailbox: PublicKey) {
        self.mailboxes.lock().unwrap().entry(mailbox).or_default();
    }

    async fn append(&self, mailbox: PublicKey, envelope: MessageEnvelope) {
        let mut mailboxes = self.mailboxes.lock().unwrap();
        let Some(state) = mailboxes.get_mut(&mailbox) else {
            return;
        };
        let id = envelope.id();
        if state.items.iter().any(|item| item.envelope.id() == id) {
            return;
        }
        state.last_cursor += 1;
        state.items.push(StoredItem {
            cursor: state.last_cursor,
            envelope,
        });
    }

    async fn fetch(&self, mailbox: PublicKey, after: u64) -> Vec<(u64, MessageEnvelope)> {
        self.mailboxes
            .lock()
            .unwrap()
            .get(&mailbox)
            .map(|state| {
                state
                    .items
                    .iter()
                    .filter(|item| item.cursor > after)
                    .map(|item| (item.cursor, item.envelope.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn ack(&self, mailbox: PublicKey, up_to: u64) {
        if let Some(state) = self.mailboxes.lock().unwrap().get_mut(&mailbox) {
            state.items.retain(|item| item.cursor > up_to);
        }
    }
}
