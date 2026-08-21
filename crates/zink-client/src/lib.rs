//! Client core: everything "being a zink client" means — keystore,
//! conversation state, send/recv flows, blob push/fetch. Shared by the CLI,
//! the app, and (later, via the WASM build) the PWA. Edges own presentation
//! and argument handling; this crate owns keys, state, and flows.
//! See `docs/design/client-core.md`.

#[cfg(not(target_family = "wasm"))]
mod adapters;
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
#[cfg(not(target_family = "wasm"))]
mod ports;
#[cfg(not(target_family = "wasm"))]
mod reach;
#[cfg(target_family = "wasm")]
mod spike;
#[cfg(not(target_family = "wasm"))]
mod state;
#[cfg(not(target_family = "wasm"))]
mod sync;

#[cfg(not(target_family = "wasm"))]
pub use client::{
    AvatarReceipt, Client, ClientConfig, Contact, ConversationSummary, DeviceEvidence, Disavowal,
    FlushReport, FriendView, HistoryMessage, Inbox, LastMessage, LearnedName, MAX_MESSAGE_REQUESTS,
    OUTBOX_GIVE_UP_MS, PersonEntry, PersonId, Reachable, Received, RecordMatch, RecordUpdate,
    RecvReport, RelayFailure, RelayHealth, RelayResolution, RelaySource, RelayStatus,
    ReplyContacts, ResolvedName, SendReceipt, StagedSend, WhoIsAnswer, WhoIsOutcome, triage,
};
pub use error::Error;
#[cfg(target_family = "wasm")]
pub use spike::spike_register;
