# logigator-sim

A fast, change-driven **logic-circuit simulator** written in Rust, with one engine core compiled to
three surfaces: a **WebAssembly** module (browser), a **Node.js native addon**, and a native **CLI**.

---

## How it works

A circuit is modelled as **links** carrying a boolean `powered` value, driven by component **outputs**
under wired-OR (bus) semantics, and advanced by a **double-buffered, change-driven tick loop**:
per-tick work is proportional to the number of links that *changed*, not to board size.

The engine is built for cache efficiency and predictability:

- **Struct-of-arrays + dense `u32` ids + CSR adjacency** — no pointer chasing, no per-component
  virtual dispatch.
- **Packed `u64` bitsets** for link/output state; reads hand out the same packed words the engine
  uses internally (zero unpacking).
- **Incremental driver counts** replace the wired-OR gather: a link is powered iff its
  count of powered drivers is non-zero — a branch-free cross-zero test, provably equivalent to
  `any_of(drivers)`.
- **Macro-generated component dispatch** (`enum CompType` + `match` over per-type batch kernels) so
  each kernel monomorphizes and inlines.
- **Coherent tick-boundary snapshots** (full or delta) so state can be read *while a simulation runs*
  without tearing.

Two deliberate, documented behavior changes vs. the original engine: the **RNG** component is now
per-component seeded (reproducible and order-independent), and the **SR flip-flop** is rising-edge
triggered (consistent with the D/JK flip-flops). Everything else is verified tick-exact.

---

## Workspace layout

```
logigator-sim/
├── crates/
│   ├── sim-core/   # the engine (lib): SoA board, tick loop, components, .lgb codec, snapshots
│   ├── sim-cli/    # native CLI (`sim run|trace|verify|bench`)
│   ├── sim-wasm/   # WebAssembly surface (wasm-bindgen / wasm-pack), zero-copy state views
│   └── sim-node/   # Node.js native addon (napi-rs), async background run, coherent snapshots
├── corpus/         # golden board fixtures + per-tick traces (tick-exact correctness)
├── justfile        # one-command build/test recipes
└── implementation_plan.md   # full design + rationale
```

---

## Building

### Prerequisites

- **Rust** (stable; MSRV 1.85, edition 2024) — `rustup target add wasm32-unknown-unknown` for WASM.
- **Node.js ≥ 18** — for the Node addon and to run the WASM/Node test suites.
- [`just`](https://github.com/casey/just) — optional, runs the recipes below (otherwise read them as
  the underlying commands).
