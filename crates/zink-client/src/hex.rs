//! Hex helpers shared by the client and its edges (keys and ids are shown
//! and entered as hex everywhere).

use crate::error::Error;

pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse32(hex: &str) -> Result<[u8; 32], Error> {
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(Error::InvalidInput(format!(
            "expected 64 hex chars, got {}",
            hex.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| Error::InvalidInput(format!("invalid hex: {e}")))?;
    }
    Ok(out)
}
