//! The adapters: the ports implemented against the real world. The module
//! boundary is the audit line — nothing outside `adapters/` names an iroh
//! type or reads a real clock.

pub(crate) mod iroh;
pub(crate) mod system_clock;
