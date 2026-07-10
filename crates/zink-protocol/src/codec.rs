//! Canonical encoding and content hashing (SPEC §10).

use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};

/// Decoding failure on bytes from the outside world. Never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The leading `version` tag is not one this implementation understands.
    UnsupportedVersion { found: u16 },
    /// Truncated, trailing, or otherwise unparseable bytes.
    Malformed,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found } => {
                write!(f, "unsupported format version {found}")
            }
            Self::Malformed => write!(f, "malformed input"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub(crate) fn canonical_bytes<T: BorshSerialize>(value: &T) -> Vec<u8> {
    // Encoding an in-memory value into a Vec cannot fail; this is not input parsing.
    borsh::to_vec(value).expect("BORSH encoding to a Vec is infallible")
}

pub(crate) fn content_hash<T: BorshSerialize>(value: &T) -> [u8; 32] {
    *blake3::hash(&canonical_bytes(value)).as_bytes()
}

/// Decode a wire object whose first field is the `u16` format version.
///
/// The version is checked *before* the full parse so an unknown future version is
/// surfaced as such, never misparsed (SPEC §10).
pub(crate) fn decode_versioned<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, DecodeError> {
    let version_bytes = bytes.get(..2).ok_or(DecodeError::Malformed)?;
    let version = u16::from_le_bytes(version_bytes.try_into().expect("slice of length 2"));
    if version != crate::FORMAT_VERSION {
        return Err(DecodeError::UnsupportedVersion { found: version });
    }
    T::try_from_slice(bytes).map_err(|_| DecodeError::Malformed)
}
