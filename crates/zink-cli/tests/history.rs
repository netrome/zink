//! C3a end to end: stored conversations render as threaded, decrypted
//! history on both sides — including each device's *own* sent messages
//! (the self-wrap convention) — and blobs for stored messages come from
//! the local cache once fetched, outliving the relay.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

/// `my-record` prints the shareable payload as its first stdout line.
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

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn history__should_show_a_threaded_two_sided_conversation_on_both_devices() {
    // Given: a relay and two profiles that hold each other's records
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("history");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);

    // When: alice sends twice, bob receives and replies, alice receives
    stdout_of(&cli(&["send", "--key", &key_a, "--to", "Bob", "hi bob"]));
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "you there?",
    ]));
    stdout_of(&cli(&["recv", "--key", &key_b]));
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_b,
        "--to",
        "Alice",
        "hi alice!",
    ]));
    stdout_of(&cli(&["recv", "--key", &key_a]));

    // Then: both sides list one shared conversation with all 3 messages
    let listing_a = stdout_of(&cli(&["conversations", "--key", &key_a]));
    let listing_b = stdout_of(&cli(&["conversations", "--key", &key_b]));
    assert_eq!(listing_a.lines().count(), 1, "got: {listing_a}");
    assert!(listing_a.contains("3 message(s)"), "got: {listing_a}");
    assert!(listing_a.contains("with Bob"), "got: {listing_a}");
    assert!(listing_b.contains("3 message(s)"), "got: {listing_b}");
    assert!(listing_b.contains("with Alice"), "got: {listing_b}");
    let conversation_a = listing_a
        .split_whitespace()
        .next()
        .expect("conversation id");
    let conversation_b = listing_b
        .split_whitespace()
        .next()
        .expect("conversation id");
    assert_eq!(conversation_a, conversation_b, "one shared conversation id");

    // Then: each side's history is complete, ordered, and fully decrypted —
    // own sent messages included (self-wrap), via an id *prefix* lookup
    let history_a = stdout_of(&cli(&["history", "--key", &key_a, &conversation_a[..12]]));
    assert_eq!(
        history_a.lines().collect::<Vec<_>>(),
        ["me: hi bob", "me: you there?", "Bob: hi alice!"],
    );
    let history_b = stdout_of(&cli(&["history", "--key", &key_b, conversation_b]));
    assert_eq!(
        history_b.lines().collect::<Vec<_>>(),
        ["Alice: hi bob", "Alice: you there?", "me: hi alice!"],
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn reply__should_thread_into_the_conversation_and_skip_unknown_participants() {
    // Given: alice starts a conversation with bob (record) AND carol (raw
    // key — bob holds no record for her)
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("reply");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    let key_c = key_path(&dir, "carol.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_c]);
    let pubkey_c = stdout_of(&cli(&["pubkey", &key_c]));
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);

    let carol_raw = format!("{pubkey_c}@{dial}");
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "--to",
        &carol_raw,
        "hello group",
    ]));
    stdout_of(&cli(&["recv", "--key", &key_b]));
    let conversation = stdout_of(&cli(&["conversations", "--key", &key_b]))
        .split_whitespace()
        .next()
        .expect("conversation id")
        .to_string();

    // When: bob replies by conversation id — he can reach alice, not carol
    let output = cli(&["reply", "--key", &key_b, &conversation[..12], "hi all"]);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let replied = stdout_of(&output);
    assert!(replied.contains("to 1 relay(s)"), "got: {replied}");
    assert!(
        stderr.contains(&pubkey_c[..8]) && stderr.contains("no contact record"),
        "unknown participant not surfaced: {stderr}"
    );

    // Then: the reply threads into the same conversation on alice's side
    stdout_of(&cli(&["recv", "--key", &key_a]));
    let history_a = stdout_of(&cli(&["history", "--key", &key_a, &conversation]));
    assert_eq!(
        history_a.lines().collect::<Vec<_>>(),
        ["me: hello group", "Bob: hi all"],
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn history__should_serve_blobs_from_the_local_cache_once_the_relay_is_gone() {
    // Given: alice sent bob an image; bob received the message (not the blobs)
    let (router, dial) = spawn_relay().await;
    let dir = temp_dir("history-blobs");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    let record_a = record_payload(&key_a, "Alice", &dial);
    let record_b = record_payload(&key_b, "Bob", &dial);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);

    let image: Vec<u8> = (0..50_000u32).map(|i| (i % 249) as u8).collect();
    let image_path = dir.join("photo.bin");
    std::fs::write(&image_path, &image).expect("write image");
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "--image",
        &image_path.to_string_lossy(),
        "here's the photo",
    ]));
    stdout_of(&cli(&["recv", "--key", &key_b]));
    let conversation = stdout_of(&cli(&["conversations", "--key", &key_b]))
        .split_whitespace()
        .next()
        .expect("conversation id")
        .to_string();

    // When: bob fetches the blob once (relay-served, then cached)…
    let first = dir.join("first");
    std::fs::create_dir_all(&first).expect("create blobs dir");
    let fetched = stdout_of(&cli(&[
        "history",
        "--key",
        &key_b,
        "--blobs-dir",
        &first.to_string_lossy(),
        &conversation,
    ]));
    assert!(fetched.contains("saved full blob"), "got: {fetched}");

    // …and the relay then disappears for good
    drop(router);

    // Then: bob still gets the image (his cache), and alice still gets her
    // own (cached at send — her home relay never held it for her)
    let again = dir.join("again");
    std::fs::create_dir_all(&again).expect("create blobs dir");
    stdout_of(&cli(&[
        "history",
        "--key",
        &key_b,
        "--blobs-dir",
        &again.to_string_lossy(),
        &conversation,
    ]));
    let alice_dir = dir.join("alice-own");
    std::fs::create_dir_all(&alice_dir).expect("create blobs dir");
    stdout_of(&cli(&[
        "history",
        "--key",
        &key_a,
        "--blobs-dir",
        &alice_dir.to_string_lossy(),
        &conversation,
    ]));
    for blobs_dir in [&again, &alice_dir] {
        let saved: Vec<Vec<u8>> = std::fs::read_dir(blobs_dir)
            .expect("read blobs dir")
            .map(|entry| std::fs::read(entry.expect("dir entry").path()).expect("read blob"))
            .collect();
        assert_eq!(saved.len(), 1, "in {blobs_dir:?}");
        assert_eq!(saved[0], image, "in {blobs_dir:?}");
    }

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
