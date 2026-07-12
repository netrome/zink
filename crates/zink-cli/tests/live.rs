//! C4b end to end: a subscribed listener receives messages without polling
//! — the relay nudges its live connection on deposit, the listener drains.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

fn record_payload(key: &str, name: &str, relay: &str) -> String {
    stdout_of(&cli(&[
        "my-record",
        "--key",
        key,
        "--name",
        name,
        "--relay",
        relay,
    ]))
    .lines()
    .next()
    .expect("record payload line")
    .to_string()
}

/// The listener process — killed even when an assertion panics.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Poll bob's *offline* view (history reads never touch the network) until
/// the needle shows up — delivered by the listener, or the timeout fails.
fn wait_for_in_history(key: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let listing = stdout_of(&cli(&["conversations", "--key", key]));
        let conversation = listing
            .split_whitespace()
            .next()
            .filter(|id| id.len() == 64);
        if let Some(conversation) = conversation {
            let history = stdout_of(&cli(&["history", "--key", key, conversation]));
            if history.contains(needle) {
                return history;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never arrived in {key}'s history"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn live__should_deliver_to_a_subscribed_listener_without_polling() {
    // Given: alice and bob exchanged records; one message already waits in
    // bob's mailbox (sent before he listens)
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("live");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "while you were out",
    ]));

    // When: bob starts listening (no recv, no poll — just the subscription)
    let _listener = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_zink-cli"))
            .args(["listen", "--key", &key_b])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn listener"),
    );

    // Then: the subscription's catch-up drain lands the waiting message —
    // and proves the listener is connected + registered
    wait_for_in_history(&key_b, "while you were out");

    // When: alice sends again, with the listener already live
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "nudge nudge",
    ]));

    // Then: it arrives with no action on bob's side at all — the only
    // possible path is deposit → relay nudge → listener fetch
    let history = wait_for_in_history(&key_b, "nudge nudge");
    assert_eq!(
        history.lines().collect::<Vec<_>>(),
        ["Alice: while you were out", "Alice: nudge nudge"],
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
