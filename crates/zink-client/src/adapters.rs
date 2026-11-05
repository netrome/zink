//! The adapters: the ports implemented against the real world. The module
//! boundary is the audit line — nothing outside `adapters/` names an iroh
//! type, reads a real clock, draws timing entropy, or touches the
//! filesystem. Two deliberate exceptions: crypto randomness (see
//! `crate::ports::rng`) and the keystore (private keys — ADR 0004's
//! storage addendum).

pub(crate) mod fs_store;
pub(crate) mod iroh;
pub(crate) mod system_clock;
pub(crate) mod system_rng;
