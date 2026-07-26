//! Who may hold a mailbox here — **relay-operator policy**, not protocol
//! (SPEC §5.3). The protocol says nothing about who a relay serves; this is
//! the knob that lets an operator run one on a box that has other jobs and
//! still know the disk ceiling in advance.
//!
//! A port with two implementations, per STYLE.md: the real one reads a file
//! the operator edits, the open one is the default and preserves the
//! "anyone can run one, anyone can use it" posture for dev and for operators
//! who want it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zink_protocol::PublicKey;

/// The registration gate. Consulted once per `Register`; deposits into an
/// already-registered mailbox are not re-checked (removing a key stops new
/// mail at the next registration, it does not delete what is held —
/// discretion, not enforcement).
/// `Debug` so the protocol handler holding one stays `Debug` (iroh's
/// `ProtocolHandler` requires it). Policy objects are small and carry no
/// secrets — an allow-list of public keys is public by construction.
pub trait Admission: Send + Sync + std::fmt::Debug + 'static {
    fn permits(&self, key: &PublicKey) -> bool;
}

/// Today's behaviour: any key that connects may register. Still bounded, by
/// the service's `max_mailboxes` backstop — but first-come-first-served, so
/// a public relay can be crowded out by strangers. Prefer [`AllowListFile`]
/// on a box you care about.
#[derive(Debug, Clone, Copy)]
pub struct OpenToAll;

impl Admission for OpenToAll {
    fn permits(&self, _key: &PublicKey) -> bool {
        true
    }
}

/// Keys listed in a file the operator owns — one lowercase hex key per line;
/// blank lines and `#` comments ignored.
///
/// **Re-read on every check**, deliberately: `echo <key> >> allowed-keys`
/// then takes effect immediately, with no restart and no reload signal. The
/// file is a handful of lines and registrations are rare (once per client
/// session), so the syscall is irrelevant next to the operational win.
///
/// A missing or unreadable file permits **nobody**. Failing closed is the
/// only safe direction: the operator reached for this because the box has
/// other things on it, and a typo'd path must not silently reopen the relay
/// to everyone.
#[derive(Debug, Clone)]
pub struct AllowListFile {
    path: PathBuf,
}

impl AllowListFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The keys currently listed. Unparseable lines are skipped with a
    /// warning rather than rejecting the whole file — one fat-fingered line
    /// must not lock out every friend.
    pub fn keys(&self) -> BTreeSet<PublicKey> {
        parse_allow_list(&std::fs::read_to_string(&self.path).unwrap_or_default())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Admission for AllowListFile {
    fn permits(&self, key: &PublicKey) -> bool {
        self.keys().contains(key)
    }
}

/// Pure parse, so the format is testable without touching a disk.
pub fn parse_allow_list(contents: &str) -> BTreeSet<PublicKey> {
    contents
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| match parse_key(line) {
            Some(key) => Some(key),
            None => {
                tracing::warn!(line, "skipping an unparseable allow-list entry");
                None
            }
        })
        .collect()
}

fn parse_key(hex: &str) -> Option<PublicKey> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(PublicKey(bytes))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    fn hex_of(key: &PublicKey) -> String {
        key.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn parse_allow_list__should_read_keys_and_ignore_comments_and_blanks() {
        // Given
        let contents = format!(
            "# my friends\n{}\n\n  {}  # laptop\n",
            hex_of(&key(1)),
            hex_of(&key(2))
        );

        // When
        let keys = parse_allow_list(&contents);

        // Then
        assert_eq!(keys, BTreeSet::from([key(1), key(2)]));
    }

    #[test]
    fn parse_allow_list__should_skip_a_bad_line_rather_than_reject_the_file() {
        // Given: one fat-fingered entry between two good ones
        let contents = format!("{}\nnot-a-key\n{}\n", hex_of(&key(1)), hex_of(&key(2)));

        // When
        let keys = parse_allow_list(&contents);

        // Then: the typo costs its own line, not everyone's access
        assert_eq!(keys, BTreeSet::from([key(1), key(2)]));
    }

    #[test]
    fn allow_list_file__should_permit_nobody_when_the_file_is_missing() {
        // Given: a path that does not exist (a typo'd --allow-list)
        let list = AllowListFile::new("/nonexistent/zink-allow-list");

        // When / Then: fails closed — a missing file must never read as "open"
        assert!(!list.permits(&key(1)));
    }

    #[test]
    fn open_to_all__should_permit_any_key() {
        assert!(OpenToAll.permits(&key(9)));
    }
}
