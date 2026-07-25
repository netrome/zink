//! B2 end to end: 1→N fan-out on one relay, and cross-relay dedup by id.
//! Plus De6a: a drain is best-effort *per relay* — one unreachable relay
//! costs its own mail, never another relay's.

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
    assert!(sent.ends_with("via 1 relay(s)"), "got: {sent}");

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
    assert!(sent.ends_with("via 2 relay(s)"), "got: {sent}");

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

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn recv__should_drain_the_healthy_relay_when_another_is_unreachable() {
    // Given: bob's mailbox on two relays, one message waiting on the SECOND
    // one only — then the first relay goes down. Before De6a a `?` in recv's
    // per-relay loop aborted the pass on the first unreachable relay, so
    // this mail stayed invisible until an unrelated relay came back.
    let (router_1, dial_1) = spawn_relay().await;
    let (router_2, dial_2) = spawn_relay().await;
    let dir = temp_dir("recv-partial");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));
    cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]);
    let text = "mail on the relay that stayed up";
    let to_b = format!("{pubkey_b}@{dial_2}");
    stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, text]));
    drop(router_1);

    // When: bob drains both, the dead one FIRST (the abort order that used
    // to lose the mail)
    let output = cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]);

    // Then: the healthy relay's mail arrives, and the failure is reported
    // rather than swallowed — a partial view that says it is partial
    let stdout = stdout_of(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains(text), "got: {stdout}");
    assert!(
        stderr.contains(&format!("{dial_1} not drained")),
        "the dead relay should be named; got: {stderr}"
    );
    assert!(
        !stderr.contains(&format!("{dial_2} not drained")),
        "the healthy relay must not be reported failed; got: {stderr}"
    );

    // When: the second relay goes down too — nothing can be drained anywhere
    drop(router_2);
    let output = cli(&[
        "recv", "--key", &key_b, "--relay", &dial_1, "--relay", &dial_2,
    ]);

    // Then: still an error. "Best-effort per relay" is not "silently succeed
    // with nothing" — a caller that asked for mail and reached no relay at
    // all must see why.
    assert!(
        !output.status.success(),
        "a drain reaching no relay must fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
