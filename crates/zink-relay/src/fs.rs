//! On-disk mailbox store (slice B5): the second `MailboxStore` adapter.
//!
//! Layout: one directory per registered mailbox (hex of its key), one file
//! per envelope. Item metadata lives in the *filename* —
//! `<cursor:020>-<deposited-unix-ms:020>-<message-id-hex>.env` — and the
//! file content is the envelope's canonical wire bytes, so nothing here
//! invents a serialization format. Timestamps are wall-clock: they must
//! survive restarts, which `Instant` cannot.
//!
//! I/O is synchronous `std::fs` inside the async methods: items are a few
//! KiB and mailboxes hold tens of messages at friends-scale — not worth a
//! blocking-pool round-trip per call yet.

use std::path::{Path, PathBuf};
use std::time::Duration;

use zink_protocol::{MessageEnvelope, PublicKey};

use crate::clock::{SystemClock, WallClock};
use crate::store::{DEFAULT_MAILBOX_RETENTION, MailboxStore};

pub struct FsMailboxStore<W: WallClock = SystemClock> {
    root: PathBuf,
    retention: Duration,
    clock: W,
}

impl FsMailboxStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_clock(root, DEFAULT_MAILBOX_RETENTION, SystemClock)
    }
}

impl<W: WallClock> FsMailboxStore<W> {
    pub fn with_clock(root: impl Into<PathBuf>, retention: Duration, clock: W) -> Self {
        Self {
            root: root.into(),
            retention,
            clock,
        }
    }

    fn mailbox_dir(&self, mailbox: &PublicKey) -> PathBuf {
        self.root.join(hex(&mailbox.0))
    }

    /// All live items in a mailbox, oldest first. Expired files are deleted
    /// on the way (the lazy purge, same as the in-memory store).
    fn live_items(&self, dir: &Path) -> Vec<ItemName> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let now_ms = self.clock.now_ms();
        let retention_ms = self.retention.as_millis() as u64;
        let mut items: Vec<ItemName> = entries
            .filter_map(|entry| ItemName::parse(&entry.ok()?.file_name().to_string_lossy()))
            .collect();
        items.retain(|item| {
            let expired = now_ms.saturating_sub(item.deposited_ms) >= retention_ms;
            if expired {
                let _ = std::fs::remove_file(dir.join(item.file_name()));
            }
            !expired
        });
        items.sort_by_key(|item| item.cursor);
        items
    }
}

impl<W: WallClock> std::fmt::Debug for FsMailboxStore<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsMailboxStore")
            .field("root", &self.root)
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl<W: WallClock> MailboxStore for FsMailboxStore<W> {
    async fn register(&self, mailbox: PublicKey) {
        let _ = std::fs::create_dir_all(self.mailbox_dir(&mailbox));
    }

    async fn append(&self, mailbox: PublicKey, envelope: MessageEnvelope) {
        let dir = self.mailbox_dir(&mailbox);
        if !dir.is_dir() {
            return; // not registered
        }
        let items = self.live_items(&dir);
        let id = hex(&envelope.id().0);
        if items.iter().any(|item| item.id_hex == id) {
            return; // duplicate — idempotent retry
        }
        let next_cursor = items.last().map_or(1, |item| item.cursor + 1);
        let name = ItemName {
            cursor: next_cursor,
            deposited_ms: self.clock.now_ms(),
            id_hex: id,
        };
        let _ = std::fs::write(dir.join(name.file_name()), envelope.to_bytes());
    }

    async fn fetch(&self, mailbox: PublicKey, after: u64) -> Vec<(u64, MessageEnvelope)> {
        let dir = self.mailbox_dir(&mailbox);
        self.live_items(&dir)
            .into_iter()
            .filter(|item| item.cursor > after)
            .filter_map(|item| {
                let bytes = std::fs::read(dir.join(item.file_name())).ok()?;
                Some((item.cursor, MessageEnvelope::try_from_bytes(&bytes).ok()?))
            })
            .collect()
    }

    async fn ack(&self, mailbox: PublicKey, up_to: u64) {
        let dir = self.mailbox_dir(&mailbox);
        for item in self.live_items(&dir) {
            if item.cursor <= up_to {
                let _ = std::fs::remove_file(dir.join(item.file_name()));
            }
        }
    }
}

