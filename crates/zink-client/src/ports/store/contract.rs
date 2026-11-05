//! The store contract as one suite, run against every implementation: the
//! trait plus this suite is the stable artifact, not any one store (ADR
//! 0004). An implementation's tests invoke `store_contract!` with a
//! fixture function `fn(&str) -> (S, Guard)`: a fresh, empty store per
//! test (named by the test, for adapters that need a unique location),
//! plus whatever must live until the test ends (the fs adapter's temp-dir
//! cleanup; `()` for the double).

#![allow(non_snake_case)]

use super::{Delete, Get, Key, List, Put, Segment, Store};

/// Expands to one `#[tokio::test]` per contract clause.
macro_rules! store_contract {
    ($fixture:path) => {
        $crate::ports::store::contract::store_contract!(@tests $fixture;
            get__should_return_none_for_an_absent_key,
            put__should_make_bytes_readable,
            put__should_store_empty_bytes_as_present,
            put__should_replace_existing_bytes,
            put__should_create_missing_parents,
            put__should_fail_under_a_value,
            put__should_fail_onto_a_directory,
            delete__should_remove_the_value,
            delete__should_succeed_for_an_absent_key,
            list__should_return_immediate_children_sorted,
            list__should_return_empty_for_a_missing_dir,
            clones__should_share_one_store,
        );
    };
    (@tests $fixture:path; $($clause:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $clause() {
                let (store, _guard) = $fixture(stringify!($clause));
                $crate::ports::store::contract::$clause(store).await;
            }
        )*
    };
}
pub(crate) use store_contract;

pub(crate) async fn get__should_return_none_for_an_absent_key(store: impl Get) {
    assert_eq!(store.get(&key("a/b")).await.expect("get"), None);
}

pub(crate) async fn put__should_make_bytes_readable(store: impl Get + Put) {
    // When
    store.put(&key("a"), b"bytes").await.expect("put");

    // Then
    assert_eq!(
        store.get(&key("a")).await.expect("get"),
        Some(b"bytes".to_vec())
    );
}

pub(crate) async fn put__should_store_empty_bytes_as_present(store: impl Get + Put) {
    // When
    store.put(&key("a"), b"").await.expect("put");

    // Then
    assert_eq!(store.get(&key("a")).await.expect("get"), Some(Vec::new()));
}

pub(crate) async fn put__should_replace_existing_bytes(store: impl Get + Put) {
    // Given
    store.put(&key("a"), b"old and longer").await.expect("put");

    // When
    store.put(&key("a"), b"new").await.expect("put");

    // Then
    assert_eq!(
        store.get(&key("a")).await.expect("get"),
        Some(b"new".to_vec())
    );
}

pub(crate) async fn put__should_create_missing_parents(store: impl Get + Put) {
    // When
    store.put(&key("a/b/c"), b"deep").await.expect("put");

    // Then
    assert_eq!(
        store.get(&key("a/b/c")).await.expect("get"),
        Some(b"deep".to_vec())
    );
}

pub(crate) async fn put__should_fail_under_a_value(store: impl Get + Put) {
    // Given
    store.put(&key("a"), b"value").await.expect("put");

    // When
    let result = store.put(&key("a/b"), b"nested").await;

    // Then
    assert!(result.is_err());
    assert_eq!(
        store.get(&key("a")).await.expect("get"),
        Some(b"value".to_vec())
    );
}

pub(crate) async fn put__should_fail_onto_a_directory(store: impl Get + Put) {
    // Given
    store.put(&key("a/b"), b"child").await.expect("put");

    // When
    let result = store.put(&key("a"), b"value").await;

    // Then
    assert!(result.is_err());
    assert_eq!(
        store.get(&key("a/b")).await.expect("get"),
        Some(b"child".to_vec())
    );
}

pub(crate) async fn delete__should_remove_the_value(store: impl Get + Put + Delete) {
    // Given
    store.put(&key("a/b"), b"bytes").await.expect("put");

    // When
    store.delete(&key("a/b")).await.expect("delete");

    // Then
    assert_eq!(store.get(&key("a/b")).await.expect("get"), None);
}

pub(crate) async fn delete__should_succeed_for_an_absent_key(store: impl Delete) {
    store.delete(&key("a/b")).await.expect("delete");
}

pub(crate) async fn list__should_return_immediate_children_sorted(store: impl Put + List) {
    // Given
    for path in ["d/z", "d/a.x", "d/m/deep", "d/m/deeper", "other"] {
        store.put(&key(path), b"").await.expect("put");
    }

    // When
    let children = store.list(&key("d")).await.expect("list");

    // Then
    assert_eq!(children, segments(&["a.x", "m", "z"]));
}

pub(crate) async fn list__should_return_empty_for_a_missing_dir(store: impl List) {
    assert_eq!(store.list(&key("nowhere")).await.expect("list"), vec![]);
}

pub(crate) async fn clones__should_share_one_store(store: impl Store) {
    // Given
    let clone = store.clone();

    // When
    clone.put(&key("a"), b"bytes").await.expect("put");

    // Then
    assert_eq!(
        store.get(&key("a")).await.expect("get"),
        Some(b"bytes".to_vec())
    );
}

/// A key from a `/`-joined literal — test-only shorthand.
pub(crate) fn key(path: &str) -> Key {
    Key::parse(path).expect("valid key path")
}

fn segments(names: &[&str]) -> Vec<Segment> {
    names
        .iter()
        .map(|name| Segment::new(name).expect("valid segment"))
        .collect()
}
