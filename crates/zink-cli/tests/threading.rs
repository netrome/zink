//! B5 end to end: client state persists between CLI invocations, so
//! messages thread into one conversation — same conversation id, advancing
//! `seq`, replies joining from the other side.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

/// `deposited <id> (conv <hex8>, seq <n>) ...` → (conv, seq)
fn conv_and_seq(sent: &str) -> (String, u64) {
    let conv = sent
        .split_once("(conv ")
        .and_then(|(_, rest)| rest.split_once(','))
        .map(|(conv, _)| conv.to_string())
        .expect("conv in output");
    let seq = sent
        .split_once("seq ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(seq, _)| seq.parse().ok())
        .expect("seq in output");
    (conv, seq)
}

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn threading__should_keep_a_conversation_across_cli_invocations() {
    // Given: a relay and two registered parties
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("threading");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    let pubkey_a = stdout_of(&cli(&["keygen", &key_a]));
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));
    cli(&["recv", "--key", &key_a, "--relay", &dial]);
    cli(&["recv", "--key", &key_b, "--relay", &dial]);
    let to_b = format!("{pubkey_b}@{dial}");
    let to_a = format!("{pubkey_a}@{dial}");

    // When: A sends twice (separate CLI invocations)
    let first = stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, "one"]));
    let second = stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, "two"]));
    let (conv_1, seq_1) = conv_and_seq(&first);
    let (conv_2, seq_2) = conv_and_seq(&second);

    // Then: same conversation, advancing seq — the DAG survived restarts
    assert_eq!(conv_1, conv_2);
    assert_eq!((seq_1, seq_2), (0, 1));

    // And: B receives both, and B's reply joins the same conversation
    let received = stdout_of(&cli(&["recv", "--key", &key_b, "--relay", &dial]));
    assert!(
        received.contains("one") && received.contains("two"),
        "got: {received}"
    );

    let reply = stdout_of(&cli(&["send", "--key", &key_b, "--to", &to_a, "three"]));
    let (reply_conv, reply_seq) = conv_and_seq(&reply);
    assert_eq!(reply_conv, conv_1);
    assert_eq!(reply_seq, 0); // B's first message in this conversation

    // And: after draining B's reply, A's next message keeps threading
    let drained = stdout_of(&cli(&["recv", "--key", &key_a, "--relay", &dial]));
    assert!(drained.contains("three"), "got: {drained}");
    let third = stdout_of(&cli(&["send", "--key", &key_a, "--to", &to_b, "four"]));
    let (conv_3, seq_3) = conv_and_seq(&third);
    assert_eq!(conv_3, conv_1);
    assert_eq!(seq_3, 2);

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
