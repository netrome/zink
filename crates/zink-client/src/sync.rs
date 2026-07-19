//! Peer sync edge (D0, D1a): the client's *serving* side — an accepting
//! router on `SYNC_ALPN` answering `get` / `get-successors` from local
//! storage and `who-is` from the contact store, at the peer's discretion
//! (SPEC §5.2/§3.5, `docs/design/sync-primitives.md`,
//! `docs/design/who-is-this.md`). This is the first place the client is a
//! server, not just a dialer. The fetching side (`Client::backfill`) lives
//! in `client`.

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use zink_protocol::{
    ContactRecord, DeviceKey, MAX_SYNC_REQUEST_BYTES, PublicKey, SYNC_ALPN, SyncErrorCode, SyncOp,
    SyncRequest, SyncResponse, SyncResult,
};

use crate::state::ClientState;

/// Serves history a peer asks for. Backed by a clone of the client's store —
/// reads only; a served peer is trusted no more than a relay. **Serving gate
/// (D0c): contacts-only.** Serving is discretionary (SPEC §5.2) and this is
/// the discretion: a caller whose key is not in the contact store (and isn't
/// us) gets answers indistinguishable from "don't hold it" — declining and
/// not-having look the same on the wire. Client policy, not protocol.
struct SyncHandler {
    state: ClientState,
    /// This device's key: identifies "us" for the gate's self-allowance
    /// (self-dial is trivially "us"; D3 own-device sync rides the same
    /// allowance) and signs the fresh self-record served for a `WhoIs`
    /// about our own key (D1a).
    device: DeviceKey,
}

/// Hand-written because `DeviceKey` is secret material — deliberately
/// neither `Clone` nor `Debug`; it must never reach log output.
impl std::fmt::Debug for SyncHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHandler")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl SyncHandler {
    /// Contacts-only gate, resolved once per connection (the caller's key IS
    /// the authenticated connection key). A peer added as a contact
    /// mid-connection is served on its next connection. The self-allowance
    /// extends to recognized own devices (D3b, multi-device.md §6) — and
    /// only to the *vouched* key of each: extra keys a device record lists
    /// never widen the gate. Local state only; nothing wire-borne.
    fn serves(&self, caller: PublicKey) -> bool {
        caller == self.device.public()
            || self
                .state
                .recognized_devices()
                .iter()
                .any(|(key, _)| *key == caller)
            || self
                .state
                .contacts()
                .unwrap_or_default()
                .iter()
                .any(|(_, record)| record.keys.contains(&caller))
    }

    /// The record served for `WhoIs { subject }` (who-is-this.md §4): the
    /// fresh self-record for our own key (`None` — indistinguishable from
    /// not-holding — while the profile is incomplete), a **recognized own
    /// device's** stored record (the D3b mirror rule, multi-device.md §6:
    /// recognizing a device is a willingness to advertise it — and nobody
    /// else can serve a new device's record), else a *user-added* contact's
    /// stored record, as stored. Learned records (D1b) are never re-served
    /// — hop limit 1 is structural — and a contact-store read error fails
    /// closed, like the gate.
    fn who_is(&self, subject: PublicKey) -> Option<ContactRecord> {
        if subject == self.device.public() {
            return crate::client::build_own_record(&self.device, &self.state);
        }
        if let Some((_, record)) = self
            .state
            .recognized_devices()
            .into_iter()
            .find(|(key, _)| *key == subject)
        {
            return Some(record);
        }
        self.state
            .contacts()
            .ok()?
            .into_iter()
            .find(|(_, record)| record.keys.contains(&subject))
            .map(|(_, record)| record)
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
                Some(SyncOp::WhoIs { key }) => match serves.then(|| self.who_is(key)).flatten() {
                    Some(record) => SyncResult::Known {
                        record: Box::new(record),
                    },
                    None => SyncResult::NotHeld,
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
pub(crate) fn spawn_sync_router(
    endpoint: Endpoint,
    state: ClientState,
    device: DeviceKey,
) -> Router {
    Router::builder(endpoint)
        .accept(SYNC_ALPN, SyncHandler { state, device })
        .spawn()
}
