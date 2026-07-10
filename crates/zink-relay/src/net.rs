//! iroh edge: serves the mailbox ALPN. Thin — extract bytes, call the
//! domain, write the response. Auth is the connection: the caller's key is
//! the connection's verified remote id.

use std::fmt;
use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use zink_protocol::{
    MAILBOX_ALPN, MAX_REQUEST_BYTES, MailboxErrorCode, MailboxRequest, MailboxResponse,
    MailboxResult, PublicKey,
};

use crate::mailbox::MailboxService;
use crate::store::MailboxStore;

/// Spawn a router serving the mailbox protocol on `endpoint`.
pub fn spawn_mailbox_router<S: MailboxStore + fmt::Debug>(
    endpoint: Endpoint,
    service: MailboxService<S>,
) -> Router {
    Router::builder(endpoint)
        .accept(MAILBOX_ALPN, MailboxHandler(Arc::new(service)))
        .spawn()
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
