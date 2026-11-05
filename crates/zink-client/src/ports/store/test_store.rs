//! The store double: an in-memory map that keeps the port contract —
//! including its failures (a put under a value or onto a directory), so a
//! test can't pass on the double and fail on disk. Clones share one map;
//! the test keeps a clone as its handle.
//!
//! The crash control is a journal of every completed write. A process
//! killed after its n-th write leaves exactly the first n on disk — the
//! port promises no multi-key atomicity, so nothing else can be assumed —
//! and `crashed_after(n)` is that store. A test walks every n an act
//! spans, so a crash-ordering claim is checked at every point, not only
//! the one a comment names.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{Delete, Get, Key, List, Put, Segment, StorageError, StoreOp};

#[derive(Clone, Default)]
pub(crate) struct TestStore(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    values: BTreeMap<Key, Vec<u8>>,
    /// Every completed write, in order, since the store was created.
    journal: Vec<Write>,
}

#[derive(Clone)]
enum Write {
    Put(Key, Vec<u8>),
    Delete(Key),
}

impl TestStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// How many writes have completed — the crash-point cursor: note it
    /// before an act, and every n from there to the count after is a
    /// state the act can leave behind.
    pub(crate) fn writes(&self) -> usize {
        self.0.lock().unwrap().journal.len()
    }

    /// A fresh, independent store holding exactly the first `writes`
    /// writes — what a crash right after the `writes`-th leaves.
    pub(crate) fn crashed_after(&self, writes: usize) -> TestStore {
        let inner = self.0.lock().unwrap();
        let mut crashed = Inner::default();
        for write in &inner.journal[..writes] {
            crashed.apply(write.clone());
        }
        TestStore(Arc::new(Mutex::new(crashed)))
    }

    /// Drop everything under `dir` — what a user deleting a directory by
    /// hand does to the store. Journaled like any write, so crash states
    /// taken afterwards stay faithful.
    pub(crate) fn remove_under(&self, dir: &Key) {
        let mut inner = self.0.lock().unwrap();
        let doomed: Vec<Key> = inner
            .values
            .keys()
            .filter(|key| key.0.len() > dir.0.len() && key.0.starts_with(&dir.0))
            .cloned()
            .collect();
        for key in doomed {
            inner.apply(Write::Delete(key));
        }
    }
}

impl Inner {
    fn apply(&mut self, write: Write) {
        match &write {
            Write::Put(key, bytes) => {
                self.values.insert(key.clone(), bytes.clone());
            }
            Write::Delete(key) => {
                self.values.remove(key);
            }
        }
        self.journal.push(write);
    }
}

impl Get for TestStore {
    async fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.0.lock().unwrap().values.get(key).cloned())
    }
}

impl Put for TestStore {
    async fn put(&self, key: &Key, bytes: &[u8]) -> Result<(), StorageError> {
        let mut inner = self.0.lock().unwrap();
        let fail = |detail: &str| StorageError {
            op: StoreOp::Put,
            key: key.clone(),
            detail: detail.to_string(),
        };
        let segments = key.segments();
        let under_a_value = (1..segments.len())
            .any(|len| inner.values.contains_key(&Key(segments[..len].to_vec())));
        let onto_a_directory = inner
            .values
            .keys()
            .any(|existing| existing.0.len() > segments.len() && existing.0.starts_with(segments));
        if under_a_value {
            Err(fail("a parent is a value"))
        } else if onto_a_directory {
            Err(fail("the key is a directory"))
        } else {
            inner.apply(Write::Put(key.clone(), bytes.to_vec()));
            Ok(())
        }
    }
}

impl Delete for TestStore {
    async fn delete(&self, key: &Key) -> Result<(), StorageError> {
        self.0.lock().unwrap().apply(Write::Delete(key.clone()));
        Ok(())
    }
}

impl List for TestStore {
    async fn list(&self, dir: &Key) -> Result<Vec<Segment>, StorageError> {
        let depth = dir.0.len();
        let mut children: Vec<Segment> = self
            .0
            .lock()
            .unwrap()
            .values
            .keys()
            .filter(|key| key.0.len() > depth && key.0.starts_with(&dir.0))
            .map(|key| key.0[depth].clone())
            .collect();
        children.dedup(); // the map's key order groups each child's entries
        Ok(children)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::TestStore;
    use crate::ports::store::contract::{key, store_contract};
    use crate::ports::store::{Get, Put};

    fn fixture(_test: &str) -> (TestStore, ()) {
        (TestStore::new(), ())
    }

    store_contract!(fixture);

    #[tokio::test]
    async fn crashed_after__should_hold_exactly_the_first_writes() {
        // Given: three writes, the last replacing the first
        let store = TestStore::new();
        store.put(&key("a"), b"1").await.expect("put");
        store.put(&key("b"), b"2").await.expect("put");
        store.put(&key("a"), b"3").await.expect("put");

        // When
        let crashed = store.crashed_after(2);

        // Then: the third write never happened there
        assert_eq!(store.writes(), 3);
        assert_eq!(crashed.get(&key("a")).await.unwrap(), Some(b"1".to_vec()));
        assert_eq!(crashed.get(&key("b")).await.unwrap(), Some(b"2".to_vec()));
    }

    #[tokio::test]
    async fn crashed_after__should_be_independent_of_the_original() {
        // Given
        let store = TestStore::new();
        store.put(&key("a"), b"1").await.expect("put");
        let crashed = store.crashed_after(1);

        // When
        crashed
            .put(&key("a"), b"after the crash")
            .await
            .expect("put");

        // Then
        assert_eq!(store.get(&key("a")).await.unwrap(), Some(b"1".to_vec()));
    }

    #[tokio::test]
    async fn put__should_not_journal_a_refused_write() {
        // Given
        let store = TestStore::new();
        store.put(&key("a"), b"value").await.expect("put");

        // When: a contract-refused put (under a value)
        let _ = store.put(&key("a/b"), b"refused").await;

        // Then
        assert_eq!(store.writes(), 1);
    }
}
