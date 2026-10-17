# logigator-sim

The Logigator simulation engine: a fast, tick-exact **logic-circuit simulator** written in Rust.
One engine core ships as:

- a **Node.js native addon** (`@logigator/sim/node`), which runs the simulation on its own thread,
- a **WebAssembly** build for the browser (`@logigator/sim/wasm`),
- a native **CLI** (`sim`), for running, tracing and benchmarking boards,
- and the Rust crate itself (`sim-core`), for embedding.

All of them run the same engine and produce the same per-tick results. The work done each tick is
proportional to what changed, not to the size of the board.

## Install

```bash
npm install @logigator/sim
```

The package is ESM-only and needs Node.js 22 or newer. It bundles prebuilt addons for Linux
(x64 glibc and musl, arm64), macOS (x64, arm64) and Windows (x64), the WASM build, and the CLI.
Standalone CLI binaries are also attached to each
[GitHub release](https://github.com/logigator/logigator-sim/releases).

## Boards

A board is a list of **components** that read and drive **links** (wires). Each component names
its type by numeric id and lists the link ids of its input and output pins; a component with no
inputs (or outputs) can leave that list out. The links are numbered by these ids, from 0 to the
highest one used:

```js
const board = {
  components: [
    { type: 200, outputs: [0] },            // UserInput, drives link 0
    { type: 1, inputs: [0], outputs: [1] }, // NOT, reads link 0 and drives link 1
  ],
};
```

A link driven by several outputs is high if any of them is (wired-OR). Every component takes one
tick to propagate. Components are identified by their position in `components`.

A link is **invalid** while its drivers disagree (some drive it high, some low), or while an SR
flip-flop that was clocked with S and R both high drives it. An invalid link keeps its wired-OR
value, which is what the circuit reads; the flag is only reported to the host (in every snapshot),
so an editor can show it or ignore it. It does not spread to the components reading the link.

Optional fields on a component:

- The parameters of its type, listed under *Parameters* below, such as a clock's `period` or a
  ROM's `data`. A parameter on any other type is an error.
- `negInputs` / `negOutputs`: indices of pins (into `inputs` / `outputs`) that read or drive the
  inverted value, at no extra delay. A NAND is an AND with `negOutputs: [0]`, and a negated clock
  input turns a rising-edge component into a falling-edge one.

The board is validated when it is loaded, and an error names the offending component. Boards can
also be stored in a compact binary format (`.lgb`), loaded with `Simulation.fromBinary`.

### Component types

Multi-bit values (addresses, words, select bits) are read least-significant bit first. Clocked
components act on the rising edge; negating the clock input makes them act on the falling edge.

**Gates.** One output each.

| Gate  | id | Inputs | Behavior                                    |
|-------|----|--------|---------------------------------------------|
| NOT   | 1  | 1      | Inverts its input.                          |
| AND   | 2  | ≥ 2    | High when all inputs are high.              |
| OR    | 3  | ≥ 2    | High when any input is high.                |
| XOR   | 4  | ≥ 2    | High when an odd number of inputs are high. |
| DELAY | 5  | 1      | Passes its input through, one tick later.   |

**Arithmetic and routing.**

| Component  | id | Inputs                             | Outputs                                   | Behavior                                                                     |
|------------|----|------------------------------------|-------------------------------------------|------------------------------------------------------------------------------|
| Half adder | 10 | a, b                               | sum, carry                                | Adds two bits.                                                               |
| Full adder | 11 | a, b, carry-in                     | sum, carry                                | Adds three bits.                                                             |
| Decoder    | 18 | n bits (1–16)                      | 2ⁿ                                        | Drives the output numbered by the input high, all others low.                |
| Encoder    | 19 | 2ⁿ                                 | n bits (1–16), then an optional valid pin | Outputs the index of the highest high input (0 if none). The valid pin, if present, is high when any input is. |
| MUX        | 20 | s select bits (1–16), then 2ˢ data | 1                                         | Outputs the data input chosen by the select bits.                            |
| DEMUX      | 21 | s select bits, then data           | 2ˢ                                        | Routes the data input to the output chosen by the select bits, all others low. |

**Flip-flops.** Outputs Q and Q̄; they act on a rising clock.

| Flip-flop | id | Inputs      | On a rising clock                                                                                       |
|-----------|----|-------------|---------------------------------------------------------------------------------------------------------|
| D         | 13 | D, clock    | Stores D.                                                                                               |
| JK        | 14 | J, clock, K | J sets, K resets, both toggle.                                                                          |
| SR        | 15 | S, clock, R | S sets, otherwise R resets. S and R together set Q and make both outputs invalid until a clock with only S or only R. |

**Clock, memory and I/O.**

| Component            | id  | Inputs                                     | Outputs                               | Behavior |
|----------------------|-----|--------------------------------------------|---------------------------------------|----------|
| CLK                  | 6   | halt                                       | 1                                     | Stays low for `period` ticks, then high for `period` ticks (square wave) or for one tick (pulse), and repeats. A high halt input stops it and holds the output low; it restarts at the beginning of a cycle. |
| ROM                  | 12  | address (1–16 bits)                        | data word (1–64 bits)                 | Outputs the word at the current address. |
| RNG                  | 16  | trigger                                    | ≥ 1                                   | Outputs new random bits on each rising edge of the trigger. Reproducible for the same board. |
| RAM                  | 17  | address (≤ 24 bits), data, write-enable, clock | data word (as wide as the data input) | Outputs the addressed word; on a rising clock with write-enable high, writes the data input there. In the legacy mode the outputs change only on a rising clock, which writes or else reads the addressed word. The host can read and write its memory. |
| UserInput            | 200 | none                                       | ≥ 1                                   | Driven by the host; see [Input](#input). |
| Recorder             | 201 | 1–32 data bits, then a trigger             | none                                  | On a rising trigger, records the data bus as a number. The host reads the records. |
| Addressable register | 204 | address, data (`dataBus` bits), clock      | ≥ 1: the stored bits                  | Outputs its stored bits, in words of `dataBus` (the last may be partial). On a rising clock, writes the data bus into the addressed word. The address has ⌈log₂(⌈outputs / dataBus⌉)⌉ bits; a write past the last output is dropped. The editor shows it as an LED matrix. |

**Parameters.**

| Component            | Parameter     | Value |
|----------------------|---------------|-------|
| CLK                  | `period`      | Required. ≥ 1. |
| CLK                  | `mode`        | `"square"` (default) or `"pulse"`. |
| ROM                  | `data`        | The contents as bytes (a `Uint8Array` or an array of 0–255), words packed LSB-first (word `a` starts at bit `a × outputs`); missing bytes are 0, extra ones an error. |
| RAM                  | `read`        | `"async"` (default) or `"clocked"` for the legacy clocked read. |
| Recorder             | `capacity`    | Required. Records kept (1–2²⁰). |
| Recorder             | `recordTicks` | Also store each record's tick. |
| Addressable register | `dataBus`     | Required. Data bus width, 1 to the output count. |

## Usage

### Node.js

```js
import { Simulation } from "@logigator/sim/node";

const sim = new Simulation(board); // or Simulation.fromBinary(buffer)

sim.run({ ticks: 10 });  // blocking; needs a `ticks` or `ms` bound
sim.link(1);             // true: NOT of a low input

// Run in the background. getStatus() and snapshot() stay responsive while it runs.
const done = sim.runAsync();      // unbounded until stop()
const snap = await sim.snapshot(false, 0);
snap.links;                       // packed link bits: link l is bit l & 7 of byte l >> 3
sim.getStatus();                  // { state, tick, speed, linkCount, componentCount }
sim.stop();
await done;

sim.destroy();
```

`snapshot(delta, threshold)` returns a consistent copy of all links taken between two ticks. With
`delta: true` it returns only the links that changed since the previous snapshot, falling back to a
full copy when more than `threshold` (0–1) of them changed. Either way, `snap.invalid` is a
`Uint32Array` of every invalid link id, ascending.

### Browser (WASM)

```js
import init, { Simulation } from "@logigator/sim/wasm";

const { memory } = await init();
const sim = new Simulation(board);

await sim.runAsync({ ms: 1000 }); // runs in short slices, yielding to the event loop in between
sim.run({ ticks: 100 });          // or blocking, with a bound

const view = sim.snapshot(false, 0); // zero-copy view into WASM memory
const links = new Uint8Array(memory.buffer, view.ptr, view.len);
const invalid = new Uint32Array(memory.buffer, view.invalidPtr, view.invalidLen / 4);
```

The WASM build has the same API as the Node addon, except that everything runs on the calling
thread: `snapshot`, `readState` and `readRecords` return their result directly instead of a
promise. A snapshot view is valid until the next call into the simulation.

The full TypeScript contract shared by both builds is published as `@logigator/sim/contract`.

### Input

`UserInput` components are driven by queuing timed **frames** on them:

```js
sim.submitInput([
  { comp: 0, frames: [{ state: [true], hold: 5 }, { state: [false] }] }, // a 5-tick pulse
]);
```

Each frame sets the input's output pins (`null` leaves a pin unchanged) and stays at least `hold`
ticks before the next frame is applied. Because frames are applied inside the tick loop, their
timing is exact regardless of how the host batches its ticks. A frame with `coalesce: true` merges
into a still-pending coalescing frame, so rapid changes such as a toggled switch reach the circuit
once. `replace: true` discards the frames still queued. `inputProgress(out)` reports how many
non-coalescing frames each input still has pending.

### Component state

RAM contents and recorder captures can be read by the host, and RAM can be written to (for example
to load a program):

```js
sim.stateVersions(compIds, versions); // two Uint32Arrays; a version changes when the state does
await sim.readState(ram, 0, sim.stateLength(ram)); // RAM bytes (sync in WASM)
sim.writeState(ram, 0, bytes);
await sim.readRecords(recorder, 0);               // { count, first, values, ticks? }
```

Poll `stateVersions` and re-read only the components whose version moved.

### CLI

```bash
sim run   <board> [--ticks N] [--ms N] [--dump state.json] [--dump-format json|bin]
sim trace <board> [--ticks N] [--out trace.json]   # the state after every tick
sim bench <board> [--ticks N] [--ms N] [--repeat N] # ticks per second
```

Installed through npm, the command is `npx logigator-sim`. A board file is a JSON board, a `.lgb`
binary, or a JSON fixture as in [`corpus/boards/`](corpus/boards), which wraps a board with a
default tick count and timed input:

```bash
sim run corpus/boards/gates.json --dump state.json
sim bench corpus/boards/clk.json --ms 1000 --repeat 3
```

### Rust

`sim-core` is not on crates.io yet; depend on it from git:

```toml
sim-core = { git = "https://github.com/logigator/logigator-sim" }
```

```rust
use sim_core::{BoardBuilder, CompType, RunConfig, Simulation};

let mut b = BoardBuilder::new(2);
b.component(CompType::UserInput, &[], &[0], &[]);
b.component(CompType::Not, &[0], &[1], &[]);

let mut sim = Simulation::from_descriptor(&b.finish())?;
sim.run(RunConfig { ticks: 10, ..Default::default() })?;
assert!(sim.link(1));
```

## Building from source

You need stable Rust (1.88+), Node.js 22+, [`wasm-pack`](https://rustwasm.github.io/wasm-pack/)
and the `wasm32-unknown-unknown` target for the WASM build, and optionally
[`just`](https://github.com/casey/just).

```bash
just build        # CLI, WASM package and Node addon
just build-cli    # → target/release/sim
just build-wasm   # → crates/sim-wasm/pkg
just setup-node   # once: install the addon's npm dependencies
just build-node   # → crates/sim-node
```

```bash
just test         # fmt, clippy and the Rust test suite
just test-node    # the same checks through the Node addon
just test-wasm    # the same checks through the WASM build
```

Every board in `corpus/boards/` is checked tick by tick against its recorded trace in
`corpus/golden/`, through each of the surfaces. Benchmarks and their results are in
[`corpus/bench/RESULTS.md`](corpus/bench/RESULTS.md); `just --list` shows all recipes.

| Path              | Contents                                                     |
|-------------------|--------------------------------------------------------------|
| `crates/sim-core` | the engine                                                   |
| `crates/sim-cli`  | the `sim` CLI                                                |
| `crates/sim-node` | the Node addon and the `@logigator/sim` package              |
| `crates/sim-wasm` | the WASM build                                               |
| `corpus`          | test boards, golden traces, benchmark boards and their tools |

## License

[GNU AGPL-3.0](LICENSE)
