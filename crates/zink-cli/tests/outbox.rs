//! C4a end to end: a send while the relay is down is queued (pending in
//! history, never lost), a later flush trigger delivers it — blobs included
//! — and entries past the give-up window stop retrying but stay surfaced.

mod common;

use common::{cli, key_path, spawn_relay_at, stdout_of, temp_dir};

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

fn conversation_of(key: &str) -> String {
    stdout_of(&cli(&["conversations", "--key", key]))
        .split_whitespace()
        .next()
        .expect("conversation id")
        .to_string()
}

/// The dial string's port — what a restarted relay must rebind.
fn port_of(dial: &str) -> u16 {
    dial.rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("port in dial string")
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn outbox__should_queue_while_the_relay_is_down_and_flush_when_it_returns() {
    // Given: alice and bob know each other via a relay that then goes down
    let secret = iroh::SecretKey::generate();
    let (router, dial) = spawn_relay_at(secret.clone(), 0).await;
    let port = port_of(&dial);
    let dir = temp_dir("outbox");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    drop(router);

    // When: alice sends text + image into the void
    let image: Vec<u8> = (0..30_000u32).map(|i| (i % 241) as u8).collect();
    let image_path = dir.join("photo.bin");
    std::fs::write(&image_path, &image).expect("write image");
    let failed = cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "--image",
        &image_path.to_string_lossy(),
        "hello?",
    ]);

    // Then: the send reports queued-not-lost, and history shows pending
    assert!(!failed.status.success(), "send should fail with relay down");
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("queued for retry"), "got: {stderr}");
    let conversation = conversation_of(&key_a);
    let history = stdout_of(&cli(&["history", "--key", &key_a, &conversation]));
    assert_eq!(history.lines().next(), Some("me: hello? [pending]"));

    // When: the relay returns at the same dial string, bob re-registers,
    // and alice's next recv triggers the flush
    let (_router, dial_again) = spawn_relay_at(secret, port).await;
    assert_eq!(
        dial_again, dial,
        "restarted relay must keep its dial string"
    );
    stdout_of(&cli(&["recv", "--key", &key_b])); // re-register (in-memory store forgot)
    stdout_of(&cli(&["recv", "--key", &key_a]));

    // Then: pending clears on alice, and bob receives text and blobs
    let history = stdout_of(&cli(&["history", "--key", &key_a, &conversation]));
    assert_eq!(history.lines().next(), Some("me: hello?"), "got: {history}");
    let blobs_dir = dir.join("received");
    std::fs::create_dir_all(&blobs_dir).expect("create blobs dir");
    let received = stdout_of(&cli(&[
        "recv",
        "--key",
        &key_b,
        "--blobs-dir",
        &blobs_dir.to_string_lossy(),
    ]));
    assert!(received.contains("hello?"), "got: {received}");
    assert!(received.contains("saved full blob"), "got: {received}");
    let saved = std::fs::read_dir(&blobs_dir)
        .expect("read blobs dir")
        .map(|entry| std::fs::read(entry.expect("dir entry").path()).expect("read blob"))
        .next()
        .expect("one saved blob");
    assert_eq!(saved, image);

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn outbox__should_stop_retrying_but_keep_surfacing_expired_entries() {
    // Given: a queued send whose outbox entry is aged past the give-up window
    let secret = iroh::SecretKey::generate();
    let (router, dial) = spawn_relay_at(secret.clone(), 0).await;
    let port = port_of(&dial);
    let dir = temp_dir("outbox-expiry");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    drop(router);
    cli(&["send", "--key", &key_a, "--to", "Bob", "too late"]);

    // (age the single ledger entry: third line is created-ms)
    let outbox_dir = std::path::PathBuf::from(format!("{key_a}.state")).join("outbox");
    let entry = std::fs::read_dir(&outbox_dir)
        .expect("read outbox dir")
        .next()
        .expect("one outbox entry")
        .expect("dir entry")
        .path();
    let aged: String = std::fs::read_to_string(&entry)
        .expect("read entry")
        .lines()
        .enumerate()
        .map(|(i, line)| if i == 2 { "1" } else { line }.to_string() + "\n")
        .collect();
    std::fs::write(&entry, aged).expect("age entry");

    // When: the relay is back and flush triggers run on both sides
    let (_router, _) = spawn_relay_at(secret, port).await;
    stdout_of(&cli(&["recv", "--key", &key_b]));
    stdout_of(&cli(&["recv", "--key", &key_a]));

    // Then: the message was NOT delivered (no retry past the window), but
    // alice still sees it — surfaced, never silently dropped
    let bob_inbox = stdout_of(&cli(&["recv", "--key", &key_b]));
    assert!(bob_inbox.contains("no new messages"), "got: {bob_inbox}");
    let conversation = conversation_of(&key_a);
    let history = stdout_of(&cli(&["history", "--key", &key_a, &conversation]));
    assert_eq!(history.lines().next(), Some("me: too late [pending]"));

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
