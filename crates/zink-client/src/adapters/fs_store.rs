//! The store port over the local filesystem: a key is a path under `root`,
//! one segment per path component, so the on-disk layout is exactly the
//! key layout. Reads and writes are plain blocking `std::fs` calls, as
//! `ClientState` has always made them — small files on a local disk.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ports::store::{Delete, Get, Key, List, Put, Segment, StorageError, StoreOp};

#[derive(Debug, Clone)]
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsStore { root: root.into() }
    }

    fn path(&self, key: &Key) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(key.segments().iter().map(Segment::as_str));
        path
    }
}

impl Get for FsStore {
    async fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, StorageError> {
        match std::fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(failure(StoreOp::Get, key, e)),
        }
    }
}

impl Put for FsStore {
    async fn put(&self, key: &Key, bytes: &[u8]) -> Result<(), StorageError> {
        let path = self.path(key);
        let parent = path.parent().expect("a key path is below the root");
        std::fs::create_dir_all(parent).map_err(|e| failure(StoreOp::Put, key, e))?;
        write_atomic(&path, bytes).map_err(|e| failure(StoreOp::Put, key, e))
    }
}

impl Delete for FsStore {
    async fn delete(&self, key: &Key) -> Result<(), StorageError> {
        match std::fs::remove_file(self.path(key)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(failure(StoreOp::Delete, key, e)),
            _ => Ok(()),
        }
    }
}

impl List for FsStore {
    /// Names that aren't segments are skipped: this store's own scratch
    /// files (by construction) and anything else no key could have written.
    async fn list(&self, dir: &Key) -> Result<Vec<Segment>, StorageError> {
        let entries = match std::fs::read_dir(self.path(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failure(StoreOp::List, dir, e)),
        };
        let mut children: Vec<Segment> = entries
            .flatten()
            .filter_map(|entry| Segment::new(entry.file_name().to_str()?).ok())
            .collect();
        children.sort();
        Ok(children)
    }
}

fn failure(op: StoreOp, key: &Key, error: std::io::Error) -> StorageError {
    StorageError {
        op,
        key: key.clone(),
        detail: error.to_string(),
    }
}

/// Per-process counter so each write gets its own scratch file.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Scratch file + rename: a crash mid-write never leaves a torn value.
/// The scratch name is unique per *write*, not just per process: one
/// `ClientState` is shared by concurrent tasks (subscription loops and
/// command handlers), so two can write the same key at once — a pid-only
/// name made them collide on one scratch file, the first rename removed
/// it and the second failed. Whichever renames last wins. The `~` keeps
/// scratch names outside the segment alphabet, so `list` never shows one.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let scratch = scratch_path(path);
    std::fs::write(&scratch, bytes)?;
    std::fs::rename(&scratch, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&scratch);
    })
}

fn scratch_path(path: &Path) -> PathBuf {
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .expect("a key path has a file name")
        .to_owned();
    name.push(format!("~{}.{seq}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::ports::store::contract::{key, store_contract};

    store_contract!(fixture);

    #[tokio::test]
    async fn put__should_write_the_bytes_at_the_path_the_key_names() {
        // Given
        let (store, dir) = fixture("put-path");

        // When
        store
            .put(&key("contacts/ab.record"), b"bytes")
            .await
            .expect("put");

        // Then
        let on_disk = std::fs::read(dir.0.join("contacts").join("ab.record"));
        assert_eq!(on_disk.expect("file at the mapped path"), b"bytes");
    }

    #[tokio::test]
    async fn list__should_hide_an_orphaned_scratch_file() {
        // Given
        let (store, dir) = fixture("list-scratch");
        store.put(&key("d/a"), b"").await.expect("put");
        std::fs::write(scratch_path(&dir.0.join("d").join("b")), b"torn").expect("orphan");

        // When
        let children = store.list(&key("d")).await.expect("list");

        // Then
        assert_eq!(children, vec![Segment::new("a").unwrap()]);
    }

    #[test]
    fn scratch_path__should_differ_per_write_to_one_key() {
        // Given
        let path = Path::new("/state/contacts/ab.record");

        // Then
        assert_ne!(scratch_path(path), scratch_path(path));
    }

    /// A fresh store in a per-test temp dir, removed when the guard drops
    /// (tests run in parallel, so the dir is namespaced by test name).
    fn fixture(test: &str) -> (FsStore, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "zink-client-fs-store-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (FsStore::new(&dir), TempDir(dir))
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
