//! Relay binary: iroh endpoint + mailbox ALPN + blob cache, persisted on
//! disk (slice B5).
//!
//! Runs self-sufficient: no external iroh relays or discovery services. A
//! zink relay sits on a publicly reachable address and clients dial it by
//! its full `EndpointAddr` (id + socket addrs), which the ContactRecord's
//! `relays` field carries (SPEC §3.6).
//!
//! ```text
//! zink-relay [data-dir] [--port <udp-port>]  # defaults: ./zink-relay-data, ephemeral
//! ```
//!
//! A deployed relay wants a fixed `--port` so its dial string survives
//! restarts (the endpoint key already does, via `relay.key`).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use zink_relay::blobs::{BlobCacheConfig, fs_blob_cache};
use zink_relay::clock::SystemClock;
use zink_relay::fs::FsMailboxStore;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (data_dir, port) = parse_args()?;
    std::fs::create_dir_all(&data_dir)?;

    // The endpoint key must survive restarts: it IS the relay's identity —
    // the dial strings in every ContactRecord point at it.
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(load_or_create_key(&data_dir.join("relay.key"))?);
    if let Some(port) = port {
        builder = builder.bind_addr((Ipv4Addr::UNSPECIFIED, port))?;
    }
    let endpoint = builder.bind().await?;
    println!("zink-relay listening (data: {})", data_dir.display());
    println!("  endpoint id: {}", endpoint.id());
    for sock in endpoint.addr().ip_addrs() {
        println!("  dial: {}@{}", endpoint.id(), sock);
    }

    let mailboxes = FsMailboxStore::new(data_dir.join("mailboxes"));
    let blob_store = fs_blob_cache(
        &data_dir.join("blobs"),
        BlobCacheConfig::default(),
        SystemClock,
    )
    .await?;
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(mailboxes),
        &blob_store,
        SystemClock,
    );

    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    Ok(())
}

/// `[data-dir] [--port <udp-port>]`, in any order.
fn parse_args() -> Result<(PathBuf, Option<u16>), Box<dyn std::error::Error + Send + Sync>> {
    let mut data_dir = PathBuf::from("./zink-relay-data");
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--port" {
            let value = args.next().ok_or("missing value for --port")?;
            port = Some(value.parse().map_err(|e| format!("--port: {e}"))?);
        } else {
            data_dir = PathBuf::from(arg);
        }
    }
    Ok((data_dir, port))
}

fn load_or_create_key(path: &Path) -> Result<SecretKey, Box<dyn std::error::Error + Send + Sync>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(SecretKey::from_bytes(bytes.as_slice().try_into().map_err(
                |_| format!("{} is not a 32-byte key", path.display()),
            )?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            std::fs::write(path, key.to_bytes())?;
            Ok(key)
        }
        Err(e) => Err(format!("read {}: {e}", path.display()).into()),
    }
}
