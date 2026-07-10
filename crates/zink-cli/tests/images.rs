//! B3 end to end: CLI sends an image (thumbnail + full-res); the recipient
//! fetches both blobs from the relay cache and decrypts them.

mod common;

use common::{cli, key_path, spawn_relay, stdout_of, temp_dir};

#[tokio::test(flavor = "multi_thread")]
#[allow(non_snake_case)]
async fn images__should_deliver_thumbnail_and_full_res_end_to_end() {
    // Given: a relay, sender A, recipient B, and two "image" files
    let (_router, dial) = spawn_relay().await;
    let dir = temp_dir("images");
    let key_a = key_path(&dir, "a.key");
    let key_b = key_path(&dir, "b.key");
    cli(&["keygen", &key_a]);
    let pubkey_b = stdout_of(&cli(&["keygen", &key_b]));
    cli(&["recv", "--key", &key_b, "--relay", &dial]);

    let full: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let thumb: Vec<u8> = (0..2_000u32).map(|i| (i % 13) as u8).collect();
    let full_path = dir.join("photo.bin");
    let thumb_path = dir.join("photo-thumb.bin");
    std::fs::write(&full_path, &full).expect("write full");
    std::fs::write(&thumb_path, &thumb).expect("write thumb");

    // When: A sends the image, B receives with a blobs dir
    let to_b = format!("{pubkey_b}@{dial}");
    let sent = stdout_of(&cli(&[
        "send",
        "--key",
        &key_a,
        "--to",
        &to_b,
        "--image",
        &full_path.to_string_lossy(),
        "--thumb",
        &thumb_path.to_string_lossy(),
        "here's the photo",
    ]));
    assert!(sent.contains("(2 blob(s))"), "got: {sent}");

    let blobs_dir = dir.join("received");
    std::fs::create_dir_all(&blobs_dir).expect("create blobs dir");
    let received = stdout_of(&cli(&[
        "recv",
        "--key",
        &key_b,
        "--relay",
        &dial,
        "--blobs-dir",
        &blobs_dir.to_string_lossy(),
    ]));

    // Then: the text arrived and both blobs decrypt to the original bytes
    assert!(received.contains("here's the photo"), "got: {received}");
    assert!(received.contains("saved thumbnail blob"), "got: {received}");
    assert!(received.contains("saved full blob"), "got: {received}");

    let mut saved: Vec<Vec<u8>> = std::fs::read_dir(&blobs_dir)
        .expect("read blobs dir")
        .map(|entry| std::fs::read(entry.expect("dir entry").path()).expect("read saved blob"))
        .collect();
    saved.sort_by_key(|bytes| bytes.len());
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0], thumb);
    assert_eq!(saved[1], full);

    std::fs::remove_dir_all(&dir).expect("clean up temp dir");
}
