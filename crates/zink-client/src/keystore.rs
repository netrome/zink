//! Device-key storage: a hex-encoded seed in a file. Plaintext on disk is
//! the dev-tool tradeoff for now; OS-keystore encryption is a later slice.
//! The file is owner-only (0600) — a plaintext seed must not be world-
//! readable on a shared machine (a stated MVP target is Linux desktop).

use rand_core::{OsRng, RngCore};
use zink_protocol::DeviceKey;

use crate::hex;

pub fn load(path: &str) -> Result<DeviceKey, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read key file {path}: {e}"))?;
    Ok(DeviceKey::from_seed(hex::parse32(content.trim())?))
}

/// Generate a fresh key and write it owner-only. Overwrites — the CLI's
/// `keygen` is an explicit act.
pub fn create(path: &str) -> Result<DeviceKey, String> {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    write_private(path.as_ref(), hex::encode(&seed).as_bytes())
        .map_err(|e| format!("write {path}: {e}"))?;
    Ok(DeviceKey::from_seed(seed))
}

/// Write a secret file with owner-only permissions (0600 on Unix). On other
/// platforms this is a plain write — desktop MVP target is Linux.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// First-run-friendly: load if present, silently create otherwise (the app
/// path — a phone has no `keygen` step).
pub fn load_or_create(path: &str) -> Result<DeviceKey, String> {
    if std::path::Path::new(path).exists() {
        load(path)
    } else {
        create(path)
    }
}
