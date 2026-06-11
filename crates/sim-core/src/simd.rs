//! Wide-gate input reductions (plan §9.2). Honest scope (§9.1): the change-driven worklist is sparse
//! and does **not** vectorize — the algorithmic wins (incremental `driver_count`, SoA bitsets,
//! monomorphized dispatch) are what move the needle. This module is the one *localized* gate
//! optimization: instead of a per-input branchy iterator (`inputs.iter().all(..)` etc.), **gather**
//! a gate's input bits into 64-bit words and **reduce** each word with a single bit-op —
//!
//! - `AND` of N inputs: every gathered bit set ⟺ each word equals its all-ones mask,
//! - `OR`  of N inputs: any gathered bit set ⟺ some word is non-zero,
//! - `XOR` of N inputs: odd number of set bits ⟺ the summed `count_ones` is odd.
//!
//! The cost is the gather, not the reduce; the reduce is already one instruction per 64 inputs, so a
//! portable-SIMD fold of the gathered words (the `wide` crate) would shave only the cheap part and
//! is **not** used here — vectorizing the *gather* (a hardware `vpgatherdd`) is the only place SIMD
//! could pay, and it is gated on a benchmark (§9.3, a separate kernel). These scalar reductions are
//! therefore both the production path and the correctness oracle the SIMD path (if added) is diffed
//! against.

use crate::bitset::BitSet;

/// Input count at/above which the wide-fan-in `vpgatherdd` gather is tried (only if it materially
/// beats scalar — §9.3; see `gather_word_avx2`). Below it, and on non-AVX2 / non-x86 targets, the
/// scalar gather is used. Such gates are vanishingly rare in real circuits, so this is a niche path.
#[cfg(target_arch = "x86_64")]
const WIDE_FANIN: usize = 256;

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

/// AVX2 `vpgatherdd` gather: the SIMD analog of [`gather_word`] for the wide-fan-in path (§9.2/§9.3).
/// Processes 8 inputs per iteration — gather their containing 32-bit `link_state` words, shift each
/// lane's target bit to the sign position, and `movemask` the 8 sign bits into 8 packed result bits.
///
/// # Safety
/// Caller must ensure AVX2 is available. `ls` must be the **frozen** `link_state` (compute never
/// writes it, invariant I1), so reading its `AtomicU64` words through a `*const u32` is race-free —
/// there is no concurrent writer, even on the parallel compute path.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gather_word_avx2(inputs: &[u32], ls: &BitSet, base: usize) -> (u64, u32) {
    use core::arch::x86_64::*;
    let words = ls.words().as_ptr() as *const i32; // frozen link_state words as u32 lanes
    let n = (inputs.len() - base).min(64);
    let mut w = 0u64;
    let mut i = 0usize;
    let bit31 = _mm256_set1_epi32(31);
    while i + 8 <= n {
        // 8 link ids → their u32-word indices (id >> 5) and in-word bit positions (id & 31).
        // SAFETY: `base + i + 8 <= inputs.len()` (loop guard) so the 256-bit load is in bounds;
        // every word index is `id >> 5 < ls.words().len() * 2`, in bounds for the gather; AVX2 is
        // guaranteed by the caller. `link_state` is frozen during compute (I1) → no concurrent write.
        let mask = unsafe {
            let idx = _mm256_loadu_si256(inputs.as_ptr().add(base + i) as *const __m256i);
            let word_idx = _mm256_srli_epi32(idx, 5);
            let gathered = _mm256_i32gather_epi32::<4>(words, word_idx);
            let bitpos = _mm256_and_si256(idx, bit31);
            // Move each lane's target bit to bit 31, then movemask the 8 sign bits → 8 packed bits.
            let shifted = _mm256_sllv_epi32(gathered, _mm256_sub_epi32(bit31, bitpos));
            _mm256_movemask_ps(_mm256_castsi256_ps(shifted)) as u32
        };
        w |= (mask as u64) << i;
        i += 8;
    }
    // Tail (< 8 inputs): scalar, reading the same frozen words.
    while i < n {
        let l = inputs[base + i];
        // SAFETY: `l >> 5` indexes a valid u32 of the frozen `link_state` words (see above).
        let word = unsafe { *words.add((l >> 5) as usize) as u32 };
        w |= (((word >> (l & 31)) & 1) as u64) << i;
        i += 1;
    }
    (w, n as u32)
}

