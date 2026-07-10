//! 🚩 The walking skeleton, end to end: relay + two `zink-cli` binaries.
//! A encrypts + deposits for B's key; B fetches + opens + prints plaintext.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use iroh::Endpoint;
use iroh::endpoint::presets;
use zink_relay::mailbox::MailboxService;
use zink_relay::net::spawn_mailbox_router;
use zink_relay::store::InMemoryStore;

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zink-cli"))
        .args(args)
        .output()
        .expect("run zink-cli")
}

fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "zink-cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zink-skeleton-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn key_path(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

// Multi-threaded runtime: the blocking `Command::output` calls must not
// starve the in-process relay's tasks.
#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn walking_skeleton__should_deliver_text_from_a_to_b_through_the_relay() {
    // Given: an in-process relay and two device keys. (Default bind, not
    // loopback: iroh only dials loopback from loopback-bound endpoints, and
    // the CLI binds default.)
    let endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .expect("bind relay endpoint");
    let sock = *endpoint.addr().ip_addrs().next().expect("relay ip addr");
    let dial = format!("{}@{}", endpoint.id(), sock);
    let _router = spawn_mailbox_router(endpoint, MailboxService::new(InMemoryStore::new()));

    let dir = temp_dir();
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));

    // B connects once so its mailbox exists before A deposits
    let first_recv = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert_eq!(first_recv, "no new messages");

    // When: A sends to B through the relay
    let text = "hello from the walking skeleton";
    let sent = stdout_of(&cli(&[
        "send", "--key", &key_a, "--relay", &dial, "--to", &pubkey_b, text,
    ]));
    assert!(sent.starts_with("deposited "), "got: {sent}");

    // Then: B fetches, decrypts, prints
    let received = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert!(received.contains(text), "got: {received}");

    // And: the ack emptied the mailbox
    let drained = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert_eq!(drained, "no new messages");

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
