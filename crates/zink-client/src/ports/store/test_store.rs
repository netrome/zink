//! The store double: an in-memory map that keeps the port contract —
//! including its failures (a put under a value or onto a directory), so a
//! test can't pass on the double and fail on disk. Clones share one map;
//! the test keeps a clone as its handle. Fault controls (fail the n-th
//! put, stop persisting at a crash point) are project 9 S3.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{Delete, Get, Key, List, Put, Segment, StorageError, StoreOp};

#[derive(Clone, Default)]
pub(crate) struct TestStore(Arc<Mutex<BTreeMap<Key, Vec<u8>>>>);

impl TestStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl Get for TestStore {
    async fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
}

impl Put for TestStore {
    async fn put(&self, key: &Key, bytes: &[u8]) -> Result<(), StorageError> {
        let mut values = self.0.lock().unwrap();
        let fail = |detail: &str| StorageError {
            op: StoreOp::Put,
            key: key.clone(),
            detail: detail.to_string(),
        };
        let segments = key.segments();
        let under_a_value =
            (1..segments.len()).any(|len| values.contains_key(&Key(segments[..len].to_vec())));
        let onto_a_directory = values
            .keys()
            .any(|existing| existing.0.len() > segments.len() && existing.0.starts_with(segments));
        if under_a_value {
            Err(fail("a parent is a value"))
        } else if onto_a_directory {
            Err(fail("the key is a directory"))
        } else {
            values.insert(key.clone(), bytes.to_vec());
            Ok(())
        }
    }
}

impl Delete for TestStore {
    async fn delete(&self, key: &Key) -> Result<(), StorageError> {
        self.0.lock().unwrap().remove(key);
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
    use crate::ports::store::contract::store_contract;

    fn fixture(_test: &str) -> (TestStore, ()) {
        (TestStore::new(), ())
    }

    store_contract!(fixture);
}
