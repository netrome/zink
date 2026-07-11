//! zink phone/desktop app (Tauri). C-spike scope: prove a real phone can
//! run the native iroh stack — one register round-trip against the relay.

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use zink_protocol::{
    MAILBOX_ALPN, MAX_RESPONSE_BYTES, MailboxOp, MailboxRequest, MailboxResponse, MailboxResult,
};

/// Register a mailbox on the relay over native QUIC — the same round-trip
/// as A6's browser spike, minus the iroh-relay detour: a native client
/// dials the relay's socket directly.
#[tauri::command]
async fn spike_register(relay: String) -> Result<String, String> {
    let (id, sock) = relay
        .split_once('@')
        .ok_or("relay must be <endpoint-id>@<ip:port>")?;
    let id = EndpointId::from_str(id).map_err(|e| format!("endpoint id: {e}"))?;
    let sock = std::net::SocketAddr::from_str(sock).map_err(|e| format!("socket addr: {e}"))?;

    // Ephemeral endpoint key (builder default) — the real keystore, where
    // the device key doubles as the endpoint key, is a later slice.
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .map_err(|e| format!("bind: {e}"))?;
    let connection = endpoint
        .connect(EndpointAddr::new(id).with_ip_addr(sock), MAILBOX_ALPN)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| format!("open stream: {e}"))?;
    send.write_all(&MailboxRequest::new(MailboxOp::Register).to_bytes())
        .await
        .map_err(|e| format!("send: {e}"))?;
    send.finish().map_err(|e| format!("finish: {e}"))?;
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| format!("read: {e}"))?;

    match MailboxResponse::try_from_bytes(&bytes)
        .map_err(|e| format!("decode: {e}"))?
        .result
    {
        MailboxResult::Registered => Ok(format!(
            "registered mailbox for {} over native QUIC",
            endpoint.id()
        )),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![spike_register])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
