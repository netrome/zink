//! iroh edge: serves the mailbox ALPN. Thin — extract bytes, call the
//! domain, write the response. Auth is the connection: the caller's key is
//! the connection's verified remote id.
//!
//! Also owns the **nudge** (live-delivery.md §3): a map of live registered
//! connections, and a zero-length uni stream to each hosted recipient on
//! deposit. Transport-level by nature, so it lives here and never enters
//! `MailboxService`.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::provider::events::{EventMask, EventSender, ProviderMessage, RequestMode};
use zink_protocol::{
    MAILBOX_ALPN, MAX_REQUEST_BYTES, MailboxErrorCode, MailboxOp, MailboxRequest, MailboxResponse,
    MailboxResult, PublicKey,
};

use crate::blobs::push_tag;
use crate::clock::WallClock;
use crate::mailbox::MailboxService;
use crate::store::MailboxStore;

/// Spawn a router serving the mailbox protocol and the encrypted blob cache
/// (iroh-blobs, SPEC §7) on `endpoint`. Pushes are allowed (uploaders
/// deposit encrypted blobs so recipients can fetch after the sender goes
/// offline); each push writes a timestamped retention tag that bounds the
/// blob's lifetime (see `blobs`).
pub fn spawn_relay_router<S: MailboxStore + fmt::Debug, W: WallClock>(
    endpoint: Endpoint,
    service: MailboxService<S>,
    blob_store: &iroh_blobs::api::Store,
    wall_clock: W,
) -> Router {
    Router::builder(endpoint)
        .accept(
            MAILBOX_ALPN,
            MailboxHandler {
                service: Arc::new(service),
                live: Arc::default(),
            },
        )
        .accept(
            iroh_blobs::ALPN,
            BlobsProtocol::new(
                blob_store,
                Some(blob_cache_events(blob_store.clone(), wall_clock)),
            ),
        )
        .spawn()
}

/// Event config for the blob cache: each push notification writes the
/// retention tag. iroh-blobs 0.103 gates *every* request type on `mask.get`
/// (upstream quirk), so `get` carries the Notify mode that push needs;
/// `push` is set to the same for when upstream fixes the dispatch.
fn blob_cache_events<W: WallClock>(
    blob_store: iroh_blobs::api::Store,
    wall_clock: W,
) -> EventSender {
    let mask = EventMask {
        get: RequestMode::Notify,
        push: RequestMode::Notify,
        ..EventMask::DEFAULT
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let ProviderMessage::PushRequestReceivedNotify(msg) = message {
                let hash = msg.inner.request.hash;
                let tag = push_tag(wall_clock.now_ms(), &hash);
                // If tagging fails the blob is stored but unprotected — the
                // next GC would delete a blob the uploader believes it
                // delivered. Log loudly; the sender may re-push (idempotent
                // by hash) to re-tag. A blocking in-process channel makes
                // this rare, but silence would be a silent delivery hole.
                if let Err(e) = blob_store.tags().set(tag, hash).await {
                    tracing::warn!(%hash, error = %e, "failed to tag pushed blob");
                }
            }
        }
    });
    EventSender::new(tx, mask)
}

/// The nudge must not wedge on a peer that never accepts uni streams (a
/// pre-nudge client eventually exhausts its stream credit): bounded, and
/// spawned so the depositor's request loop never waits on it either.
const NUDGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Live registered connections. A key can hold **several at once** — a
/// device's long-lived subscription *and* its short-lived poll connections
/// coexist (both `Register`). Keeping only one (newest-wins) let a poll's
/// throwaway connection evict the subscription from the nudge path when it
/// closed, so nudges silently fell back to the poll. Nudge every live
/// connection for a recipient; a closing connection removes only its own
/// session. Sessions are numbered so cleanup never touches another's entry.
#[derive(Debug, Default)]
struct LiveConnections {
    next_session: u64,
    by_key: HashMap<PublicKey, HashMap<u64, Connection>>,
}

