//! Network edge helpers over the transport ports: relay dialing, mailbox
//! round-trips, retrying deposits. One request per bi-stream, per the
//! mailbox wire protocol. Each helper takes the narrowest capability it
//! exercises (`docs/design/transport.md` §6); every deadline is a
//! `Clock::timeout` race here — the ports carry no time.

use crate::error::Error;
use crate::ports::clock::Clock;
use crate::ports::transport::{Dial, DialBlobs, Peer, Request};
use zink_protocol::{
    MAILBOX_ALPN, MAX_RESPONSE_BYTES, MAX_SYNC_RESPONSE_BYTES, MailboxOp, MailboxRequest,
    MailboxResponse, MailboxResult, MessageEnvelope, SyncOp, SyncRequest, SyncResponse, SyncResult,
};

/// Bounded connect to a relay by its dial spec: an unreachable relay must
/// fail a send in bounded time, not hang it — graceful failure is what the
/// outbox turns into delivery later. The deadline is
/// `ClientConfig::connect_timeout`, injected by the edge (iroh itself keeps
/// probing an unreachable address far longer).
pub(crate) async fn connect<D: Dial>(
    net: &D,
    relay: &str,
    alpn: &[u8],
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<D::Conn, Error> {
    let to = crate::adapters::iroh::parse_dial(relay)?;
    connect_peer(net, &to, alpn, timeout, clock)
        .await
        .map_err(|e| Error::Unreachable(format!("connect to {relay}: {e}")))
}

/// Connect to an already-resolved `Peer` — used for peer sync, where a dial
/// string is parsed once and where a locally-bound peer advertises several
/// addresses (loopback/LAN/public) and iroh should try them all.
pub(crate) async fn connect_peer<D: Dial>(
    net: &D,
    to: &Peer,
    alpn: &[u8],
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<D::Conn, Error> {
    clock
        .timeout(timeout, net.dial(to, alpn))
        .await
        .map_err(|_| Error::Unreachable("timed out".to_string()))?
        .map_err(|e| Error::Unreachable(e.to_string()))
}

/// Bounded connect on the blobs ALPN, same deadline discipline as `connect`.
pub(crate) async fn connect_blobs<B: DialBlobs>(
    net: &B,
    relay: &str,
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<B::Conn, Error> {
    let to = crate::adapters::iroh::parse_dial(relay)?;
    clock
        .timeout(timeout, net.dial_blobs(&to))
        .await
        .map_err(|_| Error::Unreachable("timed out".to_string()))
        .and_then(|dialed| dialed.map_err(|e| Error::Unreachable(e.to_string())))
        .map_err(|e| Error::Unreachable(format!("connect to {relay}: {e}")))
}

/// Register at a relay, surfacing a **refusal** rather than walking past it.
/// The bare `request` returns `Ok(Error { Refused })`, which every caller
/// used to ignore before going on to fetch — draining a mailbox the relay
/// was never going to fill, forever, with no clue why. A refusal is operator
/// policy (SPEC §5.3) and terminal for this relay, so it belongs in the
/// error channel where the subscription loop and the CLI can report it.
pub(crate) async fn register(connection: &impl Request, relay: &str) -> Result<(), Error> {
    match request(connection, MailboxOp::Register).await? {
        MailboxResult::Registered => Ok(()),
        MailboxResult::Error {
            code: zink_protocol::MailboxErrorCode::Refused,
        } => Err(Error::MailboxRefused(relay.to_string())),
        other => Err(Error::UnexpectedResponse(format!(
            "register at {relay}: {other:?}"
        ))),
    }
}

pub(crate) async fn request(
    connection: &impl Request,
    op: MailboxOp,
) -> Result<MailboxResult, Error> {
    let bytes = connection
        .request(&MailboxRequest::new(op).to_bytes(), MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    Ok(MailboxResponse::try_from_bytes(&bytes)
        .map_err(Error::Decode)?
        .result)
}

/// One peer sync round-trip on `SYNC_ALPN` (same one-request-per-bi-stream
/// framing as the mailbox). The connection is to a *peer*, not a relay.
pub(crate) async fn sync_request(
    connection: &impl Request,
    op: SyncOp,
) -> Result<SyncResult, Error> {
    let bytes = connection
        .request(&SyncRequest::new(op).to_bytes(), MAX_SYNC_RESPONSE_BYTES)
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    Ok(SyncResponse::try_from_bytes(&bytes)
        .map_err(Error::Decode)?
        .result)
}

/// Deposit with a fresh connection per attempt. Deposits dedup by message
/// id on the relay, so retrying after a transport error is always safe.
/// An *unreachable* relay is not retried here at all — that won't heal in
/// seconds, and healing over time is the outbox's job (live-delivery.md §2);
/// in-attempt retries are for transient post-connect stream errors only.
pub(crate) async fn deposit_with_retry<D: Dial>(
    net: &D,
    relay: &str,
    envelope: &MessageEnvelope,
    timeout: std::time::Duration,
    clock: &impl Clock,
) -> Result<(), Error> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tracing::warn!(relay, attempt, error = %last_error, "deposit failed; retrying");
        }
        let connection = match connect(net, relay, MAILBOX_ALPN, timeout, clock).await {
            Ok(connection) => connection,
            Err(error) => return Err(error),
        };
        let deposit = MailboxOp::Deposit {
            envelope: Box::new(envelope.clone()),
        };
        match request(&connection, deposit).await {
            Ok(MailboxResult::Deposited { .. }) => return Ok(()),
            Ok(other) => {
                return Err(Error::UnexpectedResponse(format!(
                    "from {relay}: {other:?}"
                )));
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(Error::Transport(format!(
        "deposit to {relay} failed after 3 attempts: {last_error}"
    )))
}
