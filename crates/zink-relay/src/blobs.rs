//! Blob-cache retention (B5, tag-based): every pushed blob gets a
//! timestamped tag; tagged blobs are GC roots, so a blob lives until its
//! tag is pruned. Tags persist inside the blob store itself, so retention
//! state survives restarts along with the blobs — no side registry to lose.
//!
//! Eviction = a sweeper task deletes tags that are past the TTL **or point
//! at oversized blobs** (C0 cap — iroh-blobs 0.103 has no hook to reject a
//! push mid-stream, so enforcement is eviction; a hostile push holds disk
//! at most until the next sweep). The next GC run collects untagged blobs.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use iroh_blobs::Hash;
use iroh_blobs::api::Store;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::store::GcConfig;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;

use crate::clock::{Clock, WallClock};

/// Blob-cache policy knobs (relay-operator policy, not protocol).
#[derive(Debug, Clone, Copy)]
pub struct BlobCacheConfig {
    /// How long a pushed blob is kept for recipients to fetch.
    pub ttl: Duration,
    /// How often GC sweeps unprotected blobs (and the sweeper prunes tags).
    pub gc_interval: Duration,
    /// Pushes larger than this are evicted on the next sweep.
    pub max_blob_bytes: u64,
    /// Total bytes the cache may hold (R2). Per-blob and per-age caps bound
    /// neither the *count* of blobs nor the disk: pushes are ungated, so
    /// without this the cache grows without limit until the TTL catches up
    /// 30 days later. Over budget, the sweep evicts oldest-first.
    pub max_total_bytes: u64,
}

/// Default total blob budget (R2): 2 GiB. Sized to be statable next to the
/// mailbox ceiling rather than derived from anything — an operator wants one
/// number for the data dir, and `--blob-budget` moves it.
pub const DEFAULT_BLOB_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

impl Default for BlobCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30 * 24 * 60 * 60),
            gc_interval: Duration::from_secs(60 * 60),
            max_blob_bytes: 64 * 1024 * 1024,
            max_total_bytes: DEFAULT_BLOB_BUDGET_BYTES,
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
pub fn mem_blob_cache<C: Clock, W: WallClock>(
    config: BlobCacheConfig,
    clock: C,
    wall_clock: W,
) -> MemStore {
    let store = MemStore::new_with_opts(iroh_blobs::store::mem::Options {
        gc_config: Some(GcConfig {
            interval: config.gc_interval,
            add_protected: None,
        }),
    });
    spawn_tag_sweeper((*store).clone(), config, clock, wall_clock);
    store
}

/// On-disk blob cache with TTL + size-cap semantics. Blobs *and* their
/// retention tags live in `root` and survive restarts together.
pub async fn fs_blob_cache<C: Clock, W: WallClock>(
    root: &Path,
    config: BlobCacheConfig,
    clock: C,
    wall_clock: W,
) -> Result<FsStore, Box<dyn std::error::Error + Send + Sync>> {
    let mut options = iroh_blobs::store::fs::options::Options::new(root);
    options.gc = Some(GcConfig {
        interval: config.gc_interval,
        add_protected: None,
    });
    let store = FsStore::load_with_opts(root.join("blobs.db"), options).await?;
    spawn_tag_sweeper((*store).clone(), config, clock, wall_clock);
    Ok(store)
}

