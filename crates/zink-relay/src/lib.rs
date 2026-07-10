//! zink-relay: mailbox service over a custom iroh ALPN.
//!
//! Untrusted infrastructure — sees ciphertext + minimal metadata, never
//! content. Domain logic in `mailbox`, storage port in `store`, iroh edge
//! in `net`.

pub mod blobs;
pub mod clock;
pub mod mailbox;
pub mod net;
pub mod store;
#[cfg(test)]
mod testutil;
