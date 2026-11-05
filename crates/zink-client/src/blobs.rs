//! Blob transfer over the transport ports: push encrypted blobs to relay
//! caches and to recipient devices (the port's `push` resolves only on
//! confirmed receipt), fetch their ciphertext back, and — the recipient
//! half of peer delivery (project 10 phase B) — take what contacts push to
//! us. The iroh-blobs mechanics live in the adapter.

use std::collections::BTreeMap;
use std::sync::Mutex;

use zink_protocol::{BlobHash, EncryptedBlob, PublicKey};

use crate::error::Error;
use crate::net;
use crate::ports::clock::Clock;
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
pub(crate) struct BlobGate {
    state: ClientState,
    me: PublicKey,
    limits: BlobLimits,
    /// Admitted connections open per peer.
    open: Mutex<BTreeMap<[u8; 32], usize>>,
}

impl BlobGate {
    pub(crate) fn new(state: ClientState, me: PublicKey, limits: BlobLimits) -> Self {
        Self {
            state,
            me,
            limits,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn handle<A: Admit>(&self, event: BlobEvent<A>) {
        match event {
            BlobEvent::Connected { peer, admit } => admit.admit(self.admits(peer)).await,
            BlobEvent::Pushed { peer, blob } => self.take(peer, &blob),
            BlobEvent::Closed { peer } => self.closed(peer),
        }
    }

    /// The admission decision, counted when positive.
    fn admits(&self, peer: PublicKey) -> bool {
        if !crate::sync::serves(&self.state, self.me, peer) {
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

    fn take(&self, peer: PublicKey, blob: &EncryptedBlob) {
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
        if let Err(error) = self.state.save_blob(&blob.hash, &blob.bytes) {
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
pub(crate) async fn serve_pushes<A: AcceptBlobs>(accept: A, gate: BlobGate) {
    while let Some(event) = accept.accept_blob().await {
        gate.handle(event).await;
    }
}

/// Push each encrypted blob to one relay's cache, confirming every transfer.
pub(crate) async fn push_blobs<B: DialBlobs>(
    net: &B,
    relay: &str,
    blobs: &[EncryptedBlob],
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<(), Error> {
    let connection = net::connect_blobs(net, relay, timeout, clock).await?;
    for blob in blobs {
        connection
            .push(blob)
            .await
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
    use crate::ports::transport::{AcceptBlobs, TestTransport};
    use zink_protocol::{ContactRecord, DeviceKey};

    fn fresh(test: &str, limits: BlobLimits) -> (BlobGate, ClientState, DeviceKey, TestTransport) {
        let root = std::env::temp_dir().join(format!("zink-test-gate-{test}"));
        let _ = std::fs::remove_dir_all(&root);
        let key_path = root.join("me.key").to_string_lossy().to_string();
        let state = ClientState::open(&key_path);
        let me = DeviceKey::from_seed([1; 32]);
        let gate = BlobGate::new(state.clone(), me.public(), limits);
        (gate, state, me, TestTransport::new())
    }

    fn contact(state: &ClientState, seed: u8) -> PublicKey {
        let key = DeviceKey::from_seed([seed; 32]).public();
        state
            .save_contact(
                &format!("friend-{seed}"),
                &ContactRecord::new(vec![key], vec![], vec![]),
            )
            .expect("save contact");
        key
    }

    /// `peer` knocks through the double; the gate's answer.
    async fn knock(net: &TestTransport, gate: &BlobGate, peer: PublicKey) -> bool {
        let answer = net.blob_accept.inject_connect(peer);
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;
        answer.await.expect("answered")
    }

    async fn push(
        net: &TestTransport,
        gate: &BlobGate,
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

    async fn close(net: &TestTransport, gate: &BlobGate, peer: PublicKey) {
        net.blob_accept.inject_close(peer);
        let event = net.accept_blob().await.expect("event");
        gate.handle(event).await;
    }

    #[tokio::test]
    async fn gate__should_admit_contacts_and_self_and_decline_strangers() {
        // Given
        let (gate, state, me, net) = fresh("admit", BlobLimits::default());
        let friend = contact(&state, 2);
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
        let (gate, state, _me, net) = fresh("cap", limits);
        let friend = contact(&state, 4);

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
        let (gate, state, _me, net) = fresh("size", limits);
        let friend = contact(&state, 5);
        assert!(knock(&net, &gate, friend).await);

        // When
        let small = push(&net, &gate, friend, vec![7; 1024]).await;
        let large = push(&net, &gate, friend, vec![8; 1025]).await;

        // Then: the cache took exactly the one under the cap
        assert_eq!(state.load_blob(&small), Some(vec![7; 1024]));
        assert!(state.load_blob(&large).is_none(), "over the cap: dropped");
    }

    #[tokio::test]
    async fn gate__should_drop_a_pushed_blob_whose_bytes_do_not_hash_as_claimed() {
        // Given: an admitted peer whose transport "verified" a lie
        let (gate, state, _me, net) = fresh("hash", BlobLimits::default());
        let friend = contact(&state, 6);
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
        assert!(state.load_blob(&claimed).is_none());
    }
}