#[derive(Debug)]
struct MailboxHandler<S> {
    service: Arc<MailboxService<S>>,
    live: Arc<Mutex<LiveConnections>>,
}

impl<S> Clone for MailboxHandler<S> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            live: self.live.clone(),
        }
    }
}

impl<S: MailboxStore + fmt::Debug> ProtocolHandler for MailboxHandler<S> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let caller = PublicKey(*connection.remote_id().as_bytes());
        let mut session = None;
        // One request per bi-stream; serve until the peer closes.
        loop {
            let Ok((mut send, mut recv)) = connection.accept_bi().await else {
                break;
            };
            let request = match recv.read_to_end(MAX_REQUEST_BYTES).await {
                Ok(bytes) => MailboxRequest::try_from_bytes(&bytes).ok(),
                Err(_) => None,
            };
            // Recipients to nudge, noted before the request moves on.
            let deposited_for: BTreeSet<PublicKey> = match &request {
                Some(MailboxRequest {
                    op: MailboxOp::Deposit { envelope },
                    ..
                }) => envelope.core.recipients.iter().copied().collect(),
                _ => BTreeSet::new(),
            };
            let response = match request {
                Some(request) => self.service.handle(caller, request).await,
                None => malformed(),
            };
            match response.result {
                // A registered, still-connected peer is "live": deliveries
                // for it get a nudge instead of waiting for its next poll.
                MailboxResult::Registered => {
                    let mut live = self.live.lock().expect("live map lock");
                    // One session per connection: assign on first register,
                    // reuse if this connection registers again.
                    let this_session = *session.get_or_insert_with(|| {
                        live.next_session += 1;
                        live.next_session
                    });
                    live.by_key
                        .entry(caller)
                        .or_default()
                        .insert(this_session, connection.clone());
                }
                MailboxResult::Deposited { .. } => {
                    for recipient in &deposited_for {
                        let targets: Vec<Connection> = {
                            let live = self.live.lock().expect("live map lock");
                            live.by_key
                                .get(recipient)
                                .map(|conns| conns.values().cloned().collect())
                                .unwrap_or_default()
                        };
                        let short = &hex_short(recipient);
                        if targets.is_empty() {
                            tracing::debug!(
                                recipient = short,
                                "deposit for a recipient with no live connection (will poll)"
                            );
                        } else {
                            tracing::debug!(
                                recipient = short,
                                connections = targets.len(),
                                "nudging live recipient"
                            );
                            for target in targets {
                                nudge(target);
                            }
                        }
                    }
                }
                _ => {}
            }
            send.write_all(&response.to_bytes())
                .await
                .map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
        }
        // Connection gone — drop only *this* connection's session; other
        // live connections for the same key (e.g. its subscription) stay.
        if let Some(session) = session {
            let mut live = self.live.lock().expect("live map lock");
            if let Some(conns) = live.by_key.get_mut(&caller) {
                conns.remove(&session);
                if conns.is_empty() {
                    live.by_key.remove(&caller);
                }
            }
        }
        Ok(())
    }
}

/// Fire one nudge: a zero-length uni stream — the stream itself is the
/// signal (live-delivery.md §3). Best-effort by design: the mailbox holds
/// the envelope and fetch-on-foreground remains the backstop, so failures
/// are ignored.
fn nudge(connection: Connection) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(NUDGE_TIMEOUT, async {
            if let Ok(mut stream) = connection.open_uni().await {
                let _ = stream.finish();
            }
        })
        .await;
    });
}

fn malformed() -> MailboxResponse {
    MailboxResponse::new(MailboxResult::Error {
        code: MailboxErrorCode::Malformed,
    })
}

/// First 8 hex chars of a key — enough to follow a recipient in the logs.
fn hex_short(key: &PublicKey) -> String {
    key.0.iter().take(4).map(|b| format!("{b:02x}")).collect()
}
