//! Addressable-register kernel (type 204): a bank of outputs written one word at a time on a rising
//! clock. The outputs double as the stored state, so the editor can show them as an LED matrix; a
//! 74259 addressable latch is the same idea with level-triggered, one-bit words.
//!
//! Inputs are `[address(addrBus), data(dataBus), clock]`; the outputs form words of `dataBus`
//! (the last word may be partial). No scratch is needed beyond the shared `edge_prev` bit. On a
//! **rising clock**, the data bus is latched into the addressed word
//! (`outputs[address*dataBus + i] = data[i]`). `dataBus = config.a`, `addrBus = config.b`. init is
//! a no-op (outputs start low).
//!
//! The address bus is `ceil(log2(words))` wide, so when the word count is not a power of two it can
//! name words past the last one. Those are unmapped: a write there lands only on the outputs that
//! exist (a partial last word) and is otherwise dropped, never on another component's pins.

use super::{Kernel, TickCtx};

/// Addressable register (type 204): latch the data bus into the addressed word on each rising clock.
pub(crate) struct AddressableRegister;

impl Kernel for AddressableRegister {
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
                let outs = ctx.output_count(c) as usize;
                for i in 0..data_bus.min(outs.saturating_sub(position)) {
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
    use crate::{BoardBuilder, CompType, ComponentDescriptor, Result, Simulation};

    /// Address bits of a register of `outs` outputs in words of `width`: enough to name every word.
    fn addr_bits(outs: u32, width: u32) -> u32 {
        outs.div_ceil(width).next_power_of_two().trailing_zeros()
    }

    /// Input 0 drives a register (component 1) of `outs` outputs with `ins` inputs and `dataBus: width`;
    /// a NOT (component 2) owns the output pin after the register's.
    fn register(outs: u32, width: u32, ins: u32) -> Result<Simulation> {
        let (out0, not_in) = (ins, ins + outs);
        let mut b = BoardBuilder::new();
        b.component(CompType::UserInput, &[], &(0..ins).collect::<Vec<_>>());
        b.add(ComponentDescriptor {
            data_bus: Some(width),
            ..ComponentDescriptor::new(
                CompType::AddressableRegister,
                &(0..ins).collect::<Vec<_>>(),
                &(out0..not_in).collect::<Vec<_>>(),
            )
        });
        b.component(CompType::Not, &[not_in], &[not_in + 1]);
        Simulation::from_descriptor(&b.finish())
    }

    /// The input count must be exactly address + data + clock for the given width and output count;
    /// the width must be 1..=outputs. The editor's 4×4, 8×8 and 16×16 LED matrices keep their pins.
    #[test]
    fn input_count_follows_width_and_output_count() {
        for (outs, width, ins) in [(16, 4, 7), (64, 8, 12), (256, 8, 14), (16, 1, 6), (6, 4, 6)] {
            assert!(
                register(outs, width, ins).is_ok(),
                "{outs} outputs × {width}"
            );
            assert!(
                register(outs, width, ins - 1).is_err(),
                "{outs} outputs × {width}, short"
            );
            assert!(
                register(outs, width, ins + 1).is_err(),
                "{outs} outputs × {width}, long"
            );
        }
        assert!(register(4, 0, 2).is_err(), "width 0");
        assert!(register(4, 5, 6).is_err(), "width past the output count");
    }

    /// A write changes only the addressed word's outputs: a partial last word keeps the outputs that
    /// exist, a word wholly past the end is dropped, and the NOT after the register is never touched.
    #[test]
    fn writes_stay_in_the_addressed_word() {
        // (output count, data width, word address, expected high outputs after writing all-ones)
        let cases: [(u32, u32, u32, &[u32]); 6] = [
            (12, 4, 2, &[8, 9, 10, 11]),          // last full word
            (12, 4, 3, &[]),                      // 3 words, 2-bit address: word 3 does not exist
            (28, 8, 3, &[24, 25, 26, 27]),        // 3 full words of 8, then a partial word of 4
            (6, 4, 1, &[4, 5]),                   // a partial second word is addressable
            (16, 1, 5, &[5]),                     // one-bit words
            (8, 8, 0, &[0, 1, 2, 3, 4, 5, 6, 7]), // one word, no address bits
        ];
        for (outs, width, addr, lit) in cases {
            let bits = addr_bits(outs, width);
            let mut sim = register(outs, width, bits + width + 1).unwrap();
            let mut pins: Vec<bool> = (0..bits).map(|i| addr >> i & 1 == 1).collect();
            pins.extend((0..width).map(|_| true));
            pins.push(false);
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());
            *pins.last_mut().unwrap() = true; // rising clock
            sim.set_input(0, &pins).unwrap();
            (0..3).for_each(|_| sim.tick());

            let on: Vec<u32> = (0..outs).filter(|&i| sim.output(1, i as usize)).collect();
            assert_eq!(on, lit, "{outs} outputs × {width}, word {addr}");
            assert!(
                sim.output(2, 0),
                "{outs} outputs × {width}, word {addr}: next component untouched"
            );
        }
    }
}
