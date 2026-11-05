//! Blob transfer over the transport ports: push encrypted blobs to relay
//! caches and to recipient devices (the port's `push` resolves only on
//! confirmed receipt), fetch their ciphertext back, and — the recipient
//! half of peer delivery (project 10 phase B) — take what contacts push to
//! us. The iroh-blobs mechanics live in the adapter.

use std::collections::BTreeMap;
use std::sync::Mutex;

use zink_protocol::{BlobHash, EncryptedBlob, PublicKey};

use crate::adapters::fs_store::FsStore;
use crate::error::Error;
use crate::net;
use crate::ports::clock::Clock;
use crate::ports::store::Store;
use crate::ports::transport::{AcceptBlobs, Admit, BlobEvent, DialBlobs, FetchBlob, PushBlob};
use crate::state::ClientState;

/// Client policy for blobs peers push to us. Knobs, not protocol.
#[derive(Debug, Clone, Copy)]
pub struct BlobLimits {
    /// A pushed blob larger than this is dropped on arrival. iroh-blobs
    /// 0.103 has no hook to refuse a push mid-stream, so this bounds the
    /// cache, not the transfer; the admission gate bounds who can try.
    pub max_blob_bytes: usize,
    /// Blob connections one peer may hold open at once — the in-flight cap.
    pub max_connections_per_peer: usize,
}

impl Default for BlobLimits {
    fn default() -> Self {
        Self {
            max_blob_bytes: 32 * 1024 * 1024,
            max_connections_per_peer: 2,
        }
    }
}

/// Who may push blobs to this device, and what we keep: the serving gate
/// (D0c — contacts, recognized own devices, self), a per-peer connection
/// cap, a per-blob size cap, and a hash check before the cache takes
/// anything (verify before trusting — the adapter already checked, the
/// domain does not rely on it). A pushed blob lands in the same ciphertext
/// cache a relay fetch would fill, so the message that references it
/// renders from the cache the moment it arrives. Nothing here is on the
/// wire: a declined peer sees a closed connection and falls back to the
/// relay path.
pub(crate) struct BlobGate<S = FsStore> {
    state: ClientState<S>,
    me: PublicKey,
    limits: BlobLimits,
    /// Admitted connections open per peer.
    open: Mutex<BTreeMap<[u8; 32], usize>>,
}

impl<S> BlobGate<S> {
    pub(crate) fn new(state: ClientState<S>, me: PublicKey, limits: BlobLimits) -> Self {
        Self {
            state,
            me,
            limits,
            open: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<S: Store> BlobGate<S> {
    pub(crate) async fn handle<A: Admit>(&self, event: BlobEvent<A>) {
        match event {
            BlobEvent::Connected { peer, admit } => admit.admit(self.admits(peer).await).await,
            BlobEvent::Pushed { peer, blob } => self.take(peer, &blob).await,
            BlobEvent::Closed { peer } => self.closed(peer),
        }
    }

    /// The admission decision, counted when positive.
    async fn admits(&self, peer: PublicKey) -> bool {
        if !crate::sync::serves(&self.state, self.me, peer).await {
            tracing::debug!("declining a blob push from a non-contact");
            return false;
        }
        let mut open = self.open.lock().expect("open lock");
        let count = open.entry(peer.0).or_default();
        if *count >= self.limits.max_connections_per_peer {
            tracing::debug!("declining a blob push: peer at its connection cap");
            return false;
        }
        *count += 1;
        true
    }

    async fn take(&self, peer: PublicKey, blob: &EncryptedBlob) {
        if blob.bytes.len() > self.limits.max_blob_bytes {
            tracing::warn!(
                peer = %crate::hex::encode(&peer.0),
                bytes = blob.bytes.len(),
                "dropping a pushed blob over the size cap"
            );
            return;
        }
        if blake3::hash(&blob.bytes).as_bytes() != &blob.hash.0 {
            tracing::warn!("dropping a pushed blob whose bytes do not hash as claimed");
            return;
        }
        if let Err(error) = self.state.save_blob(&blob.hash, &blob.bytes).await {
            tracing::warn!(%error, "could not cache a pushed blob");
        }
    }

    fn closed(&self, peer: PublicKey) {
        let mut open = self.open.lock().expect("open lock");
        if let Some(count) = open.get_mut(&peer.0) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&peer.0);
            }
        }
    }
}

