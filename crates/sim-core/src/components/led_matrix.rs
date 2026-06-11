//! LED-matrix kernel (type 204): an edge-clocked display (`src/components/led_matrix.h`).
//!
//! Inputs are `[address(addrBus), data(dataBus), clock]`; the `ledCount` outputs are the LEDs and
//! double as the stored display state (so no scratch is needed beyond the shared `edge_prev` bit).
//! On a **rising clock**, the `dataBus`-wide data bus is latched into the addressed LED row
//! (`outputs[address*dataBus + i] = data[i]`). `dataBus = config.a`, `addrBus = config.b`.
//! init is a no-op (LEDs start low, `led_matrix.h::init`).

use super::{Kernel, TickCtx};

/// LED matrix (type 204): latch a data row into the addressed LEDs on each rising clock.
pub(crate) struct LedMatrix;

impl Kernel for LedMatrix {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let in_count = ins.len();
            let cfg = ctx.config(c);
            let (data_bus, addr_bus) = (cfg.a as usize, cfg.b as usize);
            let clk = ctx.input(ins[in_count - 1]);
            if clk && !ctx.edge_prev(c) {
                let mut position = 0usize;
                for (i, &l) in ins[..addr_bus].iter().enumerate() {
                    position |= (ctx.input(l) as usize) << i;
                }
                position *= data_bus;
                for i in 0..data_bus {
                    let v = ctx.input(ins[addr_bus + i]);
                    ctx.set_output(ctx.output_at(c, (position + i) as u32), v);
                }
            }
            ctx.set_edge_prev(c, clk);
        }
    }
}
