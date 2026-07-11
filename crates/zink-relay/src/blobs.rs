//! Blob-cache retention (B5, tag-based): every pushed blob gets a
//! timestamped tag; tagged blobs are GC roots, so a blob lives until its
//! tag is pruned. Tags persist inside the blob store itself, so retention
//! state survives restarts along with the blobs — no side registry to lose.
//!
//! Eviction = a pruner task deletes tags older than the TTL; the next GC
//! run collects the now-unprotected blobs.

use std::path::Path;
use std::time::Duration;

use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use iroh_blobs::store::GcConfig;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;

use crate::clock::WallClock;

/// How long a pushed blob is kept for recipients to fetch.
pub const DEFAULT_BLOB_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How often GC sweeps unprotected blobs (and the pruner sweeps old tags).
pub const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

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

/// In-memory blob cache with TTL semantics (dev / tests).
pub fn mem_blob_cache<W: WallClock>(ttl: Duration, gc_interval: Duration, clock: W) -> MemStore {
    let store = MemStore::new_with_opts(iroh_blobs::store::mem::Options {
        gc_config: Some(GcConfig {
            interval: gc_interval,
            add_protected: None,
        }),
    });
    spawn_tag_pruner((*store).clone(), ttl, gc_interval, clock);
    store
}

/// On-disk blob cache with TTL semantics. Blobs *and* their retention tags
/// live in `root` and survive restarts together.
pub async fn fs_blob_cache<W: WallClock>(
    root: &Path,
    ttl: Duration,
    gc_interval: Duration,
    clock: W,
) -> Result<FsStore, Box<dyn std::error::Error + Send + Sync>> {
    let mut options = iroh_blobs::store::fs::options::Options::new(root);
    options.gc = Some(GcConfig {
        interval: gc_interval,
        add_protected: None,
    });
    let store = FsStore::load_with_opts(root.join("blobs.db"), options).await?;
    spawn_tag_pruner((*store).clone(), ttl, gc_interval, clock);
    Ok(store)
}

/// Periodically delete push tags older than `ttl`; GC then collects the
/// blobs they were keeping alive.
fn spawn_tag_pruner<W: WallClock>(store: Store, ttl: Duration, interval: Duration, clock: W) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            prune_expired_tags(&store, ttl, clock.now_ms()).await;
        }
    });
}

async fn prune_expired_tags(store: &Store, ttl: Duration, now_ms: u64) {
    let ttl_ms = ttl.as_millis() as u64;
    let Ok(mut tags) = store.tags().list().await else {
        return;
    };
    // Collect first, delete after — no mutation under the live list stream.
    let mut expired = Vec::new();
    while let Some(tag) = tags.next().await {
        let Ok(tag) = tag else { continue };
        let Some(pushed_ms) = push_tag_timestamp_ms(&tag.name.0) else {
            continue; // not ours
        };
        if now_ms.saturating_sub(pushed_ms) >= ttl_ms {
            expired.push(tag.name);
        }
    }
    for name in expired {
        let _ = store.tags().delete(&name.0).await;
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

    #[tokio::test]
    async fn prune_expired_tags__should_delete_only_tags_past_the_ttl() {
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

        // When: pruning with a 100s TTL
        prune_expired_tags(&store, Duration::from_secs(100), now_ms).await;

        // Then: of *our* push tags, only the fresh blob's remains
        // (`add_bytes` creates its own auto-tags — not this scheme's).
        let mut tags = store.tags().list().await.expect("list");
        let mut remaining = Vec::new();
        while let Some(tag) = tags.next().await {
            let tag = tag.expect("tag");
            if push_tag_timestamp_ms(&tag.name.0).is_some() {
                remaining.push(tag);
            }
        }
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].hash, fresh.hash);
    }
}
