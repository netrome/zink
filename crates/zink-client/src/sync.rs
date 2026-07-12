//! Peer sync edge (D0): the client's *serving* side — an accepting router on
//! `SYNC_ALPN` answering `get` / `get-successors` from local storage, at the
//! peer's discretion (SPEC §5.2, `docs/design/sync-primitives.md`). This is
//! the first place the client is a server, not just a dialer. The fetching
//! side (`Client::backfill`) lives in `client`.

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use zink_protocol::{
    MAX_SYNC_REQUEST_BYTES, SYNC_ALPN, SyncErrorCode, SyncOp, SyncRequest, SyncResponse, SyncResult,
};

use crate::state::ClientState;

/// Serves history a peer asks for. Backed by a clone of the client's store —
/// reads only; a served peer is trusted no more than a relay, and gets only
/// what we hold (discretion is client policy layered on later).
#[derive(Debug, Clone)]
struct SyncHandler {
    state: ClientState,
}

impl ProtocolHandler for SyncHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // One request per bi-stream; serve until the peer closes.
        loop {
            let Ok((mut send, mut recv)) = connection.accept_bi().await else {
                break;
            };
            let request = match recv.read_to_end(MAX_SYNC_REQUEST_BYTES).await {
                Ok(bytes) => SyncRequest::try_from_bytes(&bytes).ok(),
                Err(_) => None,
            };
            let result = match request.map(|r| r.op) {
                Some(SyncOp::Get { id }) => match self.state.find_envelope(id) {
                    Some(envelope) => SyncResult::Envelope {
                        envelope: Box::new(envelope),
                    },
                    None => SyncResult::NotHeld,
                },
                Some(SyncOp::GetSuccessors { id }) => SyncResult::Successors {
                    ids: self.state.successors(id),
                },
                None => SyncResult::Error {
                    code: SyncErrorCode::Malformed,
                },
            };
            send.write_all(&SyncResponse::new(result).to_bytes())
                .await
                .map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
        }
        Ok(())
    }
}

/// Start serving `SYNC_ALPN` on `endpoint`. The returned `Router` keeps the
/// serve loop alive for as long as the client holds it.
pub(crate) fn spawn_sync_router(endpoint: Endpoint, state: ClientState) -> Router {
    Router::builder(endpoint)
        .accept(SYNC_ALPN, SyncHandler { state })
        .spawn()
}
