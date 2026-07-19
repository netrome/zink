//! Shared helpers for zink-cli end-to-end tests.
//!
//! Each test binary compiles this module independently and uses a subset,
//! so per-binary dead-code warnings are expected noise.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use iroh::Endpoint;
use iroh::endpoint::presets;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

pub fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zink-cli"))
        .args(args)
        // Down-relay tests should fail in milliseconds, not the production
        // 10 s connect deadline. In-process relays answer in single-digit ms,
        // so 500 ms has plenty of headroom for loaded CI.
        .env("ZINK_CONNECT_TIMEOUT_MS", "500")
        .output()
        .expect("run zink-cli")
}

pub fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "zink-cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Unique per test name and process; caller cleans up with `remove_dir_all`.
pub fn temp_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zink-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn key_path(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// An in-process relay. Returns the router guard (dropping it stops the
/// relay) and its dial string. Default bind, not loopback: iroh only dials
/// loopback from loopback-bound endpoints, and the CLI binds default.
pub async fn spawn_relay() -> (iroh::protocol::Router, String) {
    spawn_relay_at(iroh::SecretKey::generate(), 0).await
}

/// A relay with a caller-controlled identity and port — restartable at the
/// *same dial string* (drop the router, spawn again with the same key and
/// port), the way the deployed relay's persisted `relay.key` + stable port
/// behave across restarts. `port` 0 = pick one.
pub async fn spawn_relay_at(
    secret: iroh::SecretKey,
    port: u16,
) -> (iroh::protocol::Router, String) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .bind_addr(std::net::SocketAddr::from((
            std::net::Ipv4Addr::UNSPECIFIED,
            port,
        )))
        .expect("valid bind addr")
        .bind()
        .await
        .expect("bind relay endpoint");
    let sock = *endpoint.addr().ip_addrs().next().expect("relay ip addr");
    let dial = format!("{}@{}", endpoint.id(), sock);
    let blob_store = iroh_blobs::store::mem::MemStore::new();
    let router = spawn_relay_router(
        endpoint,
        MailboxService::new(InMemoryStore::new()),
        &blob_store,
        zink_relay::clock::SystemClock,
    );
    (router, dial)
}

/// An in-process iroh relay *server* (peer rendezvous + QAD at the same
/// picked port number, the De2 same-port convention) — what makes clients
/// dialable by key. Pair its URL with a mailbox dial as
/// `<dial>#<url>` to form a full relay spec. Port picked up front (two
/// `:0` binds would land on different numbers); retried against races.
pub async fn spawn_iroh_relay() -> (iroh_relay::server::Server, String) {
    use iroh_relay::server::{QuicConfig, RelayConfig, Server, ServerConfig};
    use std::net::Ipv4Addr;
    for _ in 0..3 {
        let port = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("pick a port")
            .local_addr()
            .expect("local addr")
            .port();
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, port)));
        let mut quic = QuicConfig::new((Ipv4Addr::LOCALHOST, port));
        let (_certs, tls) = iroh_relay::server::testing::self_signed_tls_certs_and_config();
        quic.server_config = Some(tls);
        config.quic = Some(quic);
        if let Ok(server) = Server::spawn(config).await {
            let url = format!("http://{}", server.http_addr().expect("http addr"));
            return (server, url);
        }
    }
    panic!("no free port pair for a test iroh relay in 3 attempts");
}
