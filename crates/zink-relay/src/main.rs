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

const USAGE: &str = "usage: zink-relay [data-dir] [--port <udp-port>]

  data-dir          where mailboxes, the blob cache, and the relay's identity
                    key live (default: ./zink-relay-data)
  --port <udp-port> fixed UDP port, so the dial string survives restarts
                    (default: ephemeral)
  -h, --help        this text
  -V, --version     version + build info";

/// Package version + `git describe` (commit, nearest tag, dirty marker),
/// embedded by build.rs.
fn version() -> String {
    format!(
        "zink-relay {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("ZINK_BUILD_INFO")
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_tracing();
    let (data_dir, port) = parse_args();
    std::fs::create_dir_all(&data_dir)?;

    // The endpoint key must survive restarts: it IS the relay's identity —
    // the dial strings in every ContactRecord point at it.
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(load_or_create_key(&data_dir.join("relay.key"))?);
    if let Some(port) = port {
        builder = builder.bind_addr((Ipv4Addr::UNSPECIFIED, port))?;
    }
    let endpoint = builder.bind().await?;
    // First log line = what's running — `journalctl -u zink-relay` answers
    // "which build is deployed?" without touching the binary.
    println!("{} listening (data: {})", version(), data_dir.display());
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

/// `[data-dir] [--port <udp-port>]`, in any order. `-h`/`-V` print and
/// exit; argument mistakes print usage and exit(2) — never a Debug-dumped
/// error, and never silently taken as the data dir.
fn parse_args() -> (PathBuf, Option<u16>) {
    let bad = |message: &str| -> ! {
        eprintln!("{message}\n{USAGE}");
        std::process::exit(2);
    };
    let mut data_dir = None;
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{}", version());
                std::process::exit(0);
            }
            "--port" => {
                let Some(value) = args.next() else {
                    bad("missing value for --port");
                };
                match value.parse() {
                    Ok(value) => port = Some(value),
                    Err(e) => bad(&format!("--port: {e}")),
                }
            }
            flag if flag.starts_with('-') => bad(&format!("unknown flag {flag}")),
            _ if data_dir.is_some() => bad("more than one data-dir given"),
            _ => data_dir = Some(PathBuf::from(arg)),
        }
    }
    (
        data_dir.unwrap_or_else(|| PathBuf::from("./zink-relay-data")),
        port,
    )
}

/// Logs to stderr, off unless `RUST_LOG` is set (default `warn` so real
/// warnings still surface). `RUST_LOG=zink_relay=debug,iroh=info` for the
/// live-delivery detail.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
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
            write_private(path, &key.to_bytes())?;
            Ok(key)
        }
        Err(e) => Err(format!("read {}: {e}", path.display()).into()),
    }
}

/// Write the relay's identity key owner-only (0600 on Unix) — it must not be
/// world-readable. Small enough to keep here rather than take a dependency
/// on the client crate just for a file-permission helper.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}