/// The filename scheme, parsed and rendered in one place.
#[derive(Debug, PartialEq, Eq)]
struct ItemName {
    cursor: u64,
    deposited_ms: u64,
    id_hex: String,
}

impl ItemName {
    fn file_name(&self) -> String {
        format!(
            "{:020}-{:020}-{}.env",
            self.cursor, self.deposited_ms, self.id_hex
        )
    }

    fn parse(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(".env")?;
        let (cursor, rest) = stem.split_at_checked(20)?;
        let (deposited, id_hex) = rest.strip_prefix('-')?.split_at_checked(20)?;
        Some(Self {
            cursor: cursor.parse().ok()?,
            deposited_ms: deposited.parse().ok()?,
            id_hex: id_hex.strip_prefix('-')?.to_string(),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use zink_protocol::{DeviceKey, FORMAT_VERSION, KeyCommitment, MessageCore};

    use super::*;
    use crate::testutil::test_wall_clock;

    fn temp_root(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zink-fsmailbox-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn envelope(recipient: PublicKey, body: &[u8]) -> MessageEnvelope {
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

    #[test]
    fn item_name__should_roundtrip_through_the_filename() {
        // Given
        let name = ItemName {
            cursor: 42,
            deposited_ms: 1_700_000_000_000,
            id_hex: "ab".repeat(32),
        };

        // When / Then
        assert_eq!(ItemName::parse(&name.file_name()), Some(name));
        assert_eq!(ItemName::parse("garbage"), None);
        assert_eq!(ItemName::parse(".env"), None);
    }

    #[tokio::test]
    async fn fetch__should_return_messages_deposited_before_a_restart() {
        // Given: a store that deposits, then is dropped
        let root = temp_root("restart");
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        let deposited = envelope(mailbox, b"survives");
        {
            let store = FsMailboxStore::new(&root);
            store.register(mailbox).await;
            store.append(mailbox, deposited.clone()).await;
        }

        // When: a fresh store opens the same directory
        let store = FsMailboxStore::new(&root);
        let items = store.fetch(mailbox, 0).await;

        // Then
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1, deposited);

        std::fs::remove_dir_all(&root).expect("clean up temp dir");
    }

    #[tokio::test]
    async fn fetch__should_drop_items_past_the_retention_backstop() {
        // Given
        let root = temp_root("retention");
        let (now, clock) = test_wall_clock();
        let store = FsMailboxStore::with_clock(&root, Duration::from_secs(100), clock);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.register(mailbox).await;
        store.append(mailbox, envelope(mailbox, b"old")).await;

        // When
        *now.lock().unwrap() += 101_000;

        // Then: purged, including the file on disk
        assert!(store.fetch(mailbox, 0).await.is_empty());
        assert_eq!(
            std::fs::read_dir(store.mailbox_dir(&mailbox))
                .unwrap()
                .count(),
            0
        );

        std::fs::remove_dir_all(&root).expect("clean up temp dir");
    }

    #[tokio::test]
    async fn append__should_dedup_and_ack_should_delete() {
        // Given
        let root = temp_root("dedup-ack");
        let store = FsMailboxStore::new(&root);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.register(mailbox).await;
        let first = envelope(mailbox, b"one");
        store.append(mailbox, first.clone()).await;
        store.append(mailbox, first).await; // retry
        store.append(mailbox, envelope(mailbox, b"two")).await;

        // When
        let items = store.fetch(mailbox, 0).await;

        // Then: deduped, cursors monotonic
        assert_eq!(items.len(), 2);
        assert!(items[0].0 < items[1].0);

        // And: ack up to the first drops only the first
        store.ack(mailbox, items[0].0).await;
        let remaining = store.fetch(mailbox, 0).await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.core.body, b"two");

        std::fs::remove_dir_all(&root).expect("clean up temp dir");
    }

    #[tokio::test]
    async fn append__should_skip_unregistered_mailboxes() {
        let root = temp_root("unregistered");
        let store = FsMailboxStore::new(&root);
        let mailbox = DeviceKey::from_seed([2; 32]).public();
        store.append(mailbox, envelope(mailbox, b"void")).await;
        assert!(store.fetch(mailbox, 0).await.is_empty());
        std::fs::remove_dir_all(&root).expect("clean up temp dir");
    }
}
