//! Blob-cache retention (B5, tag-based): every pushed blob gets a
//! timestamped tag; tagged blobs are GC roots, so a blob lives until its
//! tag is pruned. Tags persist inside the blob store itself, so retention
//! state survives restarts along with the blobs — no side registry to lose.
//!
//! Eviction = a sweeper task deletes tags that are past the TTL **or point
//! at oversized blobs** (C0 cap — iroh-blobs 0.103 has no hook to reject a
//! push mid-stream, so enforcement is eviction; a hostile push holds disk
//! at most until the next sweep). The next GC run collects untagged blobs.

use std::path::Path;
use std::time::Duration;

use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::store::GcConfig;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;

use crate::clock::WallClock;

/// Blob-cache policy knobs (relay-operator policy, not protocol).
#[derive(Debug, Clone, Copy)]
pub struct BlobCacheConfig {
    /// How long a pushed blob is kept for recipients to fetch.
    pub ttl: Duration,
    /// How often GC sweeps unprotected blobs (and the sweeper prunes tags).
    pub gc_interval: Duration,
    /// Pushes larger than this are evicted on the next sweep.
    pub max_blob_bytes: u64,
}

impl Default for BlobCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30 * 24 * 60 * 60),
            gc_interval: Duration::from_secs(60 * 60),
            max_blob_bytes: 64 * 1024 * 1024,
        }
    }
}

const PUSH_TAG_PREFIX: &str = "pushed-";

/// The retention tag for a push: `pushed-<unix-ms:020>-<hash-hex>`.
/// Re-pushing writes a new tag — the newest one keeps the blob alive.
pub fn push_tag(now_ms: u64, hash: &Hash) -> String {
    format!("{PUSH_TAG_PREFIX}{now_ms:020}-{}", hash.to_hex())
}

/// The timestamp of a push tag; `None` for tags this scheme doesn't own.
pub fn push_tag_timestamp_ms(tag: &[u8]) -> Option<u64> {
    let name = std::str::from_utf8(tag).ok()?;
    let rest = name.strip_prefix(PUSH_TAG_PREFIX)?;
    let (timestamp, _) = rest.split_at_checked(20)?;
    timestamp.parse().ok()
}

/// In-memory blob cache with TTL + size-cap semantics (dev / tests).
pub fn mem_blob_cache<W: WallClock>(config: BlobCacheConfig, clock: W) -> MemStore {
    let store = MemStore::new_with_opts(iroh_blobs::store::mem::Options {
        gc_config: Some(GcConfig {
            interval: config.gc_interval,
            add_protected: None,
        }),
    });
    spawn_tag_sweeper((*store).clone(), config, clock);
    store
}

/// On-disk blob cache with TTL + size-cap semantics. Blobs *and* their
/// retention tags live in `root` and survive restarts together.
pub async fn fs_blob_cache<W: WallClock>(
    root: &Path,
    config: BlobCacheConfig,
    clock: W,
) -> Result<FsStore, Box<dyn std::error::Error + Send + Sync>> {
    let mut options = iroh_blobs::store::fs::options::Options::new(root);
    options.gc = Some(GcConfig {
        interval: config.gc_interval,
        add_protected: None,
    });
    let store = FsStore::load_with_opts(root.join("blobs.db"), options).await?;
    spawn_tag_sweeper((*store).clone(), config, clock);
    Ok(store)
}

/// Periodically delete push tags that expired or point at oversized blobs;
/// GC then collects whatever those tags were keeping alive.
fn spawn_tag_sweeper<W: WallClock>(store: Store, config: BlobCacheConfig, clock: W) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.gc_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            sweep_tags(&store, config, clock.now_ms()).await;
        }
    });
}

