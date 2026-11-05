//! The adapters: the ports implemented against the real world. The module
//! boundary is the audit line — nothing outside `adapters/` names an iroh
//! type, reads a real clock, or draws timing entropy. (Crypto randomness is
//! deliberately not behind a port — see `crate::ports::rng`.)

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "ClientState moves onto the port in project 9 S2")
)]
pub(crate) mod fs_store;
pub(crate) mod iroh;
pub(crate) mod system_clock;
pub(crate) mod system_rng;
