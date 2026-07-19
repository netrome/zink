//! Network edge helpers: relay dialing, mailbox round-trips, retrying
//! deposits. One request per bi-stream, per the mailbox wire protocol.

use std::net::SocketAddr;
use std::str::FromStr;

use crate::error::Error;
use iroh::endpoint::{Connection, presets};
use iroh::tls::CaTlsConfig;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
};
use zink_protocol::{
    DeviceKey, MAILBOX_ALPN, MAX_RESPONSE_BYTES, MAX_SYNC_RESPONSE_BYTES, MailboxOp,
    MailboxRequest, MailboxResponse, MailboxResult, MessageEnvelope, PublicKey, SyncOp,
    SyncRequest, SyncResponse, SyncResult,
};

/// The endpoint key IS the device key: mailbox auth is the connection.
///
/// `home_relays` are the iroh relay URLs of this device's own relay services
/// (D0b): with any set, the endpoint homes to them (`RelayMode::Custom`) and
/// stays reachable by key across NATs — iroh holepunches to a direct path
/// when it can and falls back to relaying the (encrypted) QUIC when it
/// can't. With none set, the endpoint is dial-only + directly-dialable,
/// exactly as before; relay changes take effect on the next bind (app
/// restart) — iroh's relay transport is fixed at bind time.
pub(crate) async fn bind_endpoint(
    device: &DeviceKey,
    home_relays: &[RelayUrl],
) -> Result<Endpoint, Error> {
    let mut builder =
        Endpoint::builder(presets::Minimal).secret_key(SecretKey::from_bytes(&device.seed()));
    if !home_relays.is_empty() {
        let map: RelayMap = home_relays.iter().cloned().map(relay_config).collect();
        builder = builder
            .relay_mode(RelayMode::Custom(map))
            // The relay serves QAD with a self-signed cert (De2) — webpki
            // roots would put a CA in the trust path, which zink relays
            // deliberately don't have. Nothing security-relevant rides on
            // this TLS: iroh connections authenticate by endpoint key, and
            // a QAD man-in-the-middle can at most misreport our observed
            // address (degraded holepunching — today's baseline anyway).
            .ca_tls_config(CaTlsConfig::insecure_skip_verify());
    }
    builder
        .bind()
        .await
        .map_err(|e| Error::Transport(format!("bind endpoint: {e}")))
}

/// One home relay's client-side config. Same-port convention (De2): the
/// relay serves QUIC address discovery on UDP at the relay URL's own port
/// number (TCP for HTTP relaying and UDP for QAD coexist at one number, and
/// distinct URLs get distinct QAD ports — multi-relay on one host stays
/// collision-free). A URL with no explicit port keeps iroh's default QAD
/// port (7842), which is exactly the convention standard iroh relays use.
fn relay_config(url: RelayUrl) -> RelayConfig {
    let port = url.port();
    let mut config = RelayConfig::from(url);
    if let (Some(port), Some(quic)) = (port, config.quic.as_mut()) {
        quic.port = port;
    }
    config
}

/// Parse an iroh relay URL from a `RelayEntry.relay_url` value.
pub(crate) fn parse_relay_url(url: &str) -> Result<RelayUrl, Error> {
    RelayUrl::from_str(url).map_err(|e| Error::InvalidInput(format!("relay url {url}: {e}")))
}

/// A peer address from its device key + its relay URLs (from its
/// ContactRecord): iroh routes initial signaling via the peer's relay, then
/// holepunches to a direct path or falls back to relaying. The device key
/// IS the endpoint key, so no lookup service is involved.
pub(crate) fn peer_addr(key: &PublicKey, relay_urls: &[RelayUrl]) -> Result<EndpointAddr, Error> {
    let id = EndpointId::from_bytes(&key.0)
        .map_err(|e| Error::InvalidInput(format!("peer endpoint id: {e}")))?;
    let mut addr = EndpointAddr::new(id);
    for url in relay_urls {
        addr = addr.with_relay_url(url.clone());
    }
    Ok(addr)
}

