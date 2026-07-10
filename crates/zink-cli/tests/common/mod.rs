//! Shared helpers for zink-cli end-to-end tests.

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
    let endpoint = Endpoint::builder(presets::Minimal)
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
    );
    (router, dial)
}
