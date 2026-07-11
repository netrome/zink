//! Blob transfer: push encrypted blobs to relay caches (observe-confirmed —
//! iroh-blobs 0.103 pushes carry no in-band ack) and fetch + decrypt them.

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh_blobs::Hash;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesSeq, ObserveRequest, PushRequest};
use iroh_blobs::store::mem::MemStore;
use n0_future::StreamExt;
use zink_protocol::{BlobHash, EncryptedBlob};

use crate::net;

/// Push each encrypted blob to one relay's cache, confirming every transfer.
pub(crate) async fn push_blobs(
    endpoint: &Endpoint,
    relay: &str,
    staging: &MemStore,
    blobs: &[EncryptedBlob],
) -> Result<(), String> {
    let connection = net::connect(endpoint, relay, iroh_blobs::ALPN).await?;
    for blob in blobs {
        let hash = Hash::from_bytes(blob.hash.0);
        let push = PushRequest::new(hash, ChunkRangesSeq::from_ranges([ChunkRanges::all()]));
        staging
            .remote()
            .execute_push(connection.clone(), push)
            .await
            .map_err(|e| format!("push blob to {relay}: {e}"))?;
        await_blob_complete(staging, &connection, hash).await?;
    }
    Ok(())
}

/// Stage encrypted blobs in a local in-memory store, ready for pushing.
pub(crate) async fn stage(blobs: &[EncryptedBlob]) -> Result<MemStore, String> {
    let staging = MemStore::new();
    for blob in blobs {
        staging
            .add_bytes(blob.bytes.clone())
            .await
            .map_err(|e| format!("stage blob: {e}"))?;
    }
    Ok(staging)
}

/// Fetch one blob's *ciphertext* from a relay's cache. The caller verifies
/// and decrypts against the envelope that references it (`open_blob`) —
/// and may cache the ciphertext, which stays exactly as untrusted as the
/// relay it came from.
pub(crate) async fn fetch_encrypted(
    endpoint: &Endpoint,
    relay: &str,
    hash: &BlobHash,
) -> Result<Vec<u8>, String> {
    let store = MemStore::new();
    let connection = net::connect(endpoint, relay, iroh_blobs::ALPN).await?;
    let blob_hash = Hash::from_bytes(hash.0);
    store
        .remote()
        .fetch(connection, blob_hash)
        .await
        .map_err(|e| format!("fetch blob: {e}"))?;
    store
        .blobs()
        .get_bytes(blob_hash)
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| format!("read fetched blob: {e}"))
}

/// Push completion is not acknowledged in-band (iroh-blobs 0.103), so
/// confirm via an Observe request: wait until the relay reports the blob
/// complete. Returning right after the push would race the transfer.
///
/// The observe stream sends one initial bitfield and then *diffs*, so the
/// items must be accumulated — no single diff ever looks complete.
async fn await_blob_complete(
    store: &MemStore,
    connection: &Connection,
    hash: Hash,
) -> Result<(), String> {
    let mut bitfields = std::pin::pin!(
        store
            .remote()
            .observe(connection.clone(), ObserveRequest::new(hash))
    );
    let mut current = iroh_blobs::api::proto::Bitfield::empty();
    while let Some(item) = bitfields.next().await {
        let item = item.map_err(|e| format!("observe blob: {e}"))?;
        current.update(&item);
        if current.is_complete() {
            return Ok(());
        }
    }
    Err("relay never confirmed the blob upload".to_string())
}
