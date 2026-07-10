//! End-to-end over real iroh connections: one endpoint deposits, another
//! fetches — the A4 done-criterion.

use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use zink_protocol::{
    DeviceKey, FORMAT_VERSION, KeyCommitment, MAILBOX_ALPN, MAX_RESPONSE_BYTES, MailboxOp,
    MailboxRequest, MailboxResponse, MailboxResult, MessageCore, MessageEnvelope,
};
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

async fn spawn_relay() -> (iroh::protocol::Router, EndpointAddr) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .expect("bind relay endpoint");
    let addr = endpoint.addr();
    let retention = std::sync::Arc::new(zink_relay::blobs::BlobRetention::new(
        zink_relay::blobs::DEFAULT_BLOB_TTL,
    ));
    let blob_store = iroh_blobs::store::mem::MemStore::new();
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
        retention,
    );
    (router, addr)
}

/// A client endpoint whose iroh key matches `DeviceKey::from_seed([seed; 32])` —
/// both derive the same Ed25519 key from the seed. The endpoint is returned
/// alongside the connection: dropping it kills the connection.
async fn client(seed: u8, relay: &EndpointAddr) -> (Endpoint, Connection) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(&[seed; 32]))
        .bind()
        .await
        .expect("bind client endpoint");
    let connection = endpoint
        .connect(relay.clone(), MAILBOX_ALPN)
        .await
        .expect("connect to relay");
    (endpoint, connection)
}

async fn request(connection: &Connection, op: MailboxOp) -> MailboxResult {
    let (mut send, mut recv) = connection.open_bi().await.expect("open stream");
    send.write_all(&MailboxRequest::new(op).to_bytes())
        .await
        .expect("send request");
    send.finish().expect("finish stream");
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .expect("read response");
    MailboxResponse::try_from_bytes(&bytes)
        .expect("decode response")
        .result
}

fn envelope_from_1_to_2() -> MessageEnvelope {
    let sender = DeviceKey::from_seed([1; 32]);
    let core = MessageCore {
        version: FORMAT_VERSION,
        conversation: None,
        parents: vec![],
        recipients: vec![DeviceKey::from_seed([2; 32]).public()],
        sender: sender.public(),
        seq: 0,
        logical: 0,
        timestamp_ms: 1_700_000_000_000,
        body: vec![0xC1, 0x9E, 0x27],
        key_commit: KeyCommitment([0; 32]),
        blob_refs: vec![],
    };
    MessageEnvelope::new(core, &sender)
}

#[tokio::test]
#[allow(non_snake_case)]
async fn mailbox__should_deliver_a_deposit_from_one_endpoint_to_another() {
    // Given: a relay, and recipient B registered
    let (_router, relay_addr) = spawn_relay().await;
    let (_b_endpoint, b) = client(2, &relay_addr).await;
    assert_eq!(
        request(&b, MailboxOp::Register).await,
        MailboxResult::Registered
    );

    // When: A deposits an envelope addressed to B
    let (_a_endpoint, a) = client(1, &relay_addr).await;
    let envelope = envelope_from_1_to_2();
    let deposited = request(
        &a,
        MailboxOp::Deposit {
            envelope: Box::new(envelope.clone()),
        },
    )
    .await;
    assert_eq!(deposited, MailboxResult::Deposited { id: envelope.id() });

    // Then: B fetches it, intact and verifiable
    let MailboxResult::Envelopes { items } = request(&b, MailboxOp::Fetch { after: 0 }).await
    else {
        panic!("expected Envelopes");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].envelope, envelope);
    assert_eq!(items[0].envelope.verify(), Ok(()));

    // And: after ack the mailbox is empty
    let cursor = items[0].cursor;
    assert_eq!(
        request(&b, MailboxOp::Ack { up_to: cursor }).await,
        MailboxResult::Acked
    );
    let MailboxResult::Envelopes { items } = request(&b, MailboxOp::Fetch { after: 0 }).await
    else {
        panic!("expected Envelopes");
    };
    assert!(items.is_empty());
}

#[tokio::test]
#[allow(non_snake_case)]
async fn mailbox__should_dedup_a_retried_deposit_over_the_wire() {
    // Given: a relay with B registered
    let (_router, relay_addr) = spawn_relay().await;
    let (_b_endpoint, b) = client(2, &relay_addr).await;
    request(&b, MailboxOp::Register).await;

    // When: the sender retries the same deposit (e.g. after a lost ack)
    let (_a_endpoint, a) = client(1, &relay_addr).await;
    let envelope = envelope_from_1_to_2();
    for _ in 0..3 {
        let result = request(
            &a,
            MailboxOp::Deposit {
                envelope: Box::new(envelope.clone()),
            },
        )
        .await;
        assert_eq!(result, MailboxResult::Deposited { id: envelope.id() });
    }

    // Then: the mailbox holds it exactly once
    let MailboxResult::Envelopes { items } = request(&b, MailboxOp::Fetch { after: 0 }).await
    else {
        panic!("expected Envelopes");
    };
    assert_eq!(items.len(), 1);
}

#[tokio::test]
#[allow(non_snake_case)]
async fn mailbox__should_return_an_error_response_for_garbage_requests() {
    // Given
    let (_router, relay_addr) = spawn_relay().await;
    let (_a_endpoint, a) = client(1, &relay_addr).await;

    // When: raw garbage instead of a request
    let (mut send, mut recv) = a.open_bi().await.expect("open stream");
    send.write_all(&[0xFF; 40]).await.expect("send garbage");
    send.finish().expect("finish stream");
    let bytes = recv
        .read_to_end(MAX_RESPONSE_BYTES)
        .await
        .expect("read response");

    // Then: a clean protocol-level error, not a dropped connection
    let response = MailboxResponse::try_from_bytes(&bytes).expect("decode response");
    assert!(matches!(response.result, MailboxResult::Error { .. }));
}
