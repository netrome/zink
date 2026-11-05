//! Client state I/O behind ports (project 9): a narrow, byte-level store
//! under typed, path-like keys. Layout and encoding are the caller's
//! (`ClientState`); a store only moves bytes. The production adapter is
//! `crate::adapters::fs_store::FsStore`; the double is `TestStore`.
//!
//! The contract every implementation keeps (pinned by `contract`'s suite,
//! run against each):
//! - `get` is `Ok(None)` for an absent key; `Err` means a real failure.
//! - `put` replaces atomically — a reader sees the old bytes or the new,
//!   never a torn write — and creates missing parents.
//! - `delete` is idempotent: deleting an absent key succeeds.
//! - `list(dir)` is the immediate children's names, sorted; a missing dir
//!   lists empty. A child may be a value or a directory, and a directory
//!   emptied by deletes may keep listing. A store's own scratch entries
//!   never list.
//! - A key names a value or a directory, never both: a `put` under a value
//!   or onto a directory fails.
//!
//! What a caller may NOT assume: atomicity across keys, or durability.
//! Ordering multi-key writes so a crash between them stays safe is the
//! caller's job — and what the fault-injecting double exists to test.

use std::fmt;
use std::future::Future;

use thiserror::Error;

/// The full storage capability, as one bound for `ClientState`'s type
/// parameter. Blanket-implemented — nothing implements it by name; helpers
/// take the narrowest verb they exercise.
pub trait Store: Get + Put + Delete + List + Clone {}
impl<T: Get + Put + Delete + List + Clone> Store for T {}

pub trait Get: Send + Sync + 'static {
    fn get(&self, key: &Key) -> impl Future<Output = Result<Option<Vec<u8>>, StorageError>> + Send;
}

pub trait Put: Send + Sync + 'static {
    fn put(&self, key: &Key, bytes: &[u8])
    -> impl Future<Output = Result<(), StorageError>> + Send;
}

pub trait Delete: Send + Sync + 'static {
    fn delete(&self, key: &Key) -> impl Future<Output = Result<(), StorageError>> + Send;
}

pub trait List: Send + Sync + 'static {
    fn list(&self, dir: &Key) -> impl Future<Output = Result<Vec<Segment>, StorageError>> + Send;
}

/// A path-like key: one or more segments. Well-formed by construction —
/// every segment is a legal, plain file name, so no key can escape a
/// store's root or name a separator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(Vec<Segment>);

impl Key {
    pub fn new(first: Segment) -> Self {
        Key(vec![first])
    }

    pub fn child(&self, segment: Segment) -> Self {
        let mut segments = self.0.clone();
        segments.push(segment);
        Key(segments)
    }

    pub fn segments(&self) -> &[Segment] {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut segments = self.0.iter();
        if let Some(first) = segments.next() {
            f.write_str(first.as_str())?;
        }
        for segment in segments {
            write!(f, "/{segment}")?;
        }
        Ok(())
    }
}

/// One key segment: 1–255 bytes of `[A-Za-z0-9_.-]`, never `.` or `..`.
/// The alphabet is what keeps it a legal file name everywhere, and leaves
/// characters free (`~`, …) for an adapter's scratch names, which can then
/// never collide with a key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Segment(String);

impl Segment {
    pub const MAX_LEN: usize = 255;

    pub fn new(name: &str) -> Result<Self, InvalidSegment> {
        let legal_char = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-');
        let well_formed = !name.is_empty()
            && name.len() <= Self::MAX_LEN
            && name != "."
            && name != ".."
            && name.bytes().all(legal_char);
        if well_formed {
            Ok(Segment(name.to_string()))
        } else {
            Err(InvalidSegment(name.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("not a key segment: {0:?}")]
pub struct InvalidSegment(pub String);

/// A store operation failed for a reason other than absence. Only ever
/// shown to a human (`ClientState` folds it into `Error::Storage`).
#[derive(Debug, Error)]
#[error("{op} {key}: {detail}")]
pub struct StorageError {
    pub op: StoreOp,
    pub key: Key,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Get,
    Put,
    Delete,
    List,
}

impl fmt::Display for StoreOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StoreOp::Get => "get",
            StoreOp::Put => "put",
            StoreOp::Delete => "delete",
            StoreOp::List => "list",
        })
    }
}

#[cfg(test)]
pub(crate) mod contract;
#[cfg(test)]
mod test_store;

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn segment__should_accept_hex_and_dotted_names() {
        for name in ["ab01", "profile.name", "x.tmp1", "a_b-c", ".next"] {
            assert!(Segment::new(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn segment__should_reject_names_that_are_not_plain_file_names() {
        for name in ["", ".", "..", "a/b", "a\\b", "a~1", "a b", "é", "a\0b"] {
            assert!(Segment::new(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn segment__should_reject_a_name_longer_than_a_file_name_may_be() {
        // Given
        let at_cap = "a".repeat(Segment::MAX_LEN);
        let over_cap = "a".repeat(Segment::MAX_LEN + 1);

        // Then
        assert!(Segment::new(&at_cap).is_ok());
        assert!(Segment::new(&over_cap).is_err());
    }

    #[test]
    fn key__should_display_as_a_slash_joined_path() {
        // Given
        let key = Key::new(seg("contacts")).child(seg("ab.record"));

        // Then
        assert_eq!(key.to_string(), "contacts/ab.record");
    }

    fn seg(name: &str) -> Segment {
        Segment::new(name).expect("valid segment")
    }
}
