//! D1d acceptance (headless half): an avatar set on one client renders on
//! another, with the relay never holding anything but ciphertext — the key
//! travels only inside the signed Avatar claim in the record.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn avatar__should_render_across_clients_with_ciphertext_on_the_relay() {
    // Given: alice with a profile + avatar (pushed to her home relay's blob
    // cache); her record — carrying the claim — added by bob
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("avatar");
    let key_a = key_path(&dir, "alice.key");
    let key_b = key_path(&dir, "bob.key");
    cli(&["keygen", &key_a]);
    cli(&["keygen", &key_b]);
    stdout_of(&cli(&[
        "my-record",
        "--key",
        &key_a,
        "--name",
        "Alice",
        "--relay",
        &dial,
    ]));
    let image = dir.join("avatar.src");
    let portrait = b"pretend this is a tiny jpeg \xFF\xD8\xFF portrait of alice";
    std::fs::write(&image, portrait).expect("write source image");
    let set = stdout_of(&cli(&[
        "set-avatar",
        "--key",
        &key_a,
        &image.to_string_lossy(),
    ]));
    assert!(set.contains("pushed to 1 relay(s)"), "got: {set}");
    let hash = set
        .split_whitespace()
        .nth(3)
        .expect("hash in set-avatar output")
        .to_string();
    // The record printed *after* set-avatar carries the avatar claim.
    let record = stdout_of(&cli(&["my-record", "--key", &key_a]))
        .lines()
        .next()
        .expect("record payload")
        .to_string();
    stdout_of(&cli(&["contact-add", "--key", &key_b, &record]));

    // When: bob fetches alice's avatar (relay blob cache → decrypt → cache)
    let out = dir.join("avatar.got");
    let fetched = stdout_of(&cli(&[
        "avatar",
        "--key",
        &key_b,
        "--out",
        &out.to_string_lossy(),
        "Alice",
    ]));

    // Then: the rendered bytes are alice's original image…
    assert!(fetched.starts_with("avatar:"), "got: {fetched}");
    assert_eq!(std::fs::read(&out).expect("fetched avatar"), portrait);

    // …while what travelled and what rests in bob's cache — the same bytes
    // the relay cached, by content address — is ciphertext
    let cached = std::fs::read(
        std::path::PathBuf::from(format!("{key_b}.state"))
            .join("blobs")
            .join(&hash),
    )
    .expect("ciphertext in bob's cache");
    assert_ne!(cached, portrait.to_vec(), "never plaintext at rest");
    assert!(
        !cached
            .windows(portrait.len().min(16))
            .any(|window| window == &portrait[..portrait.len().min(16)]),
        "no plaintext fragment in the cached blob"
    );

    let _ = std::fs::remove_dir_all(dir);
}
