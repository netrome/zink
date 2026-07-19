//! D3c acceptance: send-to-self end to end. A phone pairs a laptop (two
//! one-way recognize acts); the phone's next organic message to alice
//! carries both device keys (the signed recipients ARE the announcement);
//! alice's client auto-learns the laptop's record from the phone (the D3b
//! mirror rule), sees the mutually-confirmed link evidence, and promotes
//! the laptop explicitly; her reply reaches BOTH devices. The fresh laptop
//! — empty contact store — bootstraps through its sibling (own-device
//! authorship legitimizes, the sibling answers the scoped auto-query) and
//! its reply reaches alice DIRECTLY, with the phone offline the whole time.

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
async fn send_to_self__should_carry_a_paired_device_into_a_conversation() {
    // Given: alice ↔ phone are mutual contacts; the laptop homes to its
    // OWN relay (a shared relay would fan alice's deposit to every
    // registered recipient and mask whether she really routes to the
    // laptop — the D2a lesson).
    let (_router, dial) = spawn_relay().await;
    let (_router_l, dial_l) = spawn_relay().await;
    let (_iroh_relay, url) = spawn_iroh_relay().await;
    let spec = format!("{dial}#{url}");
    let spec_l = format!("{dial_l}#{url}");
    let dir = temp_dir("multidevice");
    let key_a = key_path(&dir, "alice.key");
    let key_p = key_path(&dir, "phone.key");
    let key_l = key_path(&dir, "laptop.key");
    for key in [&key_a, &key_p, &key_l] {
        cli(&["keygen", key]);
    }
    let record_a = record_payload(&key_a, "Alice", &spec);
    let record_p = record_payload(&key_p, "mårten phone", &spec);
    cli(&["contact-add", "--key", &key_a, &record_p]);
    cli(&["contact-add", "--key", &key_p, &record_a]);
    let phone_hex = stdout_of(&cli(&["pubkey", &key_p]));
    let laptop_hex = stdout_of(&cli(&["pubkey", &key_l]));

    // Pairing = the one-way act run once in each direction. Order matters
    // for the records: the laptop recognizes first, so the laptop record
    // the phone then stores (and later serves) already carries the
    // laptop's reverse vouch — what upgrades alice's evidence to mutual.
    cli(&["recognize", "--key", &key_l, &record_p]);
    let record_l = record_payload(&key_l, "mårten laptop", &spec_l);
    cli(&["recognize", "--key", &key_p, &record_l]);

    // When: the phone's next organic message — no introduction mechanism
    stdout_of(&cli(&[
        "send", "--key", &key_p, "--to", "Alice", "hi alice",
    ]));

    // …the phone comes online to serve (its sibling and its contact will
    // both ask it things); a probe polls until it is reachable
    let _phone = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_zink-cli"))
            .args(["listen", "--key", &key_p])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn phone"),
    );
    let key_probe = key_path(&dir, "probe.key");
    cli(&["keygen", &key_probe]);
    record_payload(&key_probe, "Probe", &spec);
    cli(&["contact-add", "--key", &key_probe, &record_p]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = stdout_of(&cli(&["who-is", "--key", &key_probe, &phone_hex]));
        if output.contains("0 unreachable") {
            break;
        }
        assert!(Instant::now() < deadline, "phone never reachable: {output}");
        std::thread::sleep(Duration::from_millis(250));
    }

    // Then: the fresh laptop receives from its first inclusion onward, and
    // bootstraps alice's record through its sibling (own-device authorship
    // legitimizes the conversation; the phone answers the auto-query)
    let laptop_got = stdout_of(&cli(&["recv", "--key", &key_l]));
    assert!(laptop_got.contains("hi alice"), "got: {laptop_got}");

    // …alice drains: the signed recipients announce the laptop key; her
    // client auto-learns its record from the phone (the D3b mirror rule)
    let alice_got = stdout_of(&cli(&["recv", "--key", &key_a]));
    assert!(alice_got.contains("hi alice"), "got: {alice_got}");

    // …a freshness pull on the phone brings its current record (with the
    // phone's vouch); the laptop's evidence then reads mutually confirmed
    stdout_of(&cli(&["who-is", "--key", &key_a, &phone_hex]));
    let resolved = stdout_of(&cli(&["who-is", "--key", &key_a, &laptop_hex]));
    assert!(
        resolved.contains("mutually confirmed"),
        "expected mutual link evidence; got: {resolved}"
    );
    assert!(
        resolved.contains("mårten laptop"),
        "expected the self-claimed device name; got: {resolved}"
    );

    // …alice accepts the offer — one explicit act, nothing auto-adopts
    let payload = resolved
        .lines()
        .find_map(|line| {
            line.split_once("ZINK:")
                .map(|(_, rest)| format!("ZINK:{rest}"))
        })
        .expect("promotable payload in the who-is output");
    let added = stdout_of(&cli(&["contact-add", "--key", &key_a, &payload]));
    assert!(added.contains("mårten laptop"), "got: {added}");
    let label = stdout_of(&cli(&["conversations", "--key", &key_a]));
    assert!(
        label.contains("mårten phone") && label.contains("mårten laptop"),
        "expected both devices labeled; got: {label}"
    );

    // When: the phone goes OFFLINE — everything from here on must work
    // without it (contacts' fan-out is robustness, never load-bearing;
    // and the laptop's directness is only proven with the sibling dead)
    drop(_phone);

    // …alice replies once
    let conv_a = sole_conversation(&key_a);
    stdout_of(&cli(&["reply", "--key", &key_a, &conv_a, "hello both"]));

    // Then: BOTH of the person's devices receive the contact's reply
    let phone_got = stdout_of(&cli(&["recv", "--key", &key_p]));
    assert!(phone_got.contains("hello both"), "got: {phone_got}");
    let laptop_got = stdout_of(&cli(&["recv", "--key", &key_l]));
    assert!(laptop_got.contains("hello both"), "got: {laptop_got}");

    // When: the new device replies — empty contact store, routes learned
    // entirely through its sibling earlier
    let conv_l = sole_conversation(&key_l);
    stdout_of(&cli(&[
        "reply",
        "--key",
        &key_l,
        &conv_l,
        "from the new device",
    ]));

    // Then: the reply reaches the contact DIRECTLY (the phone has been
    // offline since before alice's reply), and the sibling's mailbox gets
    // its copy too
    let alice_got = stdout_of(&cli(&["recv", "--key", &key_a]));
    assert!(
        alice_got.contains("from the new device"),
        "got: {alice_got}"
    );
    let phone_got = stdout_of(&cli(&["recv", "--key", &key_p]));
    assert!(
        phone_got.contains("from the new device"),
        "got: {phone_got}"
    );

    let _ = std::fs::remove_dir_all(dir);
}
