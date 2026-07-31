//! A fast hasher for the maps a link probes hundreds of thousands of times.
//!
//! `std`'s default is SipHash-1-3, chosen to make hash-collision denial of
//! service impractical. That threat model does not apply here: every key comes
//! from an object file the linker was told to read, and an attacker who can
//! choose them can already choose the output. What SipHash costs instead is
//! real — the hot maps in a link are keyed by an `(object, section)` pair,
//! eight bytes, and are probed once per relocation and once per symbol.
//!
//! This is the multiply-xor-rotate construction `rustc` uses for the same
//! reason. It is not a good hash for adversarial input and makes no claim to
//! be; for dense small integers it is a few instructions and distributes them
//! well enough that the maps stay flat.
//!
//! **Not for names.** Symbol names are attacker-shaped in a different sense —
//! long, sharing prefixes, and hashed far less often — so those maps keep the
//! default.

use std::hash::{BuildHasherDefault, Hasher};

/// A `HashMap` using [`FastHasher`].
pub type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// The odd 64-bit constant from `rustc_hash`, close to `2^64 / φ`.
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Multiply-xor-rotate, one round per word.
#[derive(Default)]
pub struct FastHasher {
    state: u64,
}

impl FastHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        // Rotate before mixing so the high bits, which the multiply spreads
        // into, reach the low bits a map's bucket index is taken from.
        self.state = (self.state.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(value as u64);
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn hash_of<T: Hash>(value: T) -> u64 {
        let mut hasher = FastHasher::default();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// The property a hash must have to be a hash at all.
    #[test]
    fn equal_keys_hash_equally() {
        assert_eq!(hash_of((7u32, 3u32)), hash_of((7u32, 3u32)));
    }

    /// And the property it must have to be a *useful* one: the pairs a link
    /// actually uses are dense small integers, and swapping the two halves of
    /// a pair must not collide, or every section of one object would land in
    /// one bucket.
    #[test]
    fn dense_pairs_do_not_collide() {
        let mut seen = std::collections::HashSet::new();
        for object in 0u32..200 {
            for section in 0u32..20 {
                assert!(
                    seen.insert(hash_of((object, section))),
                    "({object}, {section}) collided with an earlier pair"
                );
            }
        }
    }

    /// A single differing bit must change the hash. Without the rotate it does
    /// not: multiplying by an odd constant leaves the low bits of a one-word
    /// key almost untouched, and a map indexes on exactly those.
    #[test]
    fn one_bit_apart_keys_land_apart() {
        let bucket = |value: u32| hash_of((0u32, value)) & 0xff;
        let buckets: std::collections::HashSet<u64> = (0u32..64).map(bucket).collect();
        assert!(
            buckets.len() > 50,
            "64 consecutive keys used only {} of 256 buckets",
            buckets.len()
        );
    }
}
