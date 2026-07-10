//! B3: the relay blob cache over real iroh connections — push, fetch,
//! and dedup by hash.

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr};
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesSeq, ObserveRequest, PushRequest};
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;
use zink_relay::blobs::{BlobRetention, DEFAULT_BLOB_TTL, blob_cache};
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

async fn spawn_relay_with(
    ttl: Duration,
    gc_interval: Duration,
) -> (iroh::protocol::Router, MemStore, EndpointAddr) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .expect("bind relay endpoint");
    let addr = endpoint.addr();
    let retention = Arc::new(BlobRetention::new(ttl));
    let blob_store = blob_cache(retention.clone(), gc_interval);
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
        retention,
    );
    (router, blob_store, addr)
}

async fn spawn_relay() -> (iroh::protocol::Router, MemStore, EndpointAddr) {
    spawn_relay_with(DEFAULT_BLOB_TTL, Duration::from_secs(3600)).await
}

async fn client() -> (Endpoint, MemStore) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .expect("bind client endpoint");
    (endpoint, MemStore::new())
}

async fn push(
    client: &(Endpoint, MemStore),
    relay: &EndpointAddr,
    bytes: &[u8],
) -> iroh_blobs::Hash {
    let (endpoint, store) = client;
    let tag = store
        .add_bytes(bytes.to_vec())
        .await
        .expect("stage blob locally");
    let connection = endpoint
        .connect(relay.clone(), iroh_blobs::ALPN)
        .await
        .expect("connect to relay blobs");
    store
        .remote()
        .execute_push(
            connection.clone(),
            PushRequest::new(tag.hash, ChunkRangesSeq::from_ranges([ChunkRanges::all()])),
        )
        .await
        .expect("push blob");
    // Push completion is not acknowledged in-band; observe until the relay
    // reports the blob complete (initial bitfield + diffs — accumulate).
    let mut bitfields = std::pin::pin!(
        store
            .remote()
            .observe(connection, ObserveRequest::new(tag.hash))
    );
    let mut current = iroh_blobs::api::proto::Bitfield::empty();
    while let Some(item) = bitfields.next().await {
        current.update(&item.expect("observe blob"));
        if current.is_complete() {
            break;
        }
    }
    tag.hash
}

#[tokio::test]
#[allow(non_snake_case)]
async fn blob_cache__should_serve_pushed_blobs_and_dedup_by_hash() {
    // Given: a relay, and one encrypted blob pushed by two different senders
    let (_router, relay_store, relay_addr) = spawn_relay().await;
    let blob = b"opaque encrypted bytes".to_vec();
    let uploader_1 = client().await;
    let uploader_2 = client().await;
    let hash = push(&uploader_1, &relay_addr, &blob).await;
    let hash_again = push(&uploader_2, &relay_addr, &blob).await;

    // Then: content addressing collapses the two pushes into one blob
    assert_eq!(hash, hash_again);
    let hashes = relay_store
        .blobs()
        .list()
        .hashes()
        .await
        .expect("list relay blobs");
    assert_eq!(hashes, vec![hash]);

    // And: a third party fetches the bytes back intact
    let (endpoint, store) = client().await;
    let connection = endpoint
        .connect(relay_addr, iroh_blobs::ALPN)
        .await
        .expect("connect to relay blobs");
    store
        .remote()
        .fetch(connection, hash)
        .await
        .expect("fetch blob");
    let fetched = store.blobs().get_bytes(hash).await.expect("read blob");
    assert_eq!(fetched.as_ref(), blob.as_slice());
}

#[tokio::test]
#[allow(non_snake_case)]
async fn blob_cache__should_evict_blobs_past_the_ttl() {
    // Given: a cache with a very short TTL and fast GC, holding one blob
    let (_router, relay_store, relay_addr) =
        spawn_relay_with(Duration::from_millis(100), Duration::from_millis(50)).await;
    let uploader = client().await;
    let hash = push(&uploader, &relay_addr, b"short-lived").await;
    assert_eq!(
        relay_store.blobs().list().hashes().await.expect("list"),
        vec![hash]
    );

    // When: the TTL and at least one GC run pass
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Then: the blob is gone
    assert!(
        relay_store
            .blobs()
            .list()
            .hashes()
            .await
            .expect("list")
            .is_empty()
    );
}
