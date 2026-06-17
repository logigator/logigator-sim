//! Wide-gate input reductions. The change-driven worklist is sparse and does **not** vectorize —
//! the algorithmic wins (incremental `driver_count`, SoA bitsets, monomorphized dispatch) are what
//! move the needle. This module is one localized gate optimization: instead of a per-input branchy
//! iterator (`inputs.iter().all(..)` etc.), **gather** a gate's input bits into 64-bit words and
//! **reduce** each word with a single bit-op —
//!
//! - `AND` of N inputs: every gathered bit set ⟺ each word equals its all-ones mask,
//! - `OR`  of N inputs: any gathered bit set ⟺ some word is non-zero,
//! - `XOR` of N inputs: odd number of set bits ⟺ the summed `count_ones` is odd.
//!
//! The cost is the gather, not the reduce; the reduce is already one instruction per 64 inputs.
//! Vectorizing the gather via `vpgatherdd` only pays for gates with hundreds of inputs, which do
//! not occur in practice.

use crate::bitset::BitSet;

/// Gather up to 64 input bits starting at `inputs[base]` into a word (bit `i` ← `inputs[base + i]`),
/// returning the word and how many bits it holds (`< 64` only for the final partial chunk). Bits
/// above the count are zero, which is what lets `OR`/`XOR` ignore the count and `AND` mask by it.
#[inline]
fn gather_word(inputs: &[u32], ls: &BitSet, base: usize) -> (u64, u32) {
    let n = (inputs.len() - base).min(64);
    let mut w = 0u64;
    for i in 0..n {
        w |= (ls.get(inputs[base + i]) as u64) << i;
    }
    (w, n as u32)
}

/// The negation-applied gathered word: the scattered value gather XOR-ed with the component's
/// contiguous negate bits for the same chunk. `in_base` is the component's first input-CSR index, so
/// `in_base + base` is the global bit position of the chunk's first negate bit. `bits_at` returns the
/// `n` negate bits low-aligned with high bits zero, so the masking and parity reductions below stay
/// correct: an un-negated board passes an all-zero mask and this is the `^0` identity.
#[inline]
fn gather_eff(inputs: &[u32], ls: &BitSet, neg: &BitSet, in_base: u32, base: usize) -> (u64, u32) {
    let (w, n) = gather_word(inputs, ls, base);
    (w ^ neg.bits_at(in_base + base as u32, n), n)
}

/// `AND` of every input link's effective (negation-applied) value (vacuously `true` for no inputs).
/// Short-circuits on the first chunk that isn't all-ones.
#[inline]
pub(crate) fn and_inputs(inputs: &[u32], ls: &BitSet, neg: &BitSet, in_base: u32) -> bool {
    let mut base = 0;
    while base < inputs.len() {
        let (w, n) = gather_eff(inputs, ls, neg, in_base, base);
        // `n == 64` ⇒ full word (avoid the `1 << 64` overflow); else low mask.
        let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
        if w != mask {
            return false;
        }
        base += 64;
    }
    true
}

/// `OR` of every input link's effective value (vacuously `false`). Short-circuits on the first set
/// bit.
#[inline]
pub(crate) fn or_inputs(inputs: &[u32], ls: &BitSet, neg: &BitSet, in_base: u32) -> bool {
    let mut base = 0;
    while base < inputs.len() {
        if gather_eff(inputs, ls, neg, in_base, base).0 != 0 {
            return true;
        }
        base += 64;
    }
    false
}

