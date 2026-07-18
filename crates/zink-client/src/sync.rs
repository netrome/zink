//! Peer sync edge (D0): the client's *serving* side — an accepting router on
//! `SYNC_ALPN` answering `get` / `get-successors` from local storage, at the
//! peer's discretion (SPEC §5.2, `docs/design/sync-primitives.md`). This is
//! the first place the client is a server, not just a dialer. The fetching
//! side (`Client::backfill`) lives in `client`.

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use zink_protocol::{
    MAX_SYNC_REQUEST_BYTES, PublicKey, SYNC_ALPN, SyncErrorCode, SyncOp, SyncRequest, SyncResponse,
    SyncResult,
};

use crate::state::ClientState;

/// Serves history a peer asks for. Backed by a clone of the client's store —
/// reads only; a served peer is trusted no more than a relay. **Serving gate
/// (D0c): contacts-only.** Serving is discretionary (SPEC §5.2) and this is
/// the discretion: a caller whose key is not in the contact store (and isn't
/// us) gets answers indistinguishable from "don't hold it" — declining and
/// not-having look the same on the wire. Client policy, not protocol.
#[derive(Debug, Clone)]
struct SyncHandler {
    state: ClientState,
    /// Our own device key — always served (self-dial is trivially "us", and
    /// D2's own-device sync rides the same allowance).
    me: PublicKey,
}

impl SyncHandler {
    /// Contacts-only gate, resolved once per connection (the caller's key IS
    /// the authenticated connection key). A peer added as a contact
    /// mid-connection is served on its next connection.
    fn serves(&self, caller: PublicKey) -> bool {
        caller == self.me
            || self
                .state
                .contacts()
                .unwrap_or_default()
                .iter()
                .any(|(_, record)| record.keys.contains(&caller))
    }
}

impl ProtocolHandler for SyncHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let caller = PublicKey(*connection.remote_id().as_bytes());
        let serves = self.serves(caller);
        if !serves {
            tracing::debug!("sync request from a non-contact; serving nothing");
        }
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
                Some(SyncOp::Get { id }) => match serves.then(|| self.state.find_envelope(id)) {
                    Some(Some(envelope)) => SyncResult::Envelope {
                        envelope: Box::new(envelope),
                    },
                    _ => SyncResult::NotHeld,
                },
                Some(SyncOp::GetSuccessors { id }) => SyncResult::Successors {
                    ids: if serves {
                        self.state.successors(id)
                    } else {
                        Vec::new()
                    },
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
pub(crate) fn spawn_sync_router(endpoint: Endpoint, state: ClientState, me: PublicKey) -> Router {
    Router::builder(endpoint)
        .accept(SYNC_ALPN, SyncHandler { state, me })
        .spawn()
}
