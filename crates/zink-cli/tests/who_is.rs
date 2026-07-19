//! D1b acceptance: the one-way-add reply hole (who-is-this.md §1). Carol
//! scanned Alice's record and messaged her; Alice can't reply — no record
//! for Carol. Alice asks her contacts who the key is, learns Carol's
//! record from Bob (their mutual contact), promotes it, and replies.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{cli, key_path, spawn_iroh_relay, spawn_relay, stdout_of, temp_dir};

/// Set a homed profile (mailbox + iroh relay URL — dialable by key) and
/// return the shareable record payload.
fn record_payload(key: &str, name: &str, relay_spec: &str) -> String {
    stdout_of(&cli(&[
        "my-record",
        "--key",
        key,
        "--name",
        name,
        "--relay",
        relay_spec,
    ]))
    .lines()
    .next()
    .expect("record payload line")
    .to_string()
}

/// The responder process — killed even when an assertion panics.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn who_is__should_resolve_a_one_way_add_through_a_mutual_contact() {
    // Given: one relay service (in-process mailbox + iroh relay server,
    // paired in one spec). Bob knows Alice and Carol; Alice knows only
    // Bob; Carol added Alice one-way and messaged her.
    let (_router, dial) = spawn_relay().await;
    let (_iroh_relay, url) = spawn_iroh_relay().await;
    let spec = format!("{dial}#{url}");
    let dir = temp_dir("whois");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    let key_c = key_path(&dir, "carol.key");
    for key in [&key_a, &key_b, &key_c] {
        cli(&["keygen", key]);
    }
    let record_a = record_payload(&key_a, "Alice", &spec);
    let record_b = record_payload(&key_b, "Bob", &spec);
    let record_c = record_payload(&key_c, "Carol", &spec);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    cli(&["contact-add", "--key", &key_b, &record_c]);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_c, &record_a]);
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_c,
        "--to",
        "Alice",
        "hi, it's carol",
    ]));
    let received = stdout_of(&cli(&["recv", "--key", &key_a]));
    assert!(received.contains("hi, it's carol"), "got: {received}");
    let carol_hex = stdout_of(&cli(&["pubkey", &key_c]));

    // When: Bob comes online (a listener serves the sync ALPN) and Alice
    // asks her contacts about the unknown key — polled, since Bob's
    // endpoint needs a moment to home to the relay after spawning
    let _bob = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_zink-cli"))
            .args(["listen", "--key", &key_b])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bob"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let answer = loop {
        let output = stdout_of(&cli(&["who-is", "--key", &key_a, &carol_hex]));
        if output.contains("Carol") {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "no answer from Bob; last: {output}"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    // Then: Bob's answer names Carol, with provenance and a payload
    assert!(answer.contains("Bob holds a record"), "got: {answer}");
    assert!(answer.contains("calls themself \"Carol\""), "got: {answer}");
    assert!(
        answer.contains("records held by Bob"),
        "resolution should carry provenance; got: {answer}"
    );
    let payload = answer
        .split_whitespace()
        .find(|token| token.starts_with("ZINK:"))
        .expect("a shareable payload in the answer");

    // When: Alice promotes the learned record — the one explicit act —
    // and replies (threads into the existing conversation)
    let added = stdout_of(&cli(&["contact-add", "--key", &key_a, payload]));
    assert!(added.contains("\"Carol\""), "petname prefilled: {added}");
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Carol",
        "got you now, carol",
    ]));

    // Then: Carol receives the reply — the one-way-add hole is closed
    let replied = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(replied.contains("got you now, carol"), "got: {replied}");

    let _ = std::fs::remove_dir_all(dir);
}
