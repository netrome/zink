//! C4b end to end: a subscribed listener receives messages without polling
//! — the relay nudges its live connection on deposit, the listener drains.

mod common;

use common::{cli, key_path, spawn_listener, spawn_relay, stdout_of, temp_dir};

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

/// Bob's stored history, read offline (history reads never touch the
/// network). Called once the listener has *announced* the arrival, so there
/// is nothing to wait for.
fn history_of(key: &str) -> String {
    let listing = stdout_of(&cli(&["conversations", "--key", key]));
    let conversation = listing
        .split_whitespace()
        .next()
        .filter(|id| id.len() == 64)
        .expect("one conversation");
    stdout_of(&cli(&["history", "--key", key, conversation]))
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
    let listener = spawn_listener(&key_b);

    // Then: the subscription's catch-up drain lands the waiting message —
    // and proves the listener is connected + registered. Waited for on the
    // listener's own arrival line (De6c): it prints only after storing, so
    // this is the same proof the old 250 ms history poll gave, arriving as a
    // signal instead of being hunted for by subprocess.
    listener.wait_for("while you were out");

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
    listener.wait_for("nudge nudge");
    assert_eq!(
        history_of(&key_b).lines().collect::<Vec<_>>(),
        ["Alice: while you were out", "Alice: nudge nudge"],
        "both messages threaded in order in bob's store"
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
