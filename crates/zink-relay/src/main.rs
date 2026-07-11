//! Relay binary: iroh endpoint + mailbox ALPN + blob cache, persisted on
//! disk (slice B5).
//!
//! Runs self-sufficient: no external iroh relays or discovery services. A
//! zink relay sits on a publicly reachable address and clients dial it by
//! its full `EndpointAddr` (id + socket addrs), which the ContactRecord's
//! `relays` field carries (SPEC §3.6).
//!
//! ```text
//! zink-relay [data-dir]     # default: ./zink-relay-data
//! ```

use std::path::{Path, PathBuf};

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use zink_relay::blobs::{DEFAULT_BLOB_TTL, DEFAULT_GC_INTERVAL, fs_blob_cache};
use zink_relay::clock::SystemClock;
use zink_relay::fs::FsMailboxStore;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let data_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("./zink-relay-data"), PathBuf::from);
    std::fs::create_dir_all(&data_dir)?;

    // The endpoint key must survive restarts: it IS the relay's identity —
    // the dial strings in every ContactRecord point at it.
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(load_or_create_key(&data_dir.join("relay.key"))?)
        .bind()
        .await?;
    println!("zink-relay listening (data: {})", data_dir.display());
    println!("  endpoint id: {}", endpoint.id());
    for sock in endpoint.addr().ip_addrs() {
        println!("  dial: {}@{}", endpoint.id(), sock);
    }

    let mailboxes = FsMailboxStore::new(data_dir.join("mailboxes"));
    let blob_store = fs_blob_cache(
        &data_dir.join("blobs"),
        DEFAULT_BLOB_TTL,
        DEFAULT_GC_INTERVAL,
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
