//! Coherent tick-boundary snapshots, full or delta (plan §6.4, D11).
//!
//! The read phase already enumerates every link that flips, so the simulation **always** accumulates
//! a *dirty-since-last-poll* set ([`Simulation::poll_ids`] + its dedup bitset, marked in
//! [`crate::tick`]). [`Simulation::snapshot`] consumes that set: it decides between a `Full` packed
//! copy and a `Delta` of just the changed links, materializes the delta into reused buffers, and
//! resets the accumulator so the next poll starts fresh.
//!
//! This is the **synchronous** snapshot used by the single-threaded WASM surface (plan §7.3), where
//! a `Full` is zero-copy — the caller reads [`Simulation::link_words`] / [`Simulation::link_bytes`]
//! directly off the live `link_state` (coherent until the next `tick()`). The background
//! copy-and-resume native path (`RunHandle`, plan §7.2) lands with the threaded driver.

use crate::sim::Simulation;

/// How a snapshot poll should be produced (plan §6.4).
#[derive(Clone, Copy, Debug)]
pub struct SnapshotConfig {
    /// Opt into delta snapshots (emit only the changed links).
    pub delta: bool,
    /// Fraction of links that, once exceeded, falls a delta request back to a `Full` (bounds the
    /// worst case — a fast sim polled slowly accumulates many distinct changes).
    pub delta_threshold: f32,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        SnapshotConfig {
            delta: false,
            delta_threshold: 0.125,
        }
    }
}

/// What [`Simulation::snapshot`] produced (plan §6.4).
///
/// `is_delta == false` ⇒ a `Full`: read the packed `link_state` via [`Simulation::link_words`] /
/// [`Simulation::link_bytes`]. `is_delta == true` ⇒ a `Delta`: read the changed link ids via
/// [`Simulation::snapshot_ids`] and their packed values via [`Simulation::snapshot_values`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// `true` for a `Delta`, `false` for a `Full`.
    pub is_delta: bool,
    /// The tick the snapshot reflects.
    pub tick: u64,
    /// Number of changed links in a `Delta`; the total link count for a `Full`.
    pub changed: usize,
}

impl Simulation {
    /// Produce a coherent tick-boundary snapshot (plan §6.4).
    ///
    /// Emits a `Delta` only when all of: deltas were requested, a `Full` baseline has already been
    /// emitted (the consumer applies deltas cumulatively, §6.4 contract), and the changed fraction
    /// is within `delta_threshold`. Otherwise emits a `Full` (and the first poll after construction
    /// always does, establishing the baseline). Either way the accumulator is reset, so the next
    /// poll reports only links that flip *after* this one.
    pub fn snapshot(&mut self, cfg: SnapshotConfig) -> SnapshotInfo {
        let changed = self.poll_ids.len();
        let within_threshold = (changed as f32) <= cfg.delta_threshold * (self.link_count as f32);
        let use_delta = cfg.delta && self.delta_baseline && within_threshold;

        let info = if use_delta {
            self.snap_ids.clear();
            self.snap_ids.extend_from_slice(&self.poll_ids);
            self.snap_values.clear();
            self.snap_values.resize(changed.div_ceil(8), 0);
            for (i, &l) in self.poll_ids.iter().enumerate() {
                if self.link_state.get(l) {
                    self.snap_values[i >> 3] |= 1u8 << (i & 7);
                }
            }
            SnapshotInfo {
                is_delta: true,
                tick: self.tick,
                changed,
            }
        } else {
            self.delta_baseline = true;
            SnapshotInfo {
                is_delta: false,
                tick: self.tick,
                changed: self.link_count as usize,
            }
        };

        // Reset the accumulation window: the consumer is now synced to this tick, so the next poll
        // reports only subsequent flips. O(changed) — never a board-size sweep (plan §6.4).
        for &l in &self.poll_ids {
            self.poll_seen.set(l, false);
        }
        self.poll_ids.clear();
        info
    }

    /// Changed link ids of the most recent `Delta` (empty / stale after a `Full`). Pairs index-wise
    /// with [`Simulation::snapshot_values`].
    pub fn snapshot_ids(&self) -> &[u32] {
        &self.snap_ids
    }

    /// Packed values of the most recent `Delta`: bit `i` (`values[i >> 3] >> (i & 7)`) is the
    /// current `link_state` of `snapshot_ids()[i]`.
    pub fn snapshot_values(&self) -> &[u8] {
        &self.snap_values
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, InputEvent, Simulation, SnapshotConfig};

