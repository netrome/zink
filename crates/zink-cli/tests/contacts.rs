//! C2 end to end: exchange ContactRecords, then message by petname with no
//! keys or relay flags in sight.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn contacts__should_enable_messaging_by_name_after_a_record_exchange() {
    // Given: two identities with profiles on one relay
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("contacts");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);

    // When: each publishes a record (QR payload) and adds the other's
    let record_a = stdout_of(&cli(&[
        "my-record",
        "--key",
        &key_a,
        "--name",
        "Alice",
        "--relay",
        &dial,
    ]));
    let record_b = stdout_of(&cli(&[
        "my-record",
        "--key",
        &key_b,
        "--name",
        "Bob",
        "--relay",
        &dial,
    ]));
    assert!(record_a.starts_with("ZINK:"), "got: {record_a}");

    let added = stdout_of(&cli(&["contact-add", "--key", &key_a, &record_b]));
    assert_eq!(added, "added contact \"Bob\"");
    stdout_of(&cli(&["contact-add", "--key", &key_b, &record_a]));

    let listed = stdout_of(&cli(&["contacts", "--key", &key_a]));
    assert!(listed.starts_with("Bob"), "got: {listed}");

    // Then: registered mailboxes (first recv uses home relays by default) …
    assert_eq!(
        stdout_of(&cli(&["recv", "--key", &key_b])),
        "no new messages"
    );

    // … and A messages B *by name*, no keys or relay flags anywhere
    let sent = stdout_of(&cli(&["send", "--key", &key_a, "--to", "Bob", "hi Bob!"]));
    assert!(sent.contains("to 1 relay(s)"), "got: {sent}");
    let received = stdout_of(&cli(&["recv", "--key", &key_b]));
    assert!(received.contains("hi Bob!"), "got: {received}");

    // And: B replies by name, threading into the same conversation
    let (conv_sent, _) = sent
        .split_once("(conv ")
        .unwrap()
        .1
        .split_once(',')
        .unwrap();
    let reply = stdout_of(&cli(&[
        "send",
        "--key",
        &key_b,
        "--to",
        "Alice",
        "hi Alice!",
    ]));
    assert!(
        reply.contains(&format!("(conv {conv_sent},")),
        "got: {reply}"
    );
    let drained = stdout_of(&cli(&["recv", "--key", &key_a]));
    assert!(drained.contains("hi Alice!"), "got: {drained}");

    // And: a name collision with a different key is rejected
    let key_c = key_path(&dir, "c.key");
    cli(&["keygen", &key_c]);
    let record_c = stdout_of(&cli(&[
        "my-record",
        "--key",
        &key_c,
        "--name",
        "Bob",
        "--relay",
        &dial,
    ]));
    let output = cli(&["contact-add", "--key", &key_a, &record_c]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already named"),
        "got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
