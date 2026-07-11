//! Network edge helpers: relay dialing, mailbox round-trips, retrying
//! deposits. One request per bi-stream, per the mailbox wire protocol.

use std::net::SocketAddr;
use std::str::FromStr;

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use zink_protocol::{
    DeviceKey, MAILBOX_ALPN, MAX_RESPONSE_BYTES, MailboxOp, MailboxRequest, MailboxResponse,
    MailboxResult, MessageEnvelope,
};

/// The endpoint key IS the device key: mailbox auth is the connection.
pub(crate) async fn bind_endpoint(device: &DeviceKey) -> Result<Endpoint, String> {
    Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&device.seed()))
        .bind()
        .await
        .map_err(|e| format!("bind endpoint: {e}"))
}

/// `<endpoint-id>@<ip:port>`, as printed by `zink-relay`.
pub(crate) fn parse_relay(spec: &str) -> Result<EndpointAddr, String> {
    let (id, sock) = spec
        .split_once('@')
        .ok_or("relay must be <endpoint-id>@<ip:port>")?;
    let id = EndpointId::from_str(id).map_err(|e| format!("relay endpoint id: {e}"))?;
    let sock = SocketAddr::from_str(sock).map_err(|e| format!("relay socket addr: {e}"))?;
    Ok(EndpointAddr::new(id).with_ip_addr(sock))
}

pub(crate) async fn connect(
    endpoint: &Endpoint,
    relay: &str,
    alpn: &[u8],
) -> Result<Connection, String> {
    endpoint
        .connect(parse_relay(relay)?, alpn)
        .await
        .map_err(|e| format!("connect to relay {relay}: {e}"))
}

pub(crate) async fn request(
    connection: &Connection,
    op: MailboxOp,
) -> Result<MailboxResult, String> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open stream: {e}"))?;
    send.write_all(&MailboxRequest::new(op).to_bytes())
        .await
        .map_err(|e| format!("send request: {e}"))?;
    send.finish().map_err(|e| format!("finish stream: {e}"))?;
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| format!("read response: {e}"))?;
    Ok(MailboxResponse::try_from_bytes(&bytes)
        .map_err(|e| format!("decode response: {e}"))?
        .result)
}

/// Deposit with a fresh connection per attempt. Deposits dedup by message
/// id on the relay, so retrying after a transport error is always safe.
pub(crate) async fn deposit_with_retry(
    endpoint: &Endpoint,
    relay: &str,
    envelope: &MessageEnvelope,
) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            eprintln!("deposit to {relay} failed ({last_error}); retrying");
        }
        let attempt_result = async {
            let connection = connect(endpoint, relay, MAILBOX_ALPN).await?;
            request(
                &connection,
                MailboxOp::Deposit {
                    envelope: Box::new(envelope.clone()),
                },
            )
            .await
        }
        .await;
        match attempt_result {
            Ok(MailboxResult::Deposited { .. }) => return Ok(()),
            Ok(other) => return Err(format!("unexpected response from {relay}: {other:?}")),
            Err(error) => last_error = error,
        }
    }
    Err(format!(
        "deposit to {relay} failed after 3 attempts: {last_error}"
    ))
}
