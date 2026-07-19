//! Client core: everything "being a zink client" means — keystore,
//! conversation state, send/recv flows, blob push/fetch. Shared by the CLI,
//! the app, and (later, via the WASM build) the PWA. Edges own presentation
//! and argument handling; this crate owns keys, state, and flows.
//! See `docs/design/client-core.md`.

#[cfg(not(target_family = "wasm"))]
mod blobs;
#[cfg(not(target_family = "wasm"))]
mod client;
mod error;
pub mod hex;
#[cfg(not(target_family = "wasm"))]
pub mod keystore;
#[cfg(not(target_family = "wasm"))]
mod net;
#[cfg(target_family = "wasm")]
mod spike;
#[cfg(not(target_family = "wasm"))]
mod state;
#[cfg(not(target_family = "wasm"))]
mod sync;

#[cfg(not(target_family = "wasm"))]
pub use client::{
    Client, ClientConfig, Contact, ConversationSummary, FlushReport, HistoryMessage, LearnedName,
    Received, ReplyContacts, ResolvedName, SendReceipt, WhoIsAnswer,
};
pub use error::Error;
#[cfg(target_family = "wasm")]
pub use spike::spike_register;
