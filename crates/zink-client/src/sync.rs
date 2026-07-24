//! Peer sync edge (D0, D1a): the client's *serving* side — an accepting
//! router on `SYNC_ALPN` answering `get` / `get-successors` from local
//! storage and `who-is` from the contact store, at the peer's discretion
//! (SPEC §5.2/§3.5, `docs/design/sync-primitives.md`,
//! `docs/design/who-is-this.md`). This is the first place the client is a
//! server, not just a dialer. The fetching side (`Client::backfill`) lives
//! in `client`.
//!
//! Since D5 it also *accepts* messages: `Deliver` makes this device its own
//! mailbox while it is online (`docs/design/direct-delivery.md`).

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use rand_core::OsRng;
use zink_protocol::{
    ContactRecord, DeviceKey, FORMAT_VERSION, MAX_GET_KEYS_IDS, MAX_SYNC_REQUEST_BYTES,
    MessageEnvelope, MessageId, PublicKey, SYNC_ALPN, SyncErrorCode, SyncOp, SyncRequest,
    SyncResponse, SyncResult,
};

use crate::client::Received;
use crate::state::ClientState;

/// Where a directly-delivered message goes after it is stored (D5): the
/// edge's live-delivery sink, registered once via
/// `Client::on_direct_delivery`. `OnceLock` because the serving router is
/// spawned during `open` — before any edge could hand over a callback — and
/// because a second registration would silently shadow the first. Absent
/// (never registered), a direct arrival is still stored; only the live
/// notification is missed, and the next drain surfaces it.
pub(crate) type DirectSink =
    std::sync::Arc<std::sync::OnceLock<Box<dyn Fn(Vec<Received>) + Send + Sync>>>;

/// What the send side knows about reaching a peer directly (D5). Shared with
/// the router because an **inbound** connection is the cheapest evidence
/// there is that a path exists — and evidence is what licenses a send to
/// spend real time on a direct dial instead of just using the mailbox.
///
/// In memory on purpose (like `Client::queried`): reachability is a fact
/// about *now*, so a fresh process starts from "don't know" rather than from
/// a stale opinion.
pub(crate) type ReachMap =
    std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<[u8; 32], Reach>>>;

