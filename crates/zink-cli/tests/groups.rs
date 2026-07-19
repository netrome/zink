//! D2a acceptance: group membership through the DAG. Alice grows a 1:1
//! with Bob into a group by replying with Carol added (`reply --add` —
//! the signed recipients list is the announcement); everyone, including
//! the adder by name, threads ONE conversation (the groups.md §3 index
//! regression); a reply reaches a non-contact member through a learned
//! route; stop-including shrinks the reply set.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{cli, key_path, spawn_iroh_relay, spawn_relay, stdout_of, temp_dir};

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

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The single conversation id in a client's list — asserting the count is
/// exactly one IS the artifact-fork regression check.
fn sole_conversation(key: &str) -> String {
    let listing = stdout_of(&cli(&["conversations", "--key", key]));
    let ids: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| id.len() == 64)
        .collect();
    assert_eq!(ids.len(), 1, "expected exactly one conversation: {listing}");
    ids[0].to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn auto_query__should_learn_an_added_members_record_during_recv() {
    // Given: alice↔bob mutual contacts; bob knows carol; carol homes to
    // her OWN relay (the only way alice can ever reach her is a learned
    // record — a shared relay would mask routelessness, see below). Bob
    // grows the 1:1 by replying with carol added, then goes online to
    // serve. Alice does nothing but drain.
    let (_router, dial) = spawn_relay().await;
    let (_router_c, dial_c) = spawn_relay().await;
    let (_iroh_relay, url) = spawn_iroh_relay().await;
    let spec = format!("{dial}#{url}");
    let spec_c = format!("{dial_c}#{url}");
    let dir = temp_dir("autoquery");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    let key_c = key_path(&dir, "carol.key");
    for key in [&key_a, &key_b, &key_c] {
        cli(&["keygen", key]);
    }
    let record_a = record_payload(&key_a, "Alice", &spec);
    let record_b = record_payload(&key_b, "Bob", &spec);
    let record_c = record_payload(&key_c, "Carol", &spec_c);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    cli(&["contact-add", "--key", &key_b, &record_c]);
    let carol_hex = stdout_of(&cli(&["pubkey", &key_c]));
    let bob_hex = stdout_of(&cli(&["pubkey", &key_b]));
    stdout_of(&cli(&[
        "send", "--key", &key_b, "--to", "Alice", "hi alice",
    ]));
    let conv_b = sole_conversation(&key_b);
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_b,
        "--add",
        "Carol",
        &conv_b,
        "welcome carol",
    ]));

    // …bob comes online; a throwaway probe key polls until his endpoint
    // answers WhoIs connects (0 unreachable — a stranger gets NotHeld,
    // which still proves reachability)
    let _bob = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_zink-cli"))
            .args(["listen", "--key", &key_b])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bob"),
    );
    let key_probe = key_path(&dir, "probe.key");
    cli(&["keygen", &key_probe]);
    record_payload(&key_probe, "Probe", &spec);
    cli(&["contact-add", "--key", &key_probe, &record_b]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = stdout_of(&cli(&["who-is", "--key", &key_probe, &bob_hex]));
        if output.contains("0 unreachable") {
            break;
        }
        assert!(Instant::now() < deadline, "bob never reachable: {output}");
        std::thread::sleep(Duration::from_millis(250));
    }

    // When: alice drains — the scoped auto-query fires inside recv (bob
    // authored, so the conversation is legitimate; carol is an unknown
    // member; bob is the only dialable participant)
    let got = stdout_of(&cli(&["recv", "--key", &key_a]));
    assert!(got.contains("welcome carol"), "got: {got}");
    drop(_bob); // bob offline — nothing new can be learned from here on

    // Then: carol resolves from the store alone (the manual who-is below
    // reaches nobody: bob is gone, carol was never dialable)
    let resolved = stdout_of(&cli(&["who-is", "--key", &key_a, &carol_hex]));
    assert!(
        resolved.contains("records held by Bob"),
        "expected the auto-learned candidate; got: {resolved}"
    );

    // …and reply-to-all reaches carol through the auto-learned route,
    // with zero manual identity work by alice
    let conv_a = sole_conversation(&key_a);
    stdout_of(&cli(&["reply", "--key", &key_a, &conv_a, "hello everyone"]));
    let carol_got = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(carol_got.contains("hello everyone"), "got: {carol_got}");

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn groups__should_grow_thread_and_shrink_through_the_dag() {
    // Given: alice knows bob + carol; bob knows only alice (carol will be
    // his non-contact group member); carol knows nobody (receive-only).
    // Carol homes to her OWN mailbox relay: a shared relay would fan a
    // deposit out to every registered recipient in the envelope, masking
    // the no-route case this test needs to demonstrate.
    let (_router, dial) = spawn_relay().await;
    let (_router_c, dial_c) = spawn_relay().await;
    let (_iroh_relay, url) = spawn_iroh_relay().await;
    let spec = format!("{dial}#{url}");
    let spec_c = format!("{dial_c}#{url}");
    let dir = temp_dir("groups");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    let key_c = key_path(&dir, "carol.key");
    for key in [&key_a, &key_b, &key_c] {
        cli(&["keygen", key]);
    }
    let record_a = record_payload(&key_a, "Alice", &spec);
    let record_b = record_payload(&key_b, "Bob", &spec);
    let record_c = record_payload(&key_c, "Carol", &spec_c);
    cli(&["contact-add", "--key", &key_a, &record_b]);
    cli(&["contact-add", "--key", &key_a, &record_c]);
    cli(&["contact-add", "--key", &key_b, &record_a]);
    let carol_hex = stdout_of(&cli(&["pubkey", &key_c]));

    // When: a 1:1 becomes a group — alice replies with carol added
    stdout_of(&cli(&["send", "--key", &key_a, "--to", "Bob", "hi bob"]));
    let conv_a = sole_conversation(&key_a);
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_a,
        "--add",
        "Carol",
        &conv_a,
        "welcome carol",
    ]));

    // Then: both receive it; bob's history renders the derived [+ Carol]
    // delta (he has no contact record — the label falls back to hex)
    let bob_got = stdout_of(&cli(&["recv", "--key", &key_b]));
    assert!(bob_got.contains("welcome carol"), "got: {bob_got}");
    let carol_got = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(carol_got.contains("welcome carol"), "got: {carol_got}");
    let bob_history = stdout_of(&cli(&["history", "--key", &key_b, &conv_a]));
    assert!(
        bob_history.contains(&format!("[+ {}]", &carol_hex[..8])),
        "derived join delta expected; got: {bob_history}"
    );

    // When: the §3 regression — the adder sends BY NAME to the grown set
    stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        "Bob",
        "--to",
        "Carol",
        "threading check",
    ]));

    // Then: still exactly one conversation on both ends
    assert_eq!(sole_conversation(&key_a), conv_a);
    let bob_got = stdout_of(&cli(&["recv", "--key", &key_b]));
    assert!(bob_got.contains("threading check"), "got: {bob_got}");
    assert_eq!(sole_conversation(&key_b), conv_a);

    // When: bob replies with no record for carol — she has no route yet
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_b,
        &conv_a,
        "from bob, pre-route",
    ]));
    let carol_got = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(
        !carol_got.contains("from bob, pre-route"),
        "carol has no route yet; got: {carol_got}"
    );

    // …and bob learns carol's record from alice (who-is; alice listens to
    // serve, then stops before alice runs anything else)
    {
        let _alice = KillOnDrop(
            Command::new(env!("CARGO_BIN_EXE_zink-cli"))
                .args(["listen", "--key", &key_a])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn alice"),
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let output = stdout_of(&cli(&["who-is", "--key", &key_b, &carol_hex]));
            if output.contains("Carol") {
                break;
            }
            assert!(Instant::now() < deadline, "no answer; last: {output}");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_b,
        &conv_a,
        "carol via learned route",
    ]));

    // Then: the reply reaches the non-contact member — address, don't
    // trust (groups.md §2); bob still holds no contact record for carol
    let carol_got = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(
        carol_got.contains("carol via learned route"),
        "got: {carol_got}"
    );
    let bob_contacts = stdout_of(&cli(&["contacts", "--key", &key_b]));
    assert!(
        !bob_contacts.contains("Carol"),
        "carol must not have been promoted: {bob_contacts}"
    );

    // When: alice stops including carol — a plain send to bob only
    // threads (the {alice,bob} mapping predates the group) and its head
    // shrinks membership, so the next reply-all no longer reaches carol
    stdout_of(&cli(&["recv", "--key", &key_a]));
    stdout_of(&cli(&["send", "--key", &key_a, "--to", "Bob", "just us"]));
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_a,
        &conv_a,
        "current members only",
    ]));

    // Then: bob gets both; carol gets neither
    let bob_got = stdout_of(&cli(&["recv", "--key", &key_b]));
    assert!(bob_got.contains("just us") && bob_got.contains("current members only"));
    assert_eq!(
        sole_conversation(&key_a),
        conv_a,
        "stop-include must not fork"
    );
    let carol_got = stdout_of(&cli(&["recv", "--key", &key_c]));
    assert!(
        !carol_got.contains("just us") && !carol_got.contains("current members only"),
        "carol was stop-included; got: {carol_got}"
    );

    let _ = std::fs::remove_dir_all(dir);
}