async fn sweep_tags(store: &Store, config: BlobCacheConfig, now_ms: u64) {
    let ttl_ms = config.ttl.as_millis() as u64;
    let Ok(mut tags) = store.tags().list().await else {
        return;
    };
    // Collect first, judge and delete after — no mutation under the live
    // list stream.
    let mut push_tags = Vec::new();
    while let Some(tag) = tags.next().await {
        let Ok(tag) = tag else { continue };
        if push_tag_timestamp_ms(&tag.name.0).is_some() {
            push_tags.push(tag);
        }
    }
    for tag in push_tags {
        let expired = push_tag_timestamp_ms(&tag.name.0)
            .is_some_and(|pushed_ms| now_ms.saturating_sub(pushed_ms) >= ttl_ms);
        let oversized = matches!(
            store.blobs().status(tag.hash).await,
            Ok(BlobStatus::Complete { size }) if size > config.max_blob_bytes
        ) || matches!(
            store.blobs().status(tag.hash).await,
            Ok(BlobStatus::Partial { size: Some(size) }) if size > config.max_blob_bytes
        );
        if expired || oversized {
            let _ = store.tags().delete(&tag.name.0).await;
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn hash(n: u8) -> Hash {
        Hash::from_bytes([n; 32])
    }

    #[test]
    fn push_tag__should_roundtrip_its_timestamp() {
        // Given
        let tag = push_tag(1_700_000_000_000, &hash(1));

        // When / Then
        assert_eq!(
            push_tag_timestamp_ms(tag.as_bytes()),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn push_tag_timestamp_ms__should_ignore_foreign_tags() {
        for foreign in [
            &b"some-other-tag"[..],
            b"pushed-notanumber",
            b"pushed-",
            &[0xFF, 0xFE][..],
        ] {
            assert_eq!(push_tag_timestamp_ms(foreign), None);
        }
    }

    async fn remaining_push_tags(store: &MemStore) -> Vec<Hash> {
        // Only *our* push tags count — `add_bytes` creates its own
        // auto-tags, which are not this scheme's.
        let mut tags = store.tags().list().await.expect("list");
        let mut remaining = Vec::new();
        while let Some(tag) = tags.next().await {
            let tag = tag.expect("tag");
            if push_tag_timestamp_ms(&tag.name.0).is_some() {
                remaining.push(tag.hash);
            }
        }
        remaining
    }

    fn test_config() -> BlobCacheConfig {
        BlobCacheConfig {
            ttl: Duration::from_secs(100),
            ..BlobCacheConfig::default()
        }
    }

    #[tokio::test]
    async fn sweep_tags__should_delete_only_tags_past_the_ttl() {
        // Given: two pushed blobs, one old, one fresh
        let store = MemStore::new();
        let old = store.add_bytes(b"old".to_vec()).await.expect("add old");
        let fresh = store.add_bytes(b"fresh".to_vec()).await.expect("add fresh");
        let now_ms = 1_700_000_000_000u64;
        store
            .tags()
            .set(push_tag(now_ms - 200_000, &old.hash), old.hash)
            .await
            .expect("tag old");
        store
            .tags()
            .set(push_tag(now_ms - 50_000, &fresh.hash), fresh.hash)
            .await
            .expect("tag fresh");

        // When: sweeping with a 100s TTL
        sweep_tags(&store, test_config(), now_ms).await;

        // Then: only the fresh blob's push tag remains
        assert_eq!(remaining_push_tags(&store).await, vec![fresh.hash]);
    }

    #[tokio::test]
    async fn sweep_tags__should_evict_oversized_blobs_regardless_of_age() {
        // Given: a fresh-but-huge blob and a fresh small one, 1 KiB cap
        let store = MemStore::new();
        let big = store.add_bytes(vec![0xAB; 4096]).await.expect("add big");
        let small = store.add_bytes(b"ok".to_vec()).await.expect("add small");
        let now_ms = 1_700_000_000_000u64;
        for tag in [
            (push_tag(now_ms, &big.hash), big.hash),
            (push_tag(now_ms, &small.hash), small.hash),
        ] {
            store.tags().set(tag.0, tag.1).await.expect("tag");
        }
        let config = BlobCacheConfig {
            max_blob_bytes: 1024,
            ..test_config()
        };

        // When
        sweep_tags(&store, config, now_ms).await;

        // Then: the oversized blob lost its protection; the small one kept it
        assert_eq!(remaining_push_tags(&store).await, vec![small.hash]);
    }
}
