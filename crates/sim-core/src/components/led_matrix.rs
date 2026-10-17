//! LED-matrix kernel (type 204): an edge-clocked display.
//!
//! Inputs are `[address(addrBus), data(dataBus), clock]`; the `ledCount` outputs are the LEDs and
//! double as the stored display state (so no scratch is needed beyond the shared `edge_prev` bit).
//! On a **rising clock**, the `dataBus`-wide data bus is latched into the addressed LED row
//! (`outputs[address*dataBus + i] = data[i]`). `dataBus = config.a`, `addrBus = config.b`.
//! init is a no-op (LEDs start low).
//!
//! The address bus is `ceil(log2(rows))` wide, so when the row count is not a power of two it can
//! name rows past the last LED. Those are unmapped: a write there lands only on the LEDs that
//! exist (a partial last row) and is otherwise dropped, never on another component's pins.

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
    use crate::{BoardBuilder, CompType, Simulation};

    /// A write to an address past the last LED changes only LEDs of this matrix: a partial last
    /// row keeps its existing LEDs, a row wholly past the end is dropped, and the NOT whose output
    /// pin follows the matrix's pins is never touched.
    #[test]
    fn rows_past_the_last_led_are_unmapped() {
        // (LED count, ops[0] → data width, row address, expected lit LEDs after writing all-ones)
        let cases: [(u32, u32, u32, &[u32]); 3] = [
            (12, 4, 2, &[8, 9, 10, 11]),   // last full row
            (12, 4, 3, &[]),               // 3 rows, 2-bit address: row 3 does not exist
            (28, 8, 3, &[24, 25, 26, 27]), // 3 full rows of 8, then a partial row of 4
        ];
        for (leds, width, addr, lit) in cases {
            let data = if width > 4 { 8 } else { 4 };
            let addr_bits = (leds / data).next_power_of_two().trailing_zeros();
            let ins = addr_bits + data + 1; // address, data, clock
            let led0 = ins;
            let not_in = led0 + leds;
            let mut b = BoardBuilder::new(not_in + 2);
            b.component(CompType::UserInput, &[], &(0..ins).collect::<Vec<_>>(), &[]);
            b.component(
                CompType::LedMatrix,
                &(0..ins).collect::<Vec<_>>(),
                &(led0..not_in).collect::<Vec<_>>(),
                &[width],
            );
            b.component(CompType::Not, &[not_in], &[not_in + 1], &[]);
            let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

            let mut pins: Vec<bool> = (0..addr_bits).map(|i| addr >> i & 1 == 1).collect();
            pins.extend((0..data).map(|_| true));
            pins.push(false);
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());
            *pins.last_mut().unwrap() = true; // rising clock
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());

            let on: Vec<u32> = (0..leds).filter(|&i| sim.output(1, i as usize)).collect();
            assert_eq!(on, lit, "{leds} LEDs, row {addr}");
            assert!(
                sim.output(2, 0),
                "{leds} LEDs, row {addr}: the next component is untouched"
            );
        }
    }
}