/// Periodically delete push tags that expired or point at oversized blobs;
/// GC then collects whatever those tags were keeping alive.
fn spawn_tag_sweeper<C: Clock, W: WallClock>(
    store: Store,
    config: BlobCacheConfig,
    clock: C,
    wall_clock: W,
) {
    tokio::spawn(async move {
        // Sweep-then-sleep keeps `interval`'s immediate first tick, and
        // sleeping after the work matches the old Skip tick behavior. The
        // wait goes through the port — no raw timers (ADR 0004).
        loop {
            sweep_tags(&store, config, wall_clock.now_ms()).await;
            clock.sleep(config.gc_interval).await;
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
        if let Some(pushed_ms) = push_tag_timestamp_ms(&tag.name.0) {
            push_tags.push((tag, pushed_ms));
        }
    }
    // Free deletions first (age, per-blob cap); the budget then judges only
    // what survived. One status lookup per tag — the size feeds both tests.
    let mut held: BTreeMap<Hash, Held> = BTreeMap::new();
    for (tag, pushed_ms) in push_tags {
        let size = blob_size(store, tag.hash).await;
        let expired = now_ms.saturating_sub(pushed_ms) >= ttl_ms;
        if expired || size > config.max_blob_bytes {
            let _ = store.tags().delete(&tag.name.0).await;
            continue;
        }
        let entry = held.entry(tag.hash).or_insert_with(|| Held {
            size,
            newest_ms: 0,
            tags: Vec::new(),
        });
        entry.newest_ms = entry.newest_ms.max(pushed_ms);
        entry.tags.push(tag.name.0.to_vec());
    }
    evict_to_budget(store, held, config.max_total_bytes).await;
}

/// One retained blob and every push tag protecting it.
struct Held {
    size: u64,
    /// The blob's effective age: a re-push writes a *new* tag and the newest
    /// one keeps it alive (see `push_tag`), so that is what eviction sorts on.
    newest_ms: u64,
    tags: Vec<Vec<u8>>,
}

/// Evict oldest-first until the cache is inside its total budget (R2).
///
/// **Grouped by hash, not by tag.** Re-pushing the same blob writes a second
/// tag, so summing per tag double-counts the bytes, and deleting one tag
/// frees nothing while the other still protects the blob. A hash is evicted
/// by dropping *all* of its push tags at once; GC collects it on the next run.
///
/// Untagged blobs are ignored: nothing protects them, so they are already
/// transient and GC takes them regardless.
async fn evict_to_budget(store: &Store, held: BTreeMap<Hash, Held>, budget: u64) {
    let mut total: u64 = held.values().map(|entry| entry.size).sum();
    if total <= budget {
        return;
    }
    let mut oldest_first: Vec<&Held> = held.values().collect();
    // Stable sort over hash-ordered input, so ties evict deterministically.
    oldest_first.sort_by_key(|entry| entry.newest_ms);
    let over_by = total - budget;
    let mut evicted = 0usize;
    for entry in oldest_first {
        if total <= budget {
            break;
        }
        for name in &entry.tags {
            let _ = store.tags().delete(name).await;
        }
        total = total.saturating_sub(entry.size);
        evicted += 1;
    }
    tracing::info!(
        evicted,
        over_by,
        now_held = total,
        budget,
        "blob cache over budget; evicted oldest pushes"
    );
}

/// Bytes this blob occupies, as far as the store knows. A partial push whose
/// size is not yet known counts 0 — it is mid-negotiation, and the next sweep
/// will count it rather than this one guessing.
async fn blob_size(store: &Store, hash: Hash) -> u64 {
    match store.blobs().status(hash).await {
        Ok(BlobStatus::Complete { size }) => size,
        Ok(BlobStatus::Partial { size: Some(size) }) => size,
        _ => 0,
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
    async fn sweep_tags__should_evict_oldest_first_down_to_the_total_budget() {
        // Given: three fresh, individually-legal blobs whose *total* is over
        // budget — the case neither the TTL nor the per-blob cap catches, and
        // the one that let an ungated pusher fill the disk for 30 days.
        let store = MemStore::new();
        let now_ms = 1_700_000_000_000u64;
        let mut pushed = Vec::new();
        for (age_ms, fill) in [(3000u64, 0xAA), (2000, 0xBB), (1000, 0xCC)] {
            let blob = store.add_bytes(vec![fill; 1000]).await.expect("add").hash;
            store
                .tags()
                .set(push_tag(now_ms - age_ms, &blob), blob)
                .await
                .expect("tag");
            pushed.push(blob);
        }
        let config = BlobCacheConfig {
            max_total_bytes: 2500, // room for two of the three
            ..test_config()
        };

        // When
        sweep_tags(&store, config, now_ms).await;

        // Then: the oldest went, the two newest stayed — and we are under
        // budget, not merely smaller.
        let remaining = remaining_push_tags(&store).await;
        assert_eq!(remaining.len(), 2, "one eviction was enough");
        assert!(!remaining.contains(&pushed[0]), "the oldest was evicted");
        assert!(remaining.contains(&pushed[1]) && remaining.contains(&pushed[2]));
    }

    #[tokio::test]
    async fn sweep_tags__should_not_double_count_a_re_pushed_blob() {
        // Given: ONE blob carrying two push tags (a re-push writes a second
        // rather than replacing), plus one other. Counting per tag would read
        // 3000 bytes held against a 2500 budget and evict something that did
        // not need evicting.
        let store = MemStore::new();
        let now_ms = 1_700_000_000_000u64;
        let repushed = store.add_bytes(vec![0xAA; 1000]).await.expect("add").hash;
        let other = store.add_bytes(vec![0xBB; 1000]).await.expect("add").hash;
        for pushed_ms in [now_ms - 5000, now_ms - 100] {
            store
                .tags()
                .set(push_tag(pushed_ms, &repushed), repushed)
                .await
                .expect("tag");
        }
        store
            .tags()
            .set(push_tag(now_ms - 1000, &other), other)
            .await
            .expect("tag");
        let config = BlobCacheConfig {
            max_total_bytes: 2500,
            ..test_config()
        };

        // When
        sweep_tags(&store, config, now_ms).await;

        // Then: 2000 bytes really held, under budget — nothing evicted, and
        // both of the re-pushed blob's tags survive.
        let remaining = remaining_push_tags(&store).await;
        assert_eq!(remaining.len(), 3, "all three tags kept: {remaining:?}");
    }

    #[tokio::test]
    async fn sweep_tags__should_drop_every_tag_of_an_evicted_blob() {
        // Given: a re-pushed (two-tag) old blob and a newer one, with room
        // for only one. Deleting a single tag would free nothing — the other
        // tag still protects the blob from GC.
        let store = MemStore::new();
        let now_ms = 1_700_000_000_000u64;
        let old = store.add_bytes(vec![0xAA; 1000]).await.expect("add").hash;
        let new = store.add_bytes(vec![0xBB; 1000]).await.expect("add").hash;
        for pushed_ms in [now_ms - 9000, now_ms - 8000] {
            store
                .tags()
                .set(push_tag(pushed_ms, &old), old)
                .await
                .expect("tag");
        }
        store
            .tags()
            .set(push_tag(now_ms - 100, &new), new)
            .await
            .expect("tag");
        let config = BlobCacheConfig {
            max_total_bytes: 1500,
            ..test_config()
        };

        // When
        sweep_tags(&store, config, now_ms).await;

        // Then: the old blob is fully unprotected — no tag of it left
        assert_eq!(remaining_push_tags(&store).await, vec![new]);
    }

    #[tokio::test]
    async fn sweep_tags__should_leave_a_cache_inside_its_budget_alone() {
        // Given: two blobs well under budget
        let store = MemStore::new();
        let now_ms = 1_700_000_000_000u64;
        for fill in [0xAA, 0xBB] {
            let blob = store.add_bytes(vec![fill; 100]).await.expect("add").hash;
            store
                .tags()
                .set(push_tag(now_ms, &blob), blob)
                .await
                .expect("tag");
        }

        // When
        sweep_tags(&store, test_config(), now_ms).await;

        // Then: the budget is a ceiling, not a target — nothing is evicted
        assert_eq!(remaining_push_tags(&store).await.len(), 2);
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