/// Dispatch `$reduce` (built from a `$gather`-named gather) for `$inputs`/`$ls`: on x86-64 with AVX2
/// and a wide gate, the hardware `vpgatherdd` gather (measured ~3× the scalar gather on a 1024-input
/// gate — §9.3 "materially beats"); otherwise the scalar gather. The reduce closure is monomorphized
/// per gather (no indirect call on the common small-gate path); the AVX2 gather is diffed against the
/// scalar gather bit-for-bit in tests.
macro_rules! reduce_dispatch {
    ($inputs:expr, $ls:expr, |$gather:ident| $body:block) => {{
        let inputs = $inputs;
        let ls = $ls;
        #[cfg(target_arch = "x86_64")]
        if inputs.len() >= WIDE_FANIN && is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 detected just above; `gather_word_avx2`'s frozen-`link_state` contract
            // holds (gates run in the compute phase, which never writes `link_state` — I1).
            let $gather = |base: usize| unsafe { gather_word_avx2(inputs, ls, base) };
            return $body;
        }
        let $gather = |base: usize| gather_word(inputs, ls, base);
        $body
    }};
}

/// `AND` of every input link's powered value (vacuously `true` for no inputs). Short-circuits on the
/// first chunk that isn't all-ones.
#[inline]
pub(crate) fn and_inputs(inputs: &[u32], ls: &BitSet) -> bool {
    reduce_dispatch!(inputs, ls, |gather| {
        let mut base = 0;
        while base < inputs.len() {
            let (w, n) = gather(base);
            // `n == 64` ⇒ full word (avoid the `1 << 64` overflow — the AND-mask trap); else low mask.
            let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            if w != mask {
                return false;
            }
            base += 64;
        }
        true
    })
}

/// `OR` of every input link's powered value (vacuously `false`). Short-circuits on the first set bit.
#[inline]
pub(crate) fn or_inputs(inputs: &[u32], ls: &BitSet) -> bool {
    reduce_dispatch!(inputs, ls, |gather| {
        let mut base = 0;
        while base < inputs.len() {
            if gather(base).0 != 0 {
                return true;
            }
            base += 64;
        }
        false
    })
}

