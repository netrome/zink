//! Device-key storage: a hex-encoded seed in a file. Plaintext on disk is
//! the dev-tool tradeoff for now; OS-keystore encryption is a later slice.

use rand_core::{OsRng, RngCore};
use zink_protocol::DeviceKey;

use crate::hex;

pub fn load(path: &str) -> Result<DeviceKey, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read key file {path}: {e}"))?;
    Ok(DeviceKey::from_seed(hex::parse32(content.trim())?))
}

/// Generate a fresh key and write it. Overwrites — the CLI's `keygen` is an
/// explicit act.
pub fn create(path: &str) -> Result<DeviceKey, String> {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    std::fs::write(path, hex::encode(&seed)).map_err(|e| format!("write {path}: {e}"))?;
    Ok(DeviceKey::from_seed(seed))
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
