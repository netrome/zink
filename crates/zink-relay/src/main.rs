//! Relay binary: iroh endpoint + mailbox ALPN (in-memory store for now).
//!
//! Runs self-sufficient: no external iroh relays or discovery services. A
//! zink relay sits on a publicly reachable address and clients dial it by
//! its full `EndpointAddr` (id + socket addrs), which the ContactRecord's
//! `relays` field carries (SPEC §3.6).

use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::presets;
use zink_relay::blobs::{BlobRetention, DEFAULT_BLOB_TTL, DEFAULT_GC_INTERVAL, blob_cache};
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::builder(presets::Minimal).bind().await?;
    println!("zink-relay listening");
    println!("  endpoint id: {}", endpoint.id());
    for sock in endpoint.addr().ip_addrs() {
        println!("  dial: {}@{}", endpoint.id(), sock);
    }

    let retention = Arc::new(BlobRetention::new(DEFAULT_BLOB_TTL));
    let blob_store = blob_cache(retention.clone(), DEFAULT_GC_INTERVAL);
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
        retention,
    );

    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    Ok(())
}
