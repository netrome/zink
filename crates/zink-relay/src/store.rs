//! Mailbox storage: the port and the in-memory adapter (persistence is B5).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zink_protocol::{MessageEnvelope, PublicKey};

/// Retention backstop: an envelope a device never acks is dropped after
/// this long (mailbox design §8). Policy, not protocol.
pub const DEFAULT_MAILBOX_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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

/// The time source, injectable so retention tests need no sleeping.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub struct InMemoryStore {
    mailboxes: Mutex<HashMap<PublicKey, Mailbox>>,
    retention: Duration,
    clock: Clock,
}

impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryStore")
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::with_retention(DEFAULT_MAILBOX_RETENTION)
    }

    pub fn with_retention(retention: Duration) -> Self {
        Self::with_clock(retention, Arc::new(Instant::now))
    }

    pub fn with_clock(retention: Duration, clock: Clock) -> Self {
        Self {
            mailboxes: Mutex::new(HashMap::new()),
            retention,
            clock,
        }
    }

    /// Drop expired items. Called lazily on every access — no background
    /// task needed at this scale.
    fn purge_expired(&self, mailbox: &mut Mailbox) {
        let now = (self.clock)();
        let retention = self.retention;
        mailbox
            .items
            .retain(|item| now.duration_since(item.deposited_at) < retention);
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
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
    deposited_at: Instant,
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
        self.purge_expired(state);
        let id = envelope.id();
        if state.items.iter().any(|item| item.envelope.id() == id) {
            return;
        }
        state.last_cursor += 1;
        state.items.push(StoredItem {
            cursor: state.last_cursor,
            deposited_at: (self.clock)(),
            envelope,
        });
    }

    async fn fetch(&self, mailbox: PublicKey, after: u64) -> Vec<(u64, MessageEnvelope)> {
        let mut mailboxes = self.mailboxes.lock().unwrap();
        let Some(state) = mailboxes.get_mut(&mailbox) else {
            return Vec::new();
        };
        self.purge_expired(state);
        state
            .items
            .iter()
            .filter(|item| item.cursor > after)
            .map(|item| (item.cursor, item.envelope.clone()))
            .collect()
    }

    async fn ack(&self, mailbox: PublicKey, up_to: u64) {
        if let Some(state) = self.mailboxes.lock().unwrap().get_mut(&mailbox) {
            state.items.retain(|item| item.cursor > up_to);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, FORMAT_VERSION, KeyCommitment, MessageCore};

    use super::*;

    /// A controllable clock: tests advance it explicitly, no sleeping.
    fn test_clock() -> (Arc<Mutex<Instant>>, Clock) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let handle = now.clone();
        (now, Arc::new(move || *handle.lock().unwrap()))
    }

    fn envelope_to(recipient: PublicKey, body: &[u8]) -> MessageEnvelope {
        let sender = DeviceKey::from_seed([1; 32]);
        let core = MessageCore {
            version: FORMAT_VERSION,
            conversation: None,
            parents: vec![],
            recipients: vec![recipient],
            sender: sender.public(),
            seq: 0,
            logical: 0,
            timestamp_ms: 0,
            body: body.to_vec(),
            key_commit: KeyCommitment([0; 32]),
            blob_refs: vec![],
        };
        MessageEnvelope::new(core, &sender)
    }

    #[tokio::test]
    async fn fetch__should_drop_items_older_than_the_retention_backstop() {
        // Given: a store with a 100s retention and a deposited envelope
        let (now, clock) = test_clock();
        let store = InMemoryStore::with_clock(Duration::from_secs(100), clock);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.register(mailbox).await;
        store.append(mailbox, envelope_to(mailbox, b"old")).await;

        // When: time passes beyond the retention window
        *now.lock().unwrap() += Duration::from_secs(101);

        // Then: the unacked envelope is gone
        assert!(store.fetch(mailbox, 0).await.is_empty());
    }

    #[tokio::test]
    async fn fetch__should_keep_items_within_the_retention_window() {
        // Given
        let (now, clock) = test_clock();
        let store = InMemoryStore::with_clock(Duration::from_secs(100), clock);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.register(mailbox).await;
        store.append(mailbox, envelope_to(mailbox, b"fresh")).await;

        // When: time passes, but not past the window
        *now.lock().unwrap() += Duration::from_secs(99);

        // Then
        assert_eq!(store.fetch(mailbox, 0).await.len(), 1);
    }

    #[tokio::test]
    async fn append__should_expire_independently_per_item() {
        // Given: two deposits 60s apart, 100s retention
        let (now, clock) = test_clock();
        let store = InMemoryStore::with_clock(Duration::from_secs(100), clock);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.register(mailbox).await;
        store.append(mailbox, envelope_to(mailbox, b"first")).await;
        *now.lock().unwrap() += Duration::from_secs(60);
        store.append(mailbox, envelope_to(mailbox, b"second")).await;

        // When: the first crosses the window, the second does not
        *now.lock().unwrap() += Duration::from_secs(60);

        // Then: only the second remains
        let items = store.fetch(mailbox, 0).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1.core.body, b"second");
    }
}
