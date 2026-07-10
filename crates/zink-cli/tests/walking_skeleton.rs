//! 🚩 The walking skeleton, end to end: relay + two `zink-cli` binaries.
//! A encrypts + deposits for B's key; B fetches + opens + prints plaintext.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

// Multi-threaded runtime: the blocking `Command::output` calls must not
// starve the in-process relay's tasks.
#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn walking_skeleton__should_deliver_text_from_a_to_b_through_the_relay() {
    // Given: an in-process relay and two device keys
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("skeleton");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));

    // B connects once so its mailbox exists before A deposits
    let first_recv = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert_eq!(first_recv, "no new messages");

    // When: A sends to B through the relay
    let text = "hello from the walking skeleton";
    let to_b = format!("{pubkey_b}@{dial}");
    let sent = stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, text]));
    assert!(sent.starts_with("deposited "), "got: {sent}");

    // Then: B fetches, decrypts, prints
    let received = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert!(received.contains(text), "got: {received}");

    // And: the ack emptied the mailbox
    let drained = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert_eq!(drained, "no new messages");

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
