//! Blob transfer over the transport ports: push encrypted blobs to relay
//! caches (the port's `push` resolves only on confirmed receipt) and fetch
//! their ciphertext back. The iroh-blobs mechanics live in the adapter.

use zink_protocol::{BlobHash, EncryptedBlob};

use crate::error::Error;
use crate::net;
use crate::ports::clock::Clock;
use crate::ports::transport::{DialBlobs, FetchBlob, PushBlob};

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
