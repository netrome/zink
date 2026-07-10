//! B2 end to end: 1→N fan-out on one relay, and cross-relay dedup by id.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn fanout__should_deliver_one_send_to_every_recipient() {
    // Given: one relay, recipients B and C with registered mailboxes
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("fanout-1n");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    let key_c = key_path(&dir, "c.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));
    let pubkey_c = stdout_of(&cli(&["keygen", &key_c]));
    cli(&["recv", "--key", &key_b, "--relay", &dial]);
    cli(&["recv", "--key", &key_c, "--relay", &dial]);

    // When: A sends one message to both
    let text = "hello, both of you";
    let to_b = format!("{pubkey_b}@{dial}");
    let to_c = format!("{pubkey_c}@{dial}");
    let sent = stdout_of(&cli(&[
        "send", "--key", &key_a, "--to", &to_b, "--to", &to_c, text,
    ]));
    assert!(sent.ends_with("to 1 relay(s)"), "got: {sent}");

    // Then: both recipients decrypt the same message
    for key in [&key_b, &key_c] {
        let received = stdout_of(&cli(&["recv", "--key", key, "--relay", &dial]));
        assert!(received.contains(text), "got: {received}");
    }

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn fanout__should_dedup_by_id_when_deposited_to_two_relays() {
    // Given: B's mailbox lives on two relays
    let (_r1, dial_1) = spawn_relay().await;
    let (_r2, dial_2) = spawn_relay().await;
    let dir = temp_dir("fanout-dedup");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));
    cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]);

    // When: A deposits the same envelope to both relays
    let text = "sent twice, seen once";
    let to_b = format!("{pubkey_b}@{dial_1},{dial_2}");
    let sent = stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, text]));
    assert!(sent.ends_with("to 2 relay(s)"), "got: {sent}");

    // Then: draining both relays prints the message exactly once
    let received = stdout_of(&cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]));
    assert_eq!(received.matches(text).count(), 1, "got: {received}");

    // And: both mailboxes were acked
    let drained = stdout_of(&cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]));
    assert_eq!(drained, "no new messages");

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
