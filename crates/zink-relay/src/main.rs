//! Relay binary: iroh endpoint + mailbox ALPN (in-memory store for now).

use iroh::Endpoint;
use iroh::endpoint::presets;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_mailbox_router;
use zink_relay::store::InMemoryStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::builder(presets::N0).bind().await?;
    println!("zink-relay listening");
    println!("  endpoint id: {}", endpoint.id());

    let router = spawn_mailbox_router(endpoint, MailboxService::new(InMemoryStore::new()));

    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    Ok(())
}
