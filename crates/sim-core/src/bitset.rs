//! Packed bitset over `Box<[AtomicU64]>` (plan §5.2, D2).
//!
//! State bitsets are **atomic-typed** so the multi-threaded read/compute phases (plan phase 6)
//! can share words soundly, but the single-threaded path accesses them with *relaxed* load/store
//! — which lowers to a plain `mov`, so the atomic type costs nothing on the hot path (§1.3a, I7).
//! Atomic read-modify-write (`fetch_or`/`fetch_and`) is added with the parallel driver; until then
//! every accessor here is the plain load/store form and is sound only single-threaded.
//!
//! Bit `i` lives in word `i >> 6`, at bit `i & 63`.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

const WORD_BITS: u32 = 64;

/// A fixed-length packed bitset. `bits` is the logical length; the backing store is rounded up to
/// whole 64-bit words.
#[repr(C, align(64))]
pub struct BitSet {
    words: Box<[AtomicU64]>,
    bits: u32,
}

impl BitSet {
    /// Allocate a zeroed bitset holding `bits` logical bits.
    pub fn new(bits: u32) -> Self {
        let n_words = bits.div_ceil(WORD_BITS) as usize;
        let mut v = Vec::with_capacity(n_words);
        v.resize_with(n_words, || AtomicU64::new(0));
        BitSet {
            words: v.into_boxed_slice(),
            bits,
        }
    }

    /// Number of logical bits.
    #[inline]
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Read bit `i`. Relaxed load (plain `mov` on the single-threaded path).
    #[inline]
    pub fn get(&self, i: u32) -> bool {
        debug_assert!(i < self.bits, "bit {i} out of range (bits={})", self.bits);
        let w = (i >> 6) as usize;
        let mask = 1u64 << (i & 63);
        (self.words[w].load(Relaxed) & mask) != 0
    }

    /// Write bit `i` to `v`. Single-threaded relaxed load/store (not an atomic RMW — sound only
    /// while no other thread touches the same word; the parallel driver adds an atomic variant).
    #[inline]
    pub fn set(&self, i: u32, v: bool) {
        debug_assert!(i < self.bits, "bit {i} out of range (bits={})", self.bits);
        let w = (i >> 6) as usize;
        let mask = 1u64 << (i & 63);
        let cur = self.words[w].load(Relaxed);
        let next = if v { cur | mask } else { cur & !mask };
        self.words[w].store(next, Relaxed);
    }

    /// Set every bit to 0.
    #[inline]
    pub fn clear(&self) {
        for w in self.words.iter() {
            w.store(0, Relaxed);
        }
    }

    /// Zero-copy borrow of the packed backing words (the layout the public API hands out, §7.2).
    /// Read each word with `.load(Relaxed)`.
    #[inline]
    pub fn words(&self) -> &[AtomicU64] {
        &self.words
    }

    /// Number of backing 64-bit words.
    #[inline]
    pub fn word_count(&self) -> usize {
        self.words.len()
    }
}

impl Clone for BitSet {
    fn clone(&self) -> Self {
        let mut v = Vec::with_capacity(self.words.len());
        for w in self.words.iter() {
            v.push(AtomicU64::new(w.load(Relaxed)));
        }
        BitSet {
            words: v.into_boxed_slice(),
            bits: self.bits,
        }
    }
}

impl core::fmt::Debug for BitSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "BitSet({} bits: ", self.bits)?;
        for i in 0..self.bits {
            write!(f, "{}", self.get(i) as u8)?;
        }
        write!(f, ")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_bits() {
        let bs = BitSet::new(130); // 3 words
        assert_eq!(bs.bits(), 130);
        assert_eq!(bs.word_count(), 3);
        for i in 0..130 {
            assert!(!bs.get(i));
        }
        bs.set(0, true);
        bs.set(63, true);
        bs.set(64, true);
        bs.set(129, true);
        assert!(bs.get(0) && bs.get(63) && bs.get(64) && bs.get(129));
        assert!(!bs.get(1) && !bs.get(62) && !bs.get(65) && !bs.get(128));
        bs.set(63, false);
        assert!(!bs.get(63));
        assert!(bs.get(64)); // neighbouring word untouched
    }

    #[test]
    fn clear_zeros_all() {
        let bs = BitSet::new(70);
        bs.set(5, true);
        bs.set(69, true);
        bs.clear();
        for i in 0..70 {
            assert!(!bs.get(i));
        }
    }

    #[test]
    fn zero_length_is_valid() {
        let bs = BitSet::new(0);
        assert_eq!(bs.bits(), 0);
        assert_eq!(bs.word_count(), 0);
    }

    #[test]
    fn clone_is_independent_snapshot() {
        let a = BitSet::new(64);
        a.set(1, true);
        let b = a.clone();
        a.set(2, true);
        assert!(b.get(1));
        assert!(!b.get(2)); // clone captured the value at clone time
    }
}