/// `XOR` of every input link's effective value: `true` iff an odd number of effective inputs are
/// powered (matches `xor.h`'s `sum % 2`). Sums each chunk's popcount; only the low bit matters.
#[inline]
pub(crate) fn xor_inputs(inputs: &[u32], ls: &BitSet, neg: &BitSet, in_base: u32) -> bool {
    let mut ones = 0u64;
    let mut base = 0;
    while base < inputs.len() {
        ones += gather_eff(inputs, ls, neg, in_base, base).0.count_ones() as u64;
        base += 64;
    }
    ones & 1 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle_and(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().all(|&l| ls.get(l))
    }
    fn oracle_or(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().any(|&l| ls.get(l))
    }
    fn oracle_xor(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().filter(|&&l| ls.get(l)).count() & 1 == 1
    }

    fn seeded_bitset(bits: u32, seed: u64) -> BitSet {
        let bs = BitSet::new(bits);
        let mut x = seed;
        for i in 0..bits {
            x = crate::scratch::splitmix64(x);
            if x & 1 == 1 {
                bs.set(i, true);
            }
        }
        bs
    }

    /// A zeroed negate mask, the no-negation case — sized so `bits_at(0 + base, n)` never reads OOB.
    fn no_neg(bits: u32) -> BitSet {
        BitSet::new(bits.max(1))
    }

    #[test]
    fn reductions_match_oracle_across_fanin_and_chunk_boundaries() {
        // Cover the AND-mask trap and every chunk edge: 0, 1, just-under/at/over 64, 128±1, wide.
        let fanins = [
            0usize, 1, 2, 3, 7, 31, 63, 64, 65, 100, 127, 128, 129, 256, 257, 1000,
        ];
        for &n in &fanins {
            let bits = (n as u32).max(1);
            for seed in [1u64, 0xDEAD_BEEF, 0x5555_5555_AAAA_AAAA, u64::MAX] {
                let ls = seeded_bitset(bits, seed);
                let neg = no_neg(bits);
                let inputs: Vec<u32> = (0..n as u32).collect();
                assert_eq!(
                    and_inputs(&inputs, &ls, &neg, 0),
                    oracle_and(&inputs, &ls),
                    "AND n={n} seed={seed}"
                );
                assert_eq!(
                    or_inputs(&inputs, &ls, &neg, 0),
                    oracle_or(&inputs, &ls),
                    "OR n={n} seed={seed}"
                );
                assert_eq!(
                    xor_inputs(&inputs, &ls, &neg, 0),
                    oracle_xor(&inputs, &ls),
                    "XOR n={n} seed={seed}"
                );
            }
        }
    }

    #[test]
    fn all_set_and_all_clear_edges() {
        // All-ones: AND true, OR true, XOR = parity of N. All-zero: AND false (N>0), OR false, XOR false.
        for &n in &[1usize, 63, 64, 65, 128] {
            let inputs: Vec<u32> = (0..n as u32).collect();
            let neg = no_neg(n as u32);

            let all = BitSet::new(n as u32);
            for i in 0..n as u32 {
                all.set(i, true);
            }
            assert!(and_inputs(&inputs, &all, &neg, 0), "all-set AND n={n}");
            assert!(or_inputs(&inputs, &all, &neg, 0), "all-set OR n={n}");
            assert_eq!(
                xor_inputs(&inputs, &all, &neg, 0),
                n % 2 == 1,
                "all-set XOR n={n}"
            );

            let none = BitSet::new(n as u32);
            assert!(!and_inputs(&inputs, &none, &neg, 0), "all-clear AND n={n}");
            assert!(!or_inputs(&inputs, &none, &neg, 0), "all-clear OR n={n}");
            assert!(!xor_inputs(&inputs, &none, &neg, 0), "all-clear XOR n={n}");
        }
    }

    #[test]
    fn empty_inputs_are_vacuous() {
        let ls = BitSet::new(1);
        let neg = no_neg(1);
        assert!(
            and_inputs(&[], &ls, &neg, 0),
            "AND of nothing is vacuously true"
        );
        assert!(!or_inputs(&[], &ls, &neg, 0), "OR of nothing is false");
        assert!(!xor_inputs(&[], &ls, &neg, 0), "XOR of nothing is false");
    }

    #[test]
    fn repeated_and_scattered_links() {
        // Inputs may repeat a link and need not be contiguous/sorted — the gather indexes each.
        let ls = BitSet::new(10);
        let neg = no_neg(10);
        ls.set(3, true);
        ls.set(7, true);
        let inputs = [3u32, 3, 7, 3]; // all powered → AND true, XOR parity of 4 = even = false
        assert!(and_inputs(&inputs, &ls, &neg, 0));
        assert!(or_inputs(&inputs, &ls, &neg, 0));
        assert!(!xor_inputs(&inputs, &ls, &neg, 0));
        let mixed = [3u32, 0, 7]; // link 0 low → AND false, OR true, XOR parity of 2 = false
        assert!(!and_inputs(&mixed, &ls, &neg, 0));
        assert!(or_inputs(&mixed, &ls, &neg, 0));
        assert!(!xor_inputs(&mixed, &ls, &neg, 0));
    }

    /// The negate mask is the word-boundary-crossing code (`bits_at`), so stress it where a
    /// component's contiguous negate field straddles a 64-bit word: `in_base` values near 64/128 and
    /// input counts that cross, compared against a per-pin scalar oracle (`value XOR negate`).
    #[test]
    fn negation_straddles_word_boundary() {
        let neg_bits = 256u32; // covers max(in_base) + max(n)
        for &in_base in &[0u32, 60, 62, 63, 64, 120, 126, 127, 130] {
            for &n in &[1usize, 2, 4, 8, 10, 33, 63, 64, 65] {
                for seed in [1u64, 0xA5A5_1234_5678_9ABC, u64::MAX, 0x0F0F_F0F0_1357_2468] {
                    let ls = seeded_bitset(n as u32, seed);
                    let neg = seeded_bitset(neg_bits, seed ^ 0x9999_9999_9999_9999);
                    let inputs: Vec<u32> = (0..n as u32).collect();
                    let eff = |i: usize| ls.get(inputs[i]) ^ neg.get(in_base + i as u32);
                    let oand = (0..n).all(eff);
                    let oor = (0..n).any(eff);
                    let oxor = (0..n).filter(|&i| eff(i)).count() & 1 == 1;
                    assert_eq!(
                        and_inputs(&inputs, &ls, &neg, in_base),
                        oand,
                        "AND in_base={in_base} n={n} seed={seed}"
                    );
                    assert_eq!(
                        or_inputs(&inputs, &ls, &neg, in_base),
                        oor,
                        "OR in_base={in_base} n={n} seed={seed}"
                    );
                    assert_eq!(
                        xor_inputs(&inputs, &ls, &neg, in_base),
                        oxor,
                        "XOR in_base={in_base} n={n} seed={seed}"
                    );
                }
            }
        }
    }
}
