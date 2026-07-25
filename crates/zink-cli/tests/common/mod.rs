//! Shared helpers for zink-cli end-to-end tests.
//!
//! Each test binary compiles this module independently and uses a subset,
//! so per-binary dead-code warnings are expected noise.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use iroh::Endpoint;
use iroh::endpoint::presets;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_relay_router;
use zink_relay::store::InMemoryStore;

/// How long to wait for a line a listener is expected to print. Generous —
/// it bounds a hang, it is not a delay anyone pays: the waits are reactive,
/// so a healthy run returns the moment the line appears.
const LINE_DEADLINE: Duration = Duration::from_secs(15);

/// A background `zink-cli listen`, killed on drop — even when an assertion
/// panics. Waits are **reactive** (De6c): its stdout is read on a thread and
/// tests block on the line they need instead of polling subprocesses.
pub struct Listener {
    child: Child,
    lines: mpsc::Receiver<String>,
    /// The `reachability:` line this listener reported at startup.
    verdict: String,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Listener {
    /// What this listener said about being reachable by key at startup.
    pub fn verdict(&self) -> &str {
        &self.verdict
    }

    /// Block until a printed line contains `marker`, and return that line.
    /// Lines seen on the way are reported if the deadline passes, so a
    /// failure says what the listener *did* say.
    pub fn wait_for(&self, marker: &str) -> String {
        let deadline = Instant::now() + LINE_DEADLINE;
        let mut seen: Vec<String> = Vec::new();
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) if line.contains(marker) => return line,
                Ok(line) => seen.push(line),
                Err(_) => panic!("listener never printed {marker:?}; it said: {seen:#?}"),
            }
        }
    }
}

/// Spawn `listen` for `key` and block until it has reported its **
/// reachability verdict** (De6c) — a definite point in its startup, whatever
/// the answer. Enough for a mailbox-only listener, which is never dialable by
/// key and shouldn't be waited on for it; use `spawn_homed_listener` when the
/// test needs peers to reach this device.
///
/// Binding is not reachability: dial-by-key routes through a home relay, so
/// for roughly a second after spawning, a homed listener answers nothing.
/// These tests used to poll `who-is` on a 250 ms sleep against a 15 s
/// deadline (each probe a fresh CLI process, ~700 ms) to get past that
/// window; now the listener says where it stands and this returns at once.
pub fn spawn_listener(key: &str) -> Listener {
    // Production timeouts on purpose (as before De6c): a listener's
    // subscription loop wants patience, not the one-shot commands' haste —
    // a tight connect deadline would make it flap and back off on a loaded
    // machine. Nothing here waits on a *failure*, so haste buys nothing.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zink-cli"))
        .args(["listen", "--key", key])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn listener");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                return; // the test dropped the listener
            }
        }
    });
    let mut listener = Listener {
        child,
        lines,
        verdict: String::new(),
    };
    listener.verdict = listener.wait_for("reachability:");
    listener
}

/// `spawn_listener` for a test that needs peers to **reach this device by
/// key** — who-is, backfill, direct delivery. Asserts the verdict is the
/// positive one, so a profile without a relay url fails here with the reason
/// rather than as a mystery timeout downstream.
pub fn spawn_homed_listener(key: &str) -> Listener {
    let listener = spawn_listener(key);
    let verdict = listener.verdict();
    assert!(
        verdict.contains("reachable by key"),
        "listener is not dialable by key: {verdict}"
    );
    listener
}

pub fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zink-cli"))
        .args(args)
        // Down-relay tests should fail in milliseconds, not the production
        // 10 s connect deadline. In-process relays answer in single-digit ms,
        // so 500 ms has plenty of headroom for loaded CI.
        .env("ZINK_CONNECT_TIMEOUT_MS", "500")
        // Don't wait out iroh's ~3 s post-failed-dial drain on every one-shot
        // command (D5): ~30 invocations per test made that the suite's
        // dominant cost. The price is iroh's ungraceful-abort line on stderr,
        // which these tests ignore.
        .env("ZINK_CLOSE_DEADLINE_MS", "200")
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
