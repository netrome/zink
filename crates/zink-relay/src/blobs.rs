//! Blob-cache retention: a pushed blob lives for a TTL, then iroh-blobs GC
//! collects it (mailbox design §7/§8 — TTL cache, not permanent storage).
//!
//! iroh-blobs deletes unprotected blobs on every GC run, so retention is
//! expressed inversely: we track when each blob was pushed and *protect* the
//! ones still inside the TTL window.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh_blobs::Hash;
use iroh_blobs::store::mem::{MemStore, Options};
use iroh_blobs::store::{GcConfig, ProtectCb, ProtectOutcome};

use crate::store::Clock;

/// How long a pushed blob is kept for recipients to fetch.
pub const DEFAULT_BLOB_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How often GC sweeps expired blobs.
pub const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Tracks push times and answers "which blobs are still protected?".
pub struct BlobRetention {
    pushed_at: Mutex<HashMap<Hash, Instant>>,
    ttl: Duration,
    clock: Clock,
}

impl BlobRetention {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    pub fn with_clock(ttl: Duration, clock: Clock) -> Self {
        Self {
            pushed_at: Mutex::new(HashMap::new()),
            ttl,
            clock,
        }
    }

    /// Record (or refresh) a push. A re-push of an existing blob restarts
    /// its TTL — deliberate: someone still cares about it.
    pub fn record(&self, hash: Hash) {
        let now = (self.clock)();
        self.pushed_at.lock().unwrap().insert(hash, now);
    }

    /// Hashes still inside the TTL window. Expired entries are pruned from
    /// the registry as a side effect — GC deletes their blobs right after.
    pub fn protected(&self) -> HashSet<Hash> {
        let now = (self.clock)();
        let ttl = self.ttl;
        let mut pushed_at = self.pushed_at.lock().unwrap();
        pushed_at.retain(|_, at| now.duration_since(*at) < ttl);
        pushed_at.keys().copied().collect()
    }
}

impl std::fmt::Debug for BlobRetention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRetention")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// A blob store wired for cache semantics: GC runs every `gc_interval` and
/// deletes everything `retention` no longer protects.
pub fn blob_cache(retention: Arc<BlobRetention>, gc_interval: Duration) -> MemStore {
    let protect: ProtectCb = Arc::new(move |live| {
        let retention = retention.clone();
        Box::pin(async move {
            live.extend(retention.protected());
            ProtectOutcome::Continue
        })
    });
    MemStore::new_with_opts(Options {
        gc_config: Some(GcConfig {
            interval: gc_interval,
            add_protected: Some(protect),
        }),
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn test_clock() -> (Arc<Mutex<Instant>>, Clock) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let handle = now.clone();
        (now, Arc::new(move || *handle.lock().unwrap()))
    }

    #[test]
    fn protected__should_include_blobs_inside_the_ttl_window() {
        // Given
        let (_, clock) = test_clock();
        let retention = BlobRetention::with_clock(Duration::from_secs(100), clock);
        let hash = Hash::from_bytes([1; 32]);

        // When
        retention.record(hash);

        // Then
        assert!(retention.protected().contains(&hash));
    }

    #[test]
    fn protected__should_drop_blobs_past_the_ttl() {
        // Given
        let (now, clock) = test_clock();
        let retention = BlobRetention::with_clock(Duration::from_secs(100), clock);
        let hash = Hash::from_bytes([1; 32]);
        retention.record(hash);

        // When
        *now.lock().unwrap() += Duration::from_secs(101);

        // Then
        assert!(retention.protected().is_empty());
    }

    #[test]
    fn record__should_restart_the_ttl_on_a_re_push() {
        // Given: a blob pushed, then re-pushed 60s later (100s TTL)
        let (now, clock) = test_clock();
        let retention = BlobRetention::with_clock(Duration::from_secs(100), clock);
        let hash = Hash::from_bytes([1; 32]);
        retention.record(hash);
        *now.lock().unwrap() += Duration::from_secs(60);
        retention.record(hash);

        // When: 60 more seconds — past the first push, not the second
        *now.lock().unwrap() += Duration::from_secs(60);

        // Then
        assert!(retention.protected().contains(&hash));
    }
}