- [`wasm-pack`](https://rustwasm.github.io/wasm-pack/) — for the WASM build.

### One-command builds

```bash
just build        # CLI + WASM package + Node addon
just build-cli    # → target/release/sim
just build-wasm   # → crates/sim-wasm/pkg  (single-threaded, SIMD128, web target)
just setup-node   # one-time: install the Node addon's npm deps (@napi-rs/cli)
just build-node   # → crates/sim-node/{index.js, index.d.ts, *.node}
```

Plain `cargo build` builds only the core + CLI (the `cdylib` crates are skipped by default).

---

## Usage

### CLI

```bash
sim run    <board> [--ticks N] [--ms N] [--format json|bin] [--dump <out>] [--dump-format json|bin]
sim trace  <board> --ticks N [--out <file>]   # per-tick state dump in the golden format
sim verify <fixture>                           # check a fixture's final state (exit 1 on mismatch)
sim bench  <board> [--ticks N] [--repeat N]    # tick throughput
```

Boards are JSON `BoardDescriptor`s or the compact little-endian `.lgb` binary format (auto-detected
by extension). Example:

```bash
sim run corpus/boards/gates.json --ticks 100 --dump state.json
sim bench corpus/boards/clk.json --ticks 200000 --repeat 3
```

### Node.js

```js
const { Simulation } = require("@logigator/sim-node");

const sim = new Simulation({
  links: 2,
  components: [
    { type: 200, inputs: [], outputs: [0] }, // UserInput driving link 0
    { type: 1,   inputs: [0], outputs: [1] }, // NOT
  ],
});

// Blocking, bounded run:
sim.run({ ticks: 1000 });
console.log(sim.link(1)); // true (NOT of a low input)

// Background, interruptible run — getStatus()/snapshot() stay responsive while it runs:
const done = sim.runAsync({});           // unbounded
const snap = await sim.snapshot(false, 0.5);   // coherent tick-boundary copy
console.log(sim.getStatus());            // { state, tick, speed, ... } — never blocks
sim.stop();
await done;

sim.destroy();
```

State retrieval during a run uses **copy-and-resume snapshots**: the worker produces a coherent copy
at a tick boundary and resumes immediately, so the consumer parses the copy off the hot path. Pass
`delta: true` to receive only the links that changed since the last poll (with a full-copy fallback
above a churn threshold).

### Browser (WASM)

```js
import init, { Simulation } from "./pkg/sim_wasm.js";
await init();

const sim = new Simulation({ links: 2, components: [/* … */] });
await sim.runAsync({});               // cooperative; yields to the event loop between batches
const view = sim.snapshot(false, 0);  // ptr/len into linear memory — zero-copy
const bits = new Uint8Array(memory.buffer, view.ptr, view.len);
```

WASM is single-threaded (JS drives the ticks), so reads are inherently coherent and a full snapshot
points directly into the live link state — nothing is copied across the wasm↔JS boundary.

### Rust (embedding the core)

```rust
use sim_core::{BoardBuilder, CompType, RunConfig, Simulation};

let mut b = BoardBuilder::new(2);
b.component(CompType::UserInput, &[], &[0], &[]);
b.component(CompType::Not, &[0], &[1], &[]);

let mut sim = Simulation::from_descriptor(&b.finish())?;
sim.run(RunConfig { ticks: 1000, ..Default::default() })?;
assert!(sim.link(1));
```

---

## Component types

Numeric `type` ids are stable wire identifiers (shared across all surfaces):

| id  | type | id  | type | id  | type |
|-----|------|-----|------|-----|------|
| 1   | NOT          | 11  | Full adder    | 18  | Decoder    |
| 2   | AND          | 12  | ROM           | 19  | Encoder    |
| 3   | OR           | 13  | D flip-flop   | 20  | MUX        |
| 4   | XOR          | 14  | JK flip-flop  | 21  | DEMUX      |
| 5   | DELAY        | 15  | SR flip-flop  | 200 | UserInput  |
| 6   | CLK          | 16  | RNG           | 204 | LED matrix |
| 10  | Half adder   | 17  | RAM           |     |            |

(Ids `200..=299` other than `204` also map to `UserInput`, matching the original engine.)

---

## Testing

```bash
just test        # fmt check + clippy + the full host test suite (incl. the tick-exact golden corpus)
just test-wasm   # drive the golden corpus through the WASM binding under Node
just test-node   # drive the golden corpus through the Node addon + async-surface/liveness tests
```

Correctness is verified **tick-exact** against per-tick golden traces captured from the original C++
engine (the `corpus/` directory), and the same corpus is replayed through every binding to prove the
surfaces agree.

---

## Roadmap

Implemented:

1. Core engine skeleton (SoA board, single-threaded tick, gate + UserInput kernels).
2. Full component set (adders, ROM, flip-flops, RAM, LED matrix, decoder/encoder/mux/demux, CLK, RNG).
3. CLI (`run`/`trace`/`verify`/`bench`) + `.lgb` binary board codec.
4. WASM surface (wasm-pack, SIMD128, zero-copy state views) + cross-engine equivalence tests.
5. Node native addon (napi-rs, async background run, coherent snapshots) + cross-engine tests.

Planned:

6. Adaptive multithreading (rayon escape hatch that triggers only on heavy ticks).
7. Hand-written SIMD kernels for wide-fan-in gates.
8. (v2) spatial partitioning for sustained huge boards; optional threaded-WASM variant.

---

## License

See [`LICENSE`](LICENSE) (GNU AGPL-3.0).
