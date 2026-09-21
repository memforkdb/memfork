//! Shared helpers for the engine's tests.
#![allow(dead_code)]

use memfork_core::Value;

/// A deterministic pseudo-random source.
///
/// These tests must produce the same history on every platform, so
/// they may not use `rand`, the system clock or anything else that varies.
/// This is a plain xorshift64* with a fixed seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// A float in `[-1, 1)` with only 16 bits of mantissa, so every value is
    /// exactly representable and comparisons are not at the mercy of rounding.
    pub fn unit_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 48) as i32 - 32768;
        bits as f32 / 32768.0
    }

    pub fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.unit_f32()).collect()
    }
}

/// A value with no embedding, default importance and no metadata.
pub fn val(s: &str) -> Value {
    Value::new(s.to_owned())
}

/// The bytes of a key on a branch, as a string, for readable assertions.
pub fn text(e: &memfork_core::Entry) -> String {
    String::from_utf8_lossy(&e.value).into_owned()
}