/// The recipient side of peer blob delivery, as a loop over the
/// `AcceptBlobs` port. Sequential: admission is a map lookup and a landed
/// blob is one cache write, and the events of one connection must stay in
/// order. Ends when the transport closes.
pub(crate) async fn serve_pushes<A: AcceptBlobs, S: Store>(accept: A, gate: BlobGate<S>) {
    while let Some(event) = accept.accept_blob().await {
        gate.handle(event).await;
    }
}

/// The slowest transfer still worth waiting for. A push budget is the
/// connect deadline plus the blob's bytes at this floor rate — generous,
/// because a false timeout costs a re-push (relay path: the entry stays
/// owed, the flush re-pushes, idempotent by hash) or a relay-cache
/// fallback (peer path), while no deadline at all hangs the send on a
/// remote that admitted the push and then stalled.
const MIN_PUSH_BYTES_PER_SEC: u64 = 32 * 1024;

/// How long one blob's transfer may take: the one wait in the send path
/// that had no clock over it (project 10 S8). Derived from
/// `connect_timeout`, so an edge that tightens one tightens both.
pub(crate) fn push_deadline(
    bytes: usize,
    connect_timeout: std::time::Duration,
) -> std::time::Duration {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    connect_timeout.saturating_add(std::time::Duration::from_secs(
        bytes / MIN_PUSH_BYTES_PER_SEC,
    ))
}

