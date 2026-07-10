//! iroh edge: serves the mailbox ALPN. Thin — extract bytes, call the
//! domain, write the response. Auth is the connection: the caller's key is
//! the connection's verified remote id.

use std::fmt;
use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::provider::events::{EventMask, EventSender, RequestMode};
use zink_protocol::{
    MAILBOX_ALPN, MAX_REQUEST_BYTES, MailboxErrorCode, MailboxRequest, MailboxResponse,
    MailboxResult, PublicKey,
};

use crate::mailbox::MailboxService;
use crate::store::MailboxStore;

/// Spawn a router serving the mailbox protocol and the encrypted blob cache
/// (iroh-blobs, SPEC §7) on `endpoint`.
pub fn spawn_relay_router<S: MailboxStore + fmt::Debug>(
    endpoint: Endpoint,
    service: MailboxService<S>,
    blob_store: &iroh_blobs::api::Store,
) -> Router {
    Router::builder(endpoint)
        .accept(MAILBOX_ALPN, MailboxHandler(Arc::new(service)))
        .accept(
            iroh_blobs::ALPN,
            BlobsProtocol::new(blob_store, Some(blob_cache_events())),
        )
        .spawn()
}

/// Blob-cache event config: pushes are allowed (uploaders deposit encrypted
/// blobs so recipients can fetch after the sender goes offline); no event
/// interception. Rate/size policy is a later concern (B4).
fn blob_cache_events() -> EventSender {
    let mask = EventMask {
        push: RequestMode::None,
        ..EventMask::DEFAULT
    };
    // No mode intercepts or notifies, so nothing is ever sent on the channel.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    EventSender::new(tx, mask)
}

#[derive(Debug)]
struct MailboxHandler<S>(Arc<MailboxService<S>>);

impl<S> Clone for MailboxHandler<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S: MailboxStore + fmt::Debug> ProtocolHandler for MailboxHandler<S> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let caller = PublicKey(*connection.remote_id().as_bytes());
        // One request per bi-stream; serve until the peer closes.
        loop {
            let Ok((mut send, mut recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let response = match recv.read_to_end(MAX_REQUEST_BYTES).await {
                Ok(bytes) => match MailboxRequest::try_from_bytes(&bytes) {
                    Ok(request) => self.0.handle(caller, request).await,
                    Err(_) => malformed(),
                },
                Err(_) => malformed(),
            };
            send.write_all(&response.to_bytes())
                .await
                .map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
        }
    }
}

fn malformed() -> MailboxResponse {
    MailboxResponse::new(MailboxResult::Error {
        code: MailboxErrorCode::Malformed,
    })
}