/// `XOR` of every input link's powered value: `true` iff an odd number of inputs are powered
/// (matches `xor.h`'s `sum % 2`). Sums each chunk's popcount; only the low bit matters.
#[inline]
pub(crate) fn xor_inputs(inputs: &[u32], ls: &BitSet) -> bool {
    reduce_dispatch!(inputs, ls, |gather| {
        let mut ones = 0u64;
        let mut base = 0;
        while base < inputs.len() {
            ones += gather(base).0.count_ones() as u64;
            base += 64;
        }
        ones & 1 == 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive per-input oracles — the literal semantics the gather-reduce must reproduce.
    fn oracle_and(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().all(|&l| ls.get(l))
    }
    fn oracle_or(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().any(|&l| ls.get(l))
    }
    fn oracle_xor(inputs: &[u32], ls: &BitSet) -> bool {
        inputs.iter().filter(|&&l| ls.get(l)).count() & 1 == 1
    }

    /// A `splitmix64`-driven bit pattern over `bits` links (deterministic, dependency-free).
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

    #[test]
    fn reductions_match_oracle_across_fanin_and_chunk_boundaries() {
        // Cover the AND-mask trap and every chunk edge: 0, 1, just-under/at/over 64, 128±1, wide.
        let fanins = [
            0usize, 1, 2, 3, 7, 31, 63, 64, 65, 100, 127, 128, 129, 256, 257, 1000,
        ];
        for &n in &fanins {
            // Use distinct links so each input bit is independent; size the bitset to fit them.
            let bits = (n as u32).max(1);
            for seed in [1u64, 0xDEAD_BEEF, 0x5555_5555_AAAA_AAAA, u64::MAX] {
                let ls = seeded_bitset(bits, seed);
                let inputs: Vec<u32> = (0..n as u32).collect();
                assert_eq!(
                    and_inputs(&inputs, &ls),
                    oracle_and(&inputs, &ls),
                    "AND n={n} seed={seed}"
                );
                assert_eq!(
                    or_inputs(&inputs, &ls),
                    oracle_or(&inputs, &ls),
                    "OR n={n} seed={seed}"
                );
                assert_eq!(
                    xor_inputs(&inputs, &ls),
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

            let all = BitSet::new(n as u32);
            for i in 0..n as u32 {
                all.set(i, true);
            }
            assert!(and_inputs(&inputs, &all), "all-set AND n={n}");
            assert!(or_inputs(&inputs, &all), "all-set OR n={n}");
            assert_eq!(xor_inputs(&inputs, &all), n % 2 == 1, "all-set XOR n={n}");

            let none = BitSet::new(n as u32);
            assert!(!and_inputs(&inputs, &none), "all-clear AND n={n}");
            assert!(!or_inputs(&inputs, &none), "all-clear OR n={n}");
            assert!(!xor_inputs(&inputs, &none), "all-clear XOR n={n}");
        }
    }

    #[test]
    fn empty_inputs_are_vacuous() {
        let ls = BitSet::new(1);
        assert!(and_inputs(&[], &ls), "AND of nothing is vacuously true");
        assert!(!or_inputs(&[], &ls), "OR of nothing is false");
        assert!(!xor_inputs(&[], &ls), "XOR of nothing is false");
    }

    /// The AVX2 `vpgatherdd` gather must reproduce the scalar gather bit-for-bit, at every base
    /// offset and chunk size (incl. the <8 scalar tail). Skipped if the CPU lacks AVX2.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx2_gather_matches_scalar() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("avx2 not available — skipping");
            return;
        }
        for &total in &[8usize, 9, 15, 16, 64, 100, 256, 257, 1000] {
            for seed in [1u64, 0xABCD_1234_5678, u64::MAX] {
                let bits = total as u32;
                let ls = seeded_bitset(bits, seed);
                let inputs: Vec<u32> = (0..total as u32).collect();
                let mut base = 0;
                while base < total {
                    let scalar = gather_word(&inputs, &ls, base);
                    let avx2 = unsafe { gather_word_avx2(&inputs, &ls, base) };
                    assert_eq!(
                        scalar, avx2,
                        "gather mismatch total={total} base={base} seed={seed}"
                    );
                    base += 64;
                }
            }
        }
    }

    /// Throughput comparison (ignored — run with `--ignored --nocapture`): scalar vs AVX2 gather over
    /// a wide gate, to decide per §9.3 whether the hardware gather *materially* beats portable code.
    #[test]
    #[ignore = "benchmark; run with --ignored --nocapture"]
    #[cfg(target_arch = "x86_64")]
    fn bench_wide_gather() {
        use std::time::Instant;
        if !is_x86_feature_detected!("avx2") {
            eprintln!("avx2 not available");
            return;
        }
        let n = 1024usize;
        let ls = seeded_bitset(n as u32, 0x1234_5678);
        let inputs: Vec<u32> = (0..n as u32).collect();
        let reps = 2_000_000u64;

        let mut acc = 0u64;
        let t = Instant::now();
        for _ in 0..reps {
            let mut base = 0;
            while base < n {
                acc = acc.wrapping_add(gather_word(&inputs, &ls, base).0);
                base += 64;
            }
        }
        let scalar_ns = t.elapsed().as_nanos() as f64 / reps as f64;

        let t = Instant::now();
        for _ in 0..reps {
            let mut base = 0;
            while base < n {
                acc = acc.wrapping_add(unsafe { gather_word_avx2(&inputs, &ls, base).0 });
                base += 64;
            }
        }
        let avx2_ns = t.elapsed().as_nanos() as f64 / reps as f64;

        eprintln!(
            "wide gather n={n}: scalar {scalar_ns:.1} ns/gate, avx2 {avx2_ns:.1} ns/gate, \
             speedup {:.2}x (acc={acc})",
            scalar_ns / avx2_ns
        );
    }

    #[test]
    fn repeated_and_scattered_links() {
        // Inputs may repeat a link and need not be contiguous/sorted — the gather indexes each.
        let ls = BitSet::new(10);
        ls.set(3, true);
        ls.set(7, true);
        let inputs = [3u32, 3, 7, 3]; // all powered → AND true, XOR parity of 4 = even = false
        assert!(and_inputs(&inputs, &ls));
        assert!(or_inputs(&inputs, &ls));
        assert!(!xor_inputs(&inputs, &ls));
        let mixed = [3u32, 0, 7]; // link 0 low → AND false, OR true, XOR parity of 2 = false
        assert!(!and_inputs(&mixed, &ls));
        assert!(or_inputs(&mixed, &ls));
        assert!(!xor_inputs(&mixed, &ls));
    }
}
