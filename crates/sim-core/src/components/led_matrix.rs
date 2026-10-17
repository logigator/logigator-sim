//! LED-matrix kernel (type 204): an edge-clocked addressable register whose outputs are the LEDs.
//! The editor draws it as a matrix, but the engine sees only a bank of outputs written a row at a
//! time.
//!
//! Inputs are `[address(addrBus), data(dataBus), clock]`; the `ledCount` outputs are the LEDs and
//! double as the stored state (so no scratch is needed beyond the shared `edge_prev` bit). The
//! outputs form rows of `dataBus` (the last row may be partial). On a **rising clock**, the data
//! bus is latched into the addressed row (`outputs[address*dataBus + i] = data[i]`).
//! `dataBus = config.a` (`ops[0]`), `addrBus = config.b`. init is a no-op (LEDs start low).
//!
//! The address bus is `ceil(log2(rows))` wide, so when the row count is not a power of two it can
//! name rows past the last one. Those are unmapped: a write there lands only on the LEDs that exist
//! (a partial last row) and is otherwise dropped, never on another component's pins.

use super::{Kernel, TickCtx};

/// LED matrix (type 204): latch a data row into the addressed LEDs on each rising clock.
pub(crate) struct LedMatrix;

impl Kernel for LedMatrix {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let in_count = ctx.inputs(c).len();
            let cfg = ctx.config(c);
            let (data_bus, addr_bus) = (cfg.a as usize, cfg.b as usize);
            let clk = ctx.input_at(c, (in_count - 1) as u32);
            if clk && !ctx.edge_prev(c) {
                let mut position = 0usize;
                for i in 0..addr_bus {
                    position |= (ctx.input_at(c, i as u32) as usize) << i;
                }
                position *= data_bus;
                let leds = ctx.output_count(c) as usize;
                for i in 0..data_bus.min(leds.saturating_sub(position)) {
                    let v = ctx.input_at(c, (addr_bus + i) as u32);
                    ctx.set_output(ctx.output_at(c, (position + i) as u32), v);
                }
            }
            ctx.set_edge_prev(c, clk);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, Result, Simulation};

    /// Address bits of a matrix of `leds` outputs in rows of `width`: enough to name every row.
    fn addr_bits(leds: u32, width: u32) -> u32 {
        leds.div_ceil(width).next_power_of_two().trailing_zeros()
    }

    /// Input 0 drives a matrix (component 1) of `leds` outputs with `ins` inputs and `ops: [width]`;
    /// a NOT (component 2) owns the output pin after the matrix's.
    fn matrix(leds: u32, width: u32, ins: u32) -> Result<Simulation> {
        let (led0, not_in) = (ins, ins + leds);
        let mut b = BoardBuilder::new(not_in + 2);
        b.component(CompType::UserInput, &[], &(0..ins).collect::<Vec<_>>(), &[]);
        b.component(
            CompType::LedMatrix,
            &(0..ins).collect::<Vec<_>>(),
            &(led0..not_in).collect::<Vec<_>>(),
            &[width],
        );
        b.component(CompType::Not, &[not_in], &[not_in + 1], &[]);
        Simulation::from_descriptor(&b.finish())
    }

    /// The input count must be exactly address + data + clock for the given width and LED count;
    /// the width must be 1..=LEDs. The editor's 4×4, 8×8 and 16×16 matrices keep their pins.
    #[test]
    fn input_count_follows_width_and_led_count() {
        for (leds, width, ins) in [(16, 4, 7), (64, 8, 12), (256, 8, 14), (16, 1, 6), (6, 4, 6)] {
            assert!(matrix(leds, width, ins).is_ok(), "{leds} LEDs × {width}");
            assert!(
                matrix(leds, width, ins - 1).is_err(),
                "{leds} LEDs × {width}, short"
            );
            assert!(
                matrix(leds, width, ins + 1).is_err(),
                "{leds} LEDs × {width}, long"
            );
        }
        assert!(matrix(4, 0, 2).is_err(), "width 0");
        assert!(matrix(4, 5, 6).is_err(), "width past the LED count");
    }

    /// A write changes only the addressed row's LEDs: a partial last row keeps the LEDs that exist,
    /// a row wholly past the end is dropped, and the NOT after the matrix is never touched.
    #[test]
    fn writes_stay_in_the_addressed_row() {
        // (LED count, data width, row address, expected lit LEDs after writing all-ones)
        let cases: [(u32, u32, u32, &[u32]); 6] = [
            (12, 4, 2, &[8, 9, 10, 11]),          // last full row
            (12, 4, 3, &[]),                      // 3 rows, 2-bit address: row 3 does not exist
            (28, 8, 3, &[24, 25, 26, 27]),        // 3 full rows of 8, then a partial row of 4
            (6, 4, 1, &[4, 5]),                   // a partial second row is addressable
            (16, 1, 5, &[5]),                     // one LED per row
            (8, 8, 0, &[0, 1, 2, 3, 4, 5, 6, 7]), // one row, no address bits
        ];
        for (leds, width, addr, lit) in cases {
            let bits = addr_bits(leds, width);
            let mut sim = matrix(leds, width, bits + width + 1).unwrap();
            let mut pins: Vec<bool> = (0..bits).map(|i| addr >> i & 1 == 1).collect();
            pins.extend((0..width).map(|_| true));
            pins.push(false);
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());
            *pins.last_mut().unwrap() = true; // rising clock
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());

            let on: Vec<u32> = (0..leds).filter(|&i| sim.output(1, i as usize)).collect();
            assert_eq!(on, lit, "{leds} LEDs × {width}, row {addr}");
            assert!(
                sim.output(2, 0),
                "{leds} LEDs × {width}, row {addr}: next component untouched"
            );
        }
    }
}
