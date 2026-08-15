//! The real entropy source — implements the draw port over the OS RNG.

use rand_core::{OsRng, RngCore};

use crate::ports::rng::Draw;

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRng;

impl Draw for SystemRng {
    fn draw(&self, bound: u64) -> u64 {
        // Modulo bias is real but harmless at timing-jitter stakes; the
        // port promises "uniform enough for delays", not crypto.
        OsRng.next_u64() % bound
    }
}
