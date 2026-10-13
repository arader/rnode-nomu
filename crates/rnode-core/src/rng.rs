//! Deterministic pseudo-random numbers for the node.
//!
//! The firmware seeds Arduino's `random()` at boot and uses it for the CSMA contention window
//! (`random(cw_min, cw_max)`) and the sequence nibble of transmitted packets
//! (`random(256) & 0xF0`). The node uses this seeded generator instead, so behaviour is
//! reproducible under test; the platform seeds it from real entropy.

/// SplitMix64 pseudo-random generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// The next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A random byte.
    pub fn byte(&mut self) -> u8 {
        (self.next_u64() >> 56) as u8
    }

    /// A value in `[min, max)`, like Arduino's `random(min, max)`; `min` when `max <= min`.
    pub fn range(&mut self, min: u32, max: u32) -> u32 {
        if max <= min {
            return min;
        }
        let span = u64::from(max - min);
        // Multiply-shift maps 32 random bits onto the span without modulo bias worth caring
        // about for spans this small.
        min + ((u64::from(self.next_u32()) * span) >> 32) as u32
    }
}
