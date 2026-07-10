//! Browser client (WASM). Slice A6: the risk spike — prove a browser can
//! reach the relay's mailbox ALPN through the iroh-relay WebSocket path and
//! round-trip one frame.

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use wasm_bindgen::prelude::*;
use zink_protocol::{
    MAILBOX_ALPN, MAX_RESPONSE_BYTES, MailboxOp, MailboxRequest, MailboxResponse, MailboxResult,
};

/// Connect to a zink relay through an iroh-relay server and register a
/// mailbox — one full round-trip on the mailbox ALPN, from a browser.
#[wasm_bindgen]
pub async fn spike_register(
    relay_url: String,
    relay_endpoint_id: String,
) -> Result<String, JsError> {
    let url =
        RelayUrl::from_str(&relay_url).map_err(|e| JsError::new(&format!("relay url: {e}")))?;
    let id = EndpointId::from_str(&relay_endpoint_id)
        .map_err(|e| JsError::new(&format!("endpoint id: {e}")))?;

    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(url.clone().into()))
        .bind()
        .await
        .map_err(|e| JsError::new(&format!("bind: {e}")))?;

    let connection = endpoint
        .connect(EndpointAddr::new(id).with_relay_url(url), MAILBOX_ALPN)
        .await
        .map_err(|e| JsError::new(&format!("connect: {e}")))?;

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| JsError::new(&format!("open stream: {e}")))?;
    send.write_all(&MailboxRequest::new(MailboxOp::Register).to_bytes())
        .await
        .map_err(|e| JsError::new(&format!("send: {e}")))?;
    send.finish()
        .map_err(|e| JsError::new(&format!("finish: {e}")))?;
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| JsError::new(&format!("read: {e}")))?;

    match MailboxResponse::try_from_bytes(&bytes)
        .map_err(|e| JsError::new(&format!("decode: {e}")))?
        .result
    {
        MailboxResult::Registered => Ok(format!(
            "registered mailbox for {} via {}",
            endpoint.id(),
            relay_url
        )),
        other => Err(JsError::new(&format!("unexpected response: {other:?}"))),
    }
}