/// Wall-clock ms (0 = never). Copy-small; the map is rewritten in place.
#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct Reach {
    /// Last evidence the peer is reachable: a delivery it took, a push it
    /// declined (declining still proves we reached it), or a connection it
    /// opened to us.
    pub seen_ms: u64,
    /// Last dial that got nowhere — suppresses re-dialing for a cooldown, so
    /// a recipient that is simply offline costs a send nothing.
    pub failed_ms: u64,
}

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
    /// Where an accepted `Deliver` is announced (D5).
    sink: DirectSink,
    /// Peers that reached us are reachable (D5) — the send side reads this.
    reach: ReachMap,
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

    /// Serve a `GetKeys` re-wrap batch (D3d, multi-device.md §6).
    /// **Narrower than the gate**: recognized own devices only at D3 — a
    /// contact's request declines as `NotHeld`, indistinguishable from not
    /// holding anything (SPEC §5.2 keeps "willingness to re-wrap" at its
    /// narrowest until the recovery flows need more). Per id, capped: held
    /// envelope + a wrap this device can open → a fresh wrap sealed to the
    /// caller's connection key; misses are simply absent.
    fn get_keys(&self, caller: PublicKey, ids: &[MessageId]) -> SyncResult {
        let own_device = self
            .state
            .recognized_devices()
            .iter()
            .any(|(key, _)| *key == caller);
        if !own_device {
            return SyncResult::NotHeld;
        }
        let wraps = ids
            .iter()
            .take(MAX_GET_KEYS_IDS)
            .filter_map(|&id| {
                let envelope = self.state.find_envelope(id)?;
                let wrap = envelope.rewrap(&self.device, caller, &mut OsRng).ok()?;
                Some((id, wrap))
            })
            .collect();
        SyncResult::Wraps { wraps }
    }

    /// Accept a directly-delivered envelope (D5, direct-delivery.md §3–4).
    ///
    /// **No new trust.** A dialer is trusted no more than a relay: the
    /// envelope must carry the version this client speaks, hash-and-signature
    /// verify, and actually be addressed to us — otherwise a contact could
    /// push arbitrary history into our store, which not even our own relay
    /// can do (it indexes deposits per recipient key). The body must open
    /// too, mirroring the mailbox drain: an envelope we can't read is either
    /// a wrap bug or hostile, and storing it either way is a spam sink.
    ///
    /// **`Stored` means durably stored.** The sender skips its mailbox
    /// deposit on this ack (§3), so returning it before the write lands
    /// would lose the message with no fallback copy. Every decline is a bare
    /// `NotHeld` — the sender only needs "not stored", and a peer's reasons
    /// are nobody else's business (SPEC §5.2).
    fn accept_delivery(&self, serves: bool, envelope: MessageEnvelope) -> SyncResult {
        if !serves {
            // Gate as for history (D0c): a stranger's push falls back to
            // their mailbox deposit, where the relay's caps (C0) and the
            // parked quarantine view are the policy for unknown senders.
            tracing::debug!("declining a direct delivery from a non-contact");
            return SyncResult::NotHeld;
        }
        if envelope.version != FORMAT_VERSION || envelope.core.version != FORMAT_VERSION {
            tracing::warn!("declining a direct delivery with an unsupported version");
            return SyncResult::NotHeld;
        }
        if envelope.verify().is_err() {
            tracing::warn!("declining an unverifiable direct delivery");
            return SyncResult::NotHeld;
        }
        if !envelope.core.recipients.contains(&self.device.public()) {
            tracing::warn!("declining a direct delivery not addressed to us");
            return SyncResult::NotHeld;
        }
        let body = envelope.open(&self.device);
        if body.is_err() {
            tracing::warn!("declining a direct delivery this device cannot open");
            return SyncResult::NotHeld;
        }
        // Idempotent: a re-delivery of something already held rewrites the
        // same bytes and acks again, exactly like a repeated deposit.
        if let Err(error) = crate::client::remember(&self.state, &envelope) {
            tracing::warn!(%error, "direct delivery failed to store; declining");
            return SyncResult::NotHeld;
        }
        tracing::info!("stored a direct delivery");
        if let Some(sink) = self.sink.get() {
            sink(vec![Received {
                envelope,
                // No relay was involved — blobs resolve through our own
                // home relays' caches, where the sender pushed them.
                relay: None,
                body,
            }]);
        }
        SyncResult::Stored
    }
}

impl ProtocolHandler for SyncHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let caller = PublicKey(*connection.remote_id().as_bytes());
        let serves = self.serves(caller);
        if !serves {
            tracing::debug!("sync request from a non-contact; serving nothing");
        } else if let Ok(mut reach) = self.reach.lock() {
            // A peer that reached us can be reached (D5): our next send to it
            // is worth a real dial budget rather than a token probe. Only for
            // peers we serve — a stranger's connection is not our business.
            reach.entry(caller.0).or_default().seen_ms = crate::client::now_ms();
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
                        // This device's OWN issued claims about the
                        // subject (D4a) — never anything learned or
                        // relayed: hop limit 1 is structural.
                        endorsements: self.state.vouch_for(&key).into_iter().collect(),
                    },
                    None => SyncResult::NotHeld,
                },
                Some(SyncOp::GetKeys { ids }) => self.get_keys(caller, &ids),
                Some(SyncOp::Deliver { envelope }) => self.accept_delivery(serves, *envelope),
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
    sink: DirectSink,
    reach: ReachMap,
) -> Router {
    Router::builder(endpoint)
        .accept(
            SYNC_ALPN,
            SyncHandler {
                state,
                device,
                sink,
                reach,
            },
        )
        .spawn()
}