/// Push each encrypted blob to one relay's cache, confirming every transfer
/// — each under its own `push_deadline`, raced on the caller's clock.
pub(crate) async fn push_blobs<B: DialBlobs>(
    net: &B,
    relay: &str,
    blobs: &[EncryptedBlob],
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<(), Error> {
    let connection = net::connect_blobs(net, relay, timeout, clock).await?;
    for blob in blobs {
        clock
            .timeout(
                push_deadline(blob.bytes.len(), timeout),
                connection.push(blob),
            )
            .await
            .map_err(|_| Error::Transport(format!("blob to {relay}: transfer timed out")))?
            .map_err(|e| Error::Transport(format!("blob to {relay}: {e}")))?;
    }
    Ok(())
}

/// Fetch one blob's *ciphertext* from a relay's cache. The caller verifies
/// and decrypts against the envelope that references it (`open_blob`) —
/// and may cache the ciphertext, which stays exactly as untrusted as the
/// relay it came from.
pub(crate) async fn fetch_encrypted<B: DialBlobs>(
    net: &B,
    relay: &str,
    hash: &BlobHash,
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<Vec<u8>, Error> {
    let connection = net::connect_blobs(net, relay, timeout, clock).await?;
    connection
        .fetch(hash)
        .await
        .map_err(|e| Error::Transport(e.to_string()))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::ports::clock::TestClock;
    use crate::ports::store::TestStore;
    use crate::ports::transport::{AcceptBlobs, TestTransport};
    use zink_protocol::{ContactRecord, DeviceKey};

    #[tokio::test]
    async fn push_blobs__should_give_up_on_a_transfer_stalled_past_its_budget() {
        // Given: a cache that admits the connection and then never
        // completes the transfer — the one wait no clock covered
        let relay = DeviceKey::from_seed([42; 32]).public();
        let spec = format!("{}@203.0.113.1:1", crate::hex::encode(&relay.0));
        let net = TestTransport::new();
        net.blobs.stall(&relay);
        let clock = TestClock::new();
        let connect = std::time::Duration::from_secs(10);
        let blob = EncryptedBlob {
            hash: BlobHash([1; 32]),
            bytes: vec![0; 64 * 1024],
        };

        // When: the push, with the clock run past the blob's budget
        let (result, ()) = tokio::join!(
            push_blobs(&net, &spec, std::slice::from_ref(&blob), connect, &clock),
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(push_deadline(blob.bytes.len(), connect));
            }
        );

        // Then: an error, not a hang — the relay path's "owed, retry later"
        assert!(matches!(result, Err(Error::Transport(ref e)) if e.contains("timed out")));
        assert_eq!(
            push_deadline(blob.bytes.len(), connect),
            connect + std::time::Duration::from_secs(2),
            "64 KiB at the 32 KiB/s floor"
        );
    }

    fn fresh(
        limits: BlobLimits,
    ) -> (
        BlobGate<TestStore>,
        ClientState<TestStore>,
        DeviceKey,
        TestTransport,
    ) {
        let state = ClientState::new(TestStore::new());
        let me = DeviceKey::from_seed([1; 32]);
        let gate = BlobGate::new(state.clone(), me.public(), limits);
        (gate, state, me, TestTransport::new())
    }

    async fn contact(state: &ClientState<TestStore>, seed: u8) -> PublicKey {
        let key = DeviceKey::from_seed([seed; 32]).public();
        state
            .save_contact(
                &format!("friend-{seed}"),
                &ContactRecord::new(vec![key], vec![], vec![]),
            )
            .await
            .expect("save contact");
        key
    }

    /// `peer` knocks through the double; the gate's answer.
    async fn knock(net: &TestTransport, gate: &BlobGate<TestStore>, peer: PublicKey) -> bool {
        let answer = net.blob_accept.inject_connect(peer);
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;
        answer.await.expect("answered")
    }

    async fn push(
        net: &TestTransport,
        gate: &BlobGate<TestStore>,
        peer: PublicKey,
        bytes: Vec<u8>,
    ) -> BlobHash {
        let hash = BlobHash(*blake3::hash(&bytes).as_bytes());
        net.blob_accept
            .inject_push(peer, EncryptedBlob { hash, bytes });
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;
        hash
    }

    async fn close(net: &TestTransport, gate: &BlobGate<TestStore>, peer: PublicKey) {
        net.blob_accept.inject_close(peer);
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;
    }

    #[tokio::test]
    async fn gate__should_admit_contacts_and_self_and_decline_strangers() {
        // Given
        let (gate, state, me, net) = fresh(BlobLimits::default());
        let friend = contact(&state, 2).await;
        let stranger = DeviceKey::from_seed([3; 32]).public();

        // When / Then: the serving gate, verbatim
        assert!(knock(&net, &gate, friend).await, "a contact may push");
        assert!(
            knock(&net, &gate, me.public()).await,
            "we may push to ourselves"
        );
        assert!(!knock(&net, &gate, stranger).await, "a stranger may not");
    }

    #[tokio::test]
    async fn gate__should_cap_open_connections_per_peer_until_one_closes() {
        // Given: one connection allowed per peer
        let limits = BlobLimits {
            max_connections_per_peer: 1,
            ..BlobLimits::default()
        };
        let (gate, state, _me, net) = fresh(limits);
        let friend = contact(&state, 4).await;

        // When: a second knock while the first is open
        assert!(knock(&net, &gate, friend).await);
        let second = knock(&net, &gate, friend).await;
        // …and again once the first closed
        close(&net, &gate, friend).await;
        let third = knock(&net, &gate, friend).await;

        // Then
        assert!(!second, "declined at the cap");
        assert!(third, "admitted once a slot freed");
    }

    #[tokio::test]
    async fn gate__should_cache_a_pushed_blob_under_the_cap_and_drop_one_over_it() {
        // Given: a 1 KiB cap
        let limits = BlobLimits {
            max_blob_bytes: 1024,
            ..BlobLimits::default()
        };
        let (gate, state, _me, net) = fresh(limits);
        let friend = contact(&state, 5).await;
        assert!(knock(&net, &gate, friend).await);

        // When
        let small = push(&net, &gate, friend, vec![7; 1024]).await;
        let large = push(&net, &gate, friend, vec![8; 1025]).await;

        // Then: the cache took exactly the one under the cap
        assert_eq!(state.load_blob(&small).await, Some(vec![7; 1024]));
        assert!(
            state.load_blob(&large).await.is_none(),
            "over the cap: dropped"
        );
    }

    #[tokio::test]
    async fn gate__should_drop_a_pushed_blob_whose_bytes_do_not_hash_as_claimed() {
        // Given: an admitted peer whose transport "verified" a lie
        let (gate, state, _me, net) = fresh(BlobLimits::default());
        let friend = contact(&state, 6).await;
        assert!(knock(&net, &gate, friend).await);
        let claimed = BlobHash([9; 32]);

        // When
        net.blob_accept.inject_push(
            friend,
            EncryptedBlob {
                hash: claimed,
                bytes: vec![1, 2, 3],
            },
        );
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;

        // Then: verify before trusting — nothing under the claimed hash
        assert!(state.load_blob(&claimed).await.is_none());
    }
}