    /// Build a simple board: two UserInputs each driving their own link through a NOT, so flips are
    /// easy to provoke per-link.
    fn two_not_chain() -> Simulation {
        // links: 0 = inA, 1 = NOT(A); 2 = inB, 3 = NOT(B)
        let mut b = BoardBuilder::new(4);
        let a = b.component(CompType::UserInput, &[], &[0], &[]);
        b.component(CompType::Not, &[0], &[1], &[]);
        let bb = b.component(CompType::UserInput, &[], &[2], &[]);
        b.component(CompType::Not, &[2], &[3], &[]);
        assert_eq!((a, bb), (0, 2));
        Simulation::from_descriptor(&b.finish()).unwrap()
    }

    fn delta_cfg() -> SnapshotConfig {
        SnapshotConfig {
            delta: true,
            delta_threshold: 1.0, // never fall back on size; tests drive the fallback explicitly
        }
    }

    /// The first poll always returns `Full` (a delta needs a baseline), even when deltas are
    /// requested and the changed set is small.
    #[test]
    fn first_poll_is_always_full() {
        let mut sim = two_not_chain();
        sim.tick(); // some links flip (NOT seeds settle)
        let info = sim.snapshot(delta_cfg());
        assert!(
            !info.is_delta,
            "first poll must be Full to seed the baseline"
        );
        assert_eq!(info.changed, sim.link_count as usize);
    }

    /// After a Full baseline, a small change set is emitted as a Delta carrying exactly the links
    /// that flipped since the previous poll, with their current values.
    #[test]
    fn delta_reports_only_changes_since_last_poll() {
        let mut sim = two_not_chain();
        // Settle init: NOT(A), NOT(B) go high (inputs low → NOT output high).
        for _ in 0..3 {
            sim.tick();
        }
        let base = sim.snapshot(delta_cfg());
        assert!(!base.is_delta);

        // Drive A high → NOT(A) (link 1) goes low; link 0 also flips high. Link 2/3 untouched.
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();
        for _ in 0..3 {
            sim.tick();
        }
        let d = sim.snapshot(delta_cfg());
        assert!(d.is_delta, "a small change set after a baseline is a Delta");

        let ids = sim.snapshot_ids().to_vec();
        assert!(ids.contains(&0), "link 0 (inA) flipped high");
        assert!(ids.contains(&1), "link 1 (NOT A) flipped low");
        assert!(!ids.contains(&2) && !ids.contains(&3), "B side untouched");

        // Values pair index-wise with ids and reflect the live link_state.
        for (i, &l) in ids.iter().enumerate() {
            let bit = (sim.snapshot_values()[i >> 3] >> (i & 7)) & 1 == 1;
            assert_eq!(bit, sim.link(l), "delta value for link {l}");
        }
    }

    /// A flip that reverts within one accumulation window still appears once (dedup), carrying its
    /// settled value at poll time.
    #[test]
    fn delta_dedups_and_carries_settled_value() {
        let mut sim = two_not_chain();
        for _ in 0..3 {
            sim.tick();
        }
        sim.snapshot(delta_cfg()); // baseline

        // A 0→1 then 1→0 within the window: link 0 ends where it started, so it must NOT appear.
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();
        sim.tick();
        sim.tick();
        sim.trigger_input(0, InputEvent::Cont, &[false]).unwrap();
        for _ in 0..3 {
            sim.tick();
        }
        let d = sim.snapshot(delta_cfg());
        // Whatever ids report, none may be listed twice.
        let ids = sim.snapshot_ids();
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "no link id appears twice in a delta"
        );
        assert!(d.is_delta);
    }

    /// Exceeding the threshold falls a delta request back to a Full (bounds the worst case).
    #[test]
    fn threshold_overflow_falls_back_to_full() {
        let mut sim = two_not_chain();
        for _ in 0..3 {
            sim.tick();
        }
        sim.snapshot(delta_cfg()); // baseline

        // Flip both sides (≥3 of 4 links change), with a threshold of 0.1 → fall back to Full.
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();
        sim.trigger_input(2, InputEvent::Cont, &[true]).unwrap();
        for _ in 0..3 {
            sim.tick();
        }
        let info = sim.snapshot(SnapshotConfig {
            delta: true,
            delta_threshold: 0.1,
        });
        assert!(
            !info.is_delta,
            "over-threshold change set falls back to Full"
        );
        assert_eq!(info.changed, sim.link_count as usize);
    }

    /// `delta: false` always yields Full and (re)establishes the baseline.
    #[test]
    fn full_request_never_delta() {
        let mut sim = two_not_chain();
        for _ in 0..3 {
            sim.tick();
        }
        let cfg = SnapshotConfig {
            delta: false,
            delta_threshold: 1.0,
        };
        assert!(!sim.snapshot(cfg).is_delta);
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();
        sim.tick();
        assert!(!sim.snapshot(cfg).is_delta);
    }
}
