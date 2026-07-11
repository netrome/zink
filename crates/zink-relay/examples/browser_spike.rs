//! A6 spike server: iroh-relay (plain HTTP/WebSocket, browser-reachable) +
//! the zink mailbox endpoint homed on it, in one process. Prints what the
//! browser page needs. Dev scaffolding, not the production relay shape yet.

use std::net::{Ipv4Addr, SocketAddr};

use iroh::endpoint::presets;
use iroh::{Endpoint, RelayMode, RelayUrl};
use iroh_blobs::store::mem::MemStore;
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // iroh-relay server: plain HTTP so a browser can speak ws:// to it
    // without certificates (spike only; production wants TLS, slice C0).
    let http_bind: SocketAddr = (Ipv4Addr::UNSPECIFIED, 3340).into();
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new(http_bind));
    let relay_server = Server::spawn(config).await?;
    let relay_url: RelayUrl = format!("http://{}:3340", public_ip()?).parse()?;

    // The mailbox endpoint, homed on that relay so browser traffic reaches it.
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(relay_url.clone().into()))
        .bind()
        .await?;
    endpoint.online().await;
    println!("browser spike ready — open web/spike/ and enter:");
    println!("  relay url:   {relay_url}");
    println!("  endpoint id: {}", endpoint.id());

    let blob_store = MemStore::new();
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
        zink_relay::clock::SystemClock,
    );

    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    relay_server.shutdown().await?;
    Ok(())
}

/// First non-loopback IPv4 of this host — what a remote browser dials.
fn public_ip() -> Result<Ipv4Addr, Box<dyn std::error::Error>> {
    // Route-table trick: connecting a UDP socket picks the outbound interface.
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.connect("1.1.1.1:80")?;
    match sock.local_addr()? {
        SocketAddr::V4(addr) => Ok(*addr.ip()),
        SocketAddr::V6(_) => Err("no IPv4 route".into()),
    }
}
