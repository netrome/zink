//! The ports: traits describing what the domain needs from the outside
//! world, plus the plain data they speak. Nothing here touches a network,
//! a clock, or a socket — implementations live in `crate::adapters` (the
//! real world) and in each port's `#[cfg(test)]` submodule (the doubles).

pub(crate) mod clock;
pub(crate) mod rng;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "ClientState moves onto the port in project 9 S2")
)]
pub(crate) mod store;
pub(crate) mod transport;
