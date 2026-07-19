//! Relay binary: iroh endpoint + mailbox ALPN + blob cache, persisted on
//! disk (slice B5) — plus, since D0b, the embedded **iroh relay server**
//! (plain HTTP, no TLS/domain for native clients): one service = iroh
//! relaying (peer rendezvous + holepunch coordination) + mailbox/blobs.
//! Since De2 the relay server also answers **QUIC address discovery** (QAD,
//! the STUN replacement) on UDP at the same port number as the HTTP relay —
//! the *same-port convention* clients use to derive the QAD port from the
//! relay URL. Without it a homing client's first net-report waits out the
//! full probe timeout (~3 s) before the endpoint reports online, and
//! address discovery for holepunching is disco-only.
//!
//! Runs self-sufficient: no external iroh relays or discovery services. A
//! zink relay sits on a publicly reachable address and clients dial it by
//! its full `EndpointAddr` (id + socket addrs), which the ContactRecord's
//! `relays` field carries (SPEC §3.6). Clients *home* to the iroh relay
//! URL (`RelayMode::Custom`) to stay reachable by key across NATs.
//!
//! ```text
//! zink-relay [data-dir] [--port <udp-port>] [--relay-port <tcp-port>]
//!            # defaults: ./zink-relay-data, ephemeral, ephemeral
//! ```
//!
//! A deployed relay wants a fixed `--port` and `--relay-port` so its
//! printed spec survives restarts (the endpoint key already does, via
//! `relay.key`).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use iroh_relay::server::{QuicConfig, RelayConfig, Server, ServerConfig};
use zink_relay::blobs::{BlobCacheConfig, fs_blob_cache};
use zink_relay::clock::SystemClock;
use zink_relay::fs::FsMailboxStore;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;

const USAGE: &str = "usage: zink-relay [data-dir] [--port <udp-port>] [--relay-port <tcp-port>]

  data-dir          where mailboxes, the blob cache, and the relay's identity
                    key live (default: ./zink-relay-data)
  --port <udp-port> fixed UDP port, so the dial string survives restarts
                    (default: ephemeral)
  --relay-port <tcp-port>
                    fixed HTTP port for the embedded iroh relay server —
                    peer rendezvous/holepunch coordination; clients home to
                    it. QUIC address discovery is served on the same port
                    number over UDP — open both (default: ephemeral)
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
    let (data_dir, port, relay_port) = parse_args();
    std::fs::create_dir_all(&data_dir)?;

    // The embedded iroh relay server (D0b): plain HTTP — `RelayConfig::new`
    // is `tls: None`, so no domain/cert; native clients only for now (a
    // browser client would need HTTPS, post-MVP). Untrusted like the
    // mailbox: it coordinates holepunching and forwards *encrypted* QUIC.
    // QAD rides on UDP at the same port number (De2, same-port convention).
    let (relay_server, relay_http_port) = spawn_relay_server(relay_port).await?;

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
    // The full relay spec `<id>@<ip:port>#<relay-url>` — what users paste
    // into a profile: mailbox dial string + the same host's iroh relay URL.
    for sock in endpoint.addr().ip_addrs() {
        // A URL host must bracket IPv6 literals ([::1]:4456, not ::1:4456).
        let url_host = std::net::SocketAddr::new(sock.ip(), relay_http_port);
        println!("  relay spec: {}@{sock}#http://{url_host}", endpoint.id());
    }
    println!("  QAD: udp/{relay_http_port} (self-signed TLS)");

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
    relay_server.shutdown().await?;
    Ok(())
}

/// Spawn the embedded iroh relay server: HTTP relaying on TCP + QAD on UDP,
/// both at the same port number (clients derive the QAD port from the relay
/// URL, so the two must agree). An ephemeral port is picked here rather than
/// left to the OS — two `:0` binds would land on different numbers; retried
/// in case the picked pair races another process.
async fn spawn_relay_server(
    relay_port: Option<u16>,
) -> Result<(Server, u16), Box<dyn std::error::Error + Send + Sync>> {
    let attempts = if relay_port.is_some() { 1 } else { 3 };
    let mut last_error: Box<dyn std::error::Error + Send + Sync> = "no port attempted".into();
    for _ in 0..attempts {
        let port = match relay_port {
            Some(port) => port,
            None => std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?
                .local_addr()?
                .port(),
        };
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new((Ipv4Addr::UNSPECIFIED, port)));
        let mut quic = QuicConfig::new((Ipv4Addr::UNSPECIFIED, port));
        quic.server_config = Some(qad_tls_config()?);
        config.quic = Some(quic);
        match Server::spawn(config).await {
            Ok(server) => {
                let http_port = server
                    .http_addr()
                    .ok_or("iroh relay server has no http addr")?
                    .port();
                return Ok((server, http_port));
            }
            Err(e) => last_error = e.into(),
        }
    }
    Err(last_error)
}

/// The QAD endpoint's TLS 1.3 config (QUIC requires TLS; iroh rejects
/// anything below 1.3). The cert is self-signed and regenerated every start:
/// no domain, no CA, nothing pins it — clients deliberately don't verify it
/// (iroh connections authenticate by endpoint key; a QAD man-in-the-middle
/// can at most misreport a client's observed address, degrading holepunching
/// to the relayed path — see zink-client's `net.rs`).
fn qad_tls_config() -> Result<rustls::ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let cert = rcgen::generate_simple_self_signed(vec!["zink-relay".to_string()])?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key.into())?;
    Ok(config)
}

/// `[data-dir] [--port <udp-port>] [--relay-port <tcp-port>]`, in any
/// order. `-h`/`-V` print and exit; argument mistakes print usage and
/// exit(2) — never a Debug-dumped error, and never silently taken as the
/// data dir.
fn parse_args() -> (PathBuf, Option<u16>, Option<u16>) {
    let bad = |message: &str| -> ! {
        eprintln!("{message}\n{USAGE}");
        std::process::exit(2);
    };
    let mut data_dir = None;
    let mut port = None;
    let mut relay_port = None;
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
            "--relay-port" => {
                let Some(value) = args.next() else {
                    bad("missing value for --relay-port");
                };
                match value.parse() {
                    Ok(value) => relay_port = Some(value),
                    Err(e) => bad(&format!("--relay-port: {e}")),
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
        relay_port,
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