/// `<endpoint-id>@<ip:port>`, as printed by `zink-relay`. Tolerates the
/// full relay spec `<endpoint-id>@<ip:port>#<relay-url>` — mailbox dialing
/// only needs the part before the `#`.
pub(crate) fn parse_relay(spec: &str) -> Result<EndpointAddr, Error> {
    let spec = spec.split_once('#').map_or(spec, |(dial, _)| dial);
    let (id, sock) = spec
        .split_once('@')
        .ok_or_else(|| Error::InvalidInput("relay must be <endpoint-id>@<ip:port>".into()))?;
    let id = EndpointId::from_str(id)
        .map_err(|e| Error::InvalidInput(format!("relay endpoint id: {e}")))?;
    let sock = SocketAddr::from_str(sock)
        .map_err(|e| Error::InvalidInput(format!("relay socket addr: {e}")))?;
    Ok(EndpointAddr::new(id).with_ip_addr(sock))
}

/// Bounded connect: an unreachable relay must fail a send in bounded time,
/// not hang it — graceful failure is what the outbox turns into delivery
/// later. The deadline is `ClientConfig::connect_timeout`, injected by the
/// edge (iroh itself keeps probing an unreachable address far longer).
pub(crate) async fn connect(
    endpoint: &Endpoint,
    relay: &str,
    alpn: &[u8],
    timeout: std::time::Duration,
) -> Result<Connection, Error> {
    connect_addr(endpoint, parse_relay(relay)?, alpn, timeout)
        .await
        .map_err(|e| Error::Unreachable(format!("connect to {relay}: {e}")))
}

/// Connect to an already-resolved `EndpointAddr` — used for peer sync, where a
/// dial string is parsed once and where a locally-bound peer advertises
/// several addresses (loopback/LAN/public) and iroh should try them all.
pub(crate) async fn connect_addr(
    endpoint: &Endpoint,
    addr: EndpointAddr,
    alpn: &[u8],
    timeout: std::time::Duration,
) -> Result<Connection, Error> {
    n0_future::time::timeout(timeout, endpoint.connect(addr, alpn))
        .await
        .map_err(|_| Error::Unreachable("timed out".to_string()))?
        .map_err(|e| Error::Unreachable(e.to_string()))
}

pub(crate) async fn request(
    connection: &Connection,
    op: MailboxOp,
) -> Result<MailboxResult, Error> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| Error::Transport(format!("open stream: {e}")))?;
    send.write_all(&MailboxRequest::new(op).to_bytes())
        .await
        .map_err(|e| Error::Transport(format!("send request: {e}")))?;
    send.finish()
        .map_err(|e| Error::Transport(format!("finish stream: {e}")))?;
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| Error::Transport(format!("read response: {e}")))?;
    Ok(MailboxResponse::try_from_bytes(&bytes)
        .map_err(Error::Decode)?
        .result)
}

/// One peer sync round-trip on `SYNC_ALPN` (same one-request-per-bi-stream
/// framing as the mailbox). The connection is to a *peer*, not a relay.
pub(crate) async fn sync_request(connection: &Connection, op: SyncOp) -> Result<SyncResult, Error> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| Error::Transport(format!("open stream: {e}")))?;
    send.write_all(&SyncRequest::new(op).to_bytes())
        .await
        .map_err(|e| Error::Transport(format!("send request: {e}")))?;
    send.finish()
        .map_err(|e| Error::Transport(format!("finish stream: {e}")))?;
    let bytes = recv
        .read_to_end(MAX_SYNC_RESPONSE_BYTES)
        .await
        .map_err(|e| Error::Transport(format!("read response: {e}")))?;
    Ok(SyncResponse::try_from_bytes(&bytes)
        .map_err(Error::Decode)?
        .result)
}

/// Deposit with a fresh connection per attempt. Deposits dedup by message
/// id on the relay, so retrying after a transport error is always safe.
/// An *unreachable* relay is not retried here at all — that won't heal in
/// seconds, and healing over time is the outbox's job (live-delivery.md §2);
/// in-attempt retries are for transient post-connect stream errors only.
pub(crate) async fn deposit_with_retry(
    endpoint: &Endpoint,
    relay: &str,
    envelope: &MessageEnvelope,
    timeout: std::time::Duration,
) -> Result<(), Error> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tracing::warn!(relay, attempt, error = %last_error, "deposit failed; retrying");
        }
        let connection = match connect(endpoint, relay, MAILBOX_ALPN, timeout).await {
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
