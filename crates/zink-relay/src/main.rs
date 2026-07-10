//! Relay binary: iroh endpoint + mailbox ALPN (in-memory store for now).
//!
//! Runs self-sufficient: no external iroh relays or discovery services. A
//! zink relay sits on a publicly reachable address and clients dial it by
//! its full `EndpointAddr` (id + socket addrs), which the ContactRecord's
//! `relays` field carries (SPEC §3.6).

use iroh::Endpoint;
use iroh::endpoint::presets;
use iroh_blobs::store::mem::MemStore;
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

    let blob_store = MemStore::new();
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
    );

    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    Ok(())
}
