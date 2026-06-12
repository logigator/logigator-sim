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
├── corpus/         # golden board fixtures + per-tick traces (tick-exact correctness),
│                   # benchmark boards + results, and the C++-oracle generator tools
└── justfile        # one-command build/test/bench recipes
```

---

## Building

### Prerequisites

- **Rust** (stable; MSRV 1.85, edition 2024) — `rustup target add wasm32-unknown-unknown` for WASM.
- **Node.js ≥ 20** — for the Node addon and to run the WASM/Node test suites.
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
import { Simulation } from "@logigator/sim/node";

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
import init, { Simulation } from "@logigator/sim/wasm";
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

## API reference

### Shared types

These types are the same contract across all three surfaces. Numeric values are the wire representation at the Node / WASM boundary.

**SimState** — lifecycle of a `Simulation`:

| value | Rust | Node / WASM |
|------:|------|-------------|
| 0 | `SimState::Uninitialized` | `SimState.Uninitialized` |
| 1 | `SimState::Stopped` | `SimState.Stopped` |
| 2 | `SimState::Running` | `SimState.Running` |
| 3 | `SimState::Stopping` | `SimState.Stopping` |

**InputEvent** — how a `triggerInput` / `trigger_input` payload is applied:

| value | name | meaning |
|------:|------|---------|
| 0 | `Cont` | Set-and-hold: outputs latch until changed again |
| 1 | `Pulse` | One-tick pulse: outputs assert for one tick then auto-clear |

**BoardDescriptor** — the board shape accepted by every surface constructor:

```ts
// Node / WASM  (JSON or JS object; "links" key)
interface BoardDescriptor    { links: number; components: ComponentDescriptor[]; }
interface ComponentDescriptor { type: number; inputs: number[]; outputs: number[]; ops?: number[]; }
```
```rust
// Rust  ("link_count" field)
pub struct BoardDescriptor   { pub link_count: u32; pub components: Vec<ComponentDescriptor>; }
pub struct ComponentDescriptor { pub ty: CompType; pub inputs: Vec<u32>; pub outputs: Vec<u32>; pub ops: Vec<u32>; }
```

Component ids in every response are **submission-order**: component 0 is the first element of the
`components` array, component 1 the second, and so on.

---

### Node.js

Subpath `@logigator/sim/node` of the `@logigator/sim` package (ESM only).

#### Construction

```ts
new Simulation(board: BoardDescriptor): Simulation
Simulation.fromBinary(buf: Buffer): Simulation     // compact .lgb binary
Simulation.fromJson(json: string): Simulation      // JSON BoardDescriptor string
```

#### Running

```ts
sim.tick(): void
sim.run(config?: RunConfig): void                    // blocking; requires finite ticks or ms
sim.runAsync(config?: RunConfig): Promise<void>      // background worker; unbounded allowed
sim.stop(): void                                     // cooperative interrupt at next batch boundary
```

```ts
interface RunConfig { ticks?: number; ms?: number; }
```

#### State reads

```ts
sim.getStatus(): JsStatus                             // lock-free; safe while running
sim.link(id: number): boolean                         // coherent only when stopped
sim.linkCount(): number
sim.componentCount(): number
sim.getOutputs(): Buffer                              // 1 byte (0/1) per output pin, component-major
sim.snapshot(delta: boolean, threshold: number): Promise<JsSnapshot>
```

`snapshot` resolves at the next tick boundary; the worker copies the state and resumes without
pausing. `delta: true` requests a delta snapshot (changed links only); `threshold` is the changed
fraction (0–1) above which it falls back to a full copy.

```ts
interface JsStatus {
  state: number;         // SimState numeric value
  tick: number;
  speed: number;         // ticks/s (exponential moving average)
  linkCount: number;
  componentCount: number;
}

interface JsSnapshot {
  tick: number;
  isDelta: boolean;
  links?: Buffer;        // Full: packed link_state bits (byte l>>3, bit l&7)
  ids?: Buffer;          // Delta: changed link ids (u32 LE)
  values?: Buffer;       // Delta: packed values — bit i ↔ ids[i]
}
```

#### Input / cleanup

```ts
sim.triggerInput(compId: number, event: number, state: boolean[]): void
sim.destroy(): void    // stops run and joins worker; idempotent
```

---

### WASM (browser)

Subpath `@logigator/sim/wasm` of the `@logigator/sim` package (ESM only; local builds land
in `crates/sim-wasm/pkg/`). All methods are synchronous; single-threaded JS drives the
ticks so reads are always coherent.

#### Initialization

```ts
import init, { Simulation, SimState, InputEvent } from "@logigator/sim/wasm";
await init();   // must be awaited once before using any export
```

#### Construction

```ts
new Simulation(descriptor: BoardDescriptor): Simulation
Simulation.fromBinary(board_bin: Uint8Array): Simulation
Simulation.fromJson(json: string): Simulation
```

#### Running

```ts
sim.tick(): void
sim.run(config?: RunConfig): void
sim.runAsync(config?: RunConfig): Promise<void>   // cooperative; yields between batches
sim.stop(): void
```

#### State reads

```ts
sim.getStatus(): SimStatus
sim.link(id: number): boolean
sim.linkCount(): number
sim.componentCount(): number
sim.getOutputs(): Uint8Array                     // 1 byte (0/1) per output pin, component-major
sim.snapshot(delta: boolean, threshold: number): SnapshotView
```

```ts
interface SimStatus {
  state: SimState;
  tick: number;
  speed: number;
  link_count: number;
  component_count: number;
}
```

`snapshot` returns a `SnapshotView` — zero-copy pointers into linear memory, valid until the next
`tick()` / `run()` / allocating call. Re-acquire after any WASM memory growth detaches the JS
buffer.

```ts
class SnapshotView {
  is_delta: boolean;
  tick: number;
  ptr: number; len: number;               // Full: packed link_state (byte l>>3, bit l&7)
  values_ptr: number; values_len: number; // Delta: packed values — bit i ↔ id[i]
  free(): void;
}
```

For a delta snapshot, `ptr`/`len` hold the changed link ids (u32 LE) and `values_ptr`/`values_len`
hold the packed values.

#### Input / cleanup

```ts
sim.triggerInput(comp_id: number, event: InputEvent, state: boolean[]): void
sim.destroy(): void   // alias: sim.free()
```

**`SimState` and `InputEvent`** are exported as namespace objects with numeric constants:

```ts
SimState.Uninitialized  // 0
SimState.Stopped        // 1
SimState.Running        // 2
SimState.Stopping       // 3

InputEvent.Cont         // 0 — set-and-hold
InputEvent.Pulse        // 1 — one-tick pulse
```

---

### Rust (`sim_core`)

#### Board construction

```rust
BoardBuilder::new(link_count: u32) -> BoardBuilder
boardbuilder.component(ty: CompType, inputs: &[u32], outputs: &[u32], ops: &[u32]) -> u32
boardbuilder.finish(self) -> BoardDescriptor
```

#### Simulation lifecycle

```rust
Simulation::from_descriptor(desc: &BoardDescriptor) -> Result<Simulation>
Simulation::new(board: Board) -> Result<Simulation>
```

#### Running

```rust
sim.tick(&mut self)
sim.run(&mut self, cfg: RunConfig) -> Result<()>
sim.stop(&mut self)   // sets state to Stopping; effective at next tick boundary
```

```rust
pub struct RunConfig {
    pub ticks: u64,              // default: u64::MAX
    pub timeout: Option<Duration>,
}
RunConfig::from_float_bounds(ticks: Option<f64>, ms: Option<f64>) -> RunConfig
```

#### State reads

```rust
sim.status(&self) -> Status
sim.state(&self) -> SimState
sim.tick_count(&self) -> u64
sim.link(&self, id: u32) -> bool
sim.link_words(&self) -> &[AtomicU64]   // zero-copy borrow of packed link_state (u64 LE words)
sim.link_bytes(&self) -> Vec<u8>        // packed ceil(link_count/8)-byte copy; link l → byte l>>3 bit l&7
sim.output(&self, comp_id: u32, pin: usize) -> bool
sim.output_bytes(&self) -> Vec<u8>      // 1 byte (0/1) per output pin, component-major
```

```rust
pub struct Status {
    pub state: SimState,
    pub tick: u64,
    pub speed: u32,          // ticks/s
    pub link_count: u32,
    pub component_count: u32,
}
```

#### Snapshots

```rust
sim.snapshot(&mut self, cfg: SnapshotConfig) -> SnapshotInfo
sim.snapshot_ids(&self) -> &[u32]    // changed link ids of the last Delta
sim.snapshot_values(&self) -> &[u8]  // packed values — bit i ↔ snapshot_ids()[i]
```

```rust
pub struct SnapshotConfig {
    pub delta: bool,
    pub delta_threshold: f32,   // default 0.125 — fall back to Full above this fraction
}

pub struct SnapshotInfo {
    pub is_delta: bool,
    pub tick: u64,
    pub changed: usize,   // changed link count (Delta) or total link_count (Full)
}
```

The first `snapshot` call after construction always returns a `Full` (a delta needs a baseline).
For a `Full`, read the state via `link_words()` or `link_bytes()`. For a `Delta`, read
`snapshot_ids()` / `snapshot_values()`.

#### Input

```rust
sim.trigger_input(&mut self, comp_id: u32, event: InputEvent, state: &[bool]) -> Result<()>
```

#### Errors

```rust
pub enum SimError {
    UnknownComponentType(u16),
    LinkOutOfRange { idx: u32, link: u32, count: u32 },
    BadArity { idx: u32, ty: CompType, ins: usize, outs: usize, ops: usize },
    NotAnInput(u32),
    BadBinary(String),
}
```

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

The goldens are generated from the *published* C++ engine (`@logigator/logigator-simulation`), never
from this engine — regenerate them with `just setup-corpus && just gen-golden`. Two deliberate
divergences (RNG values, SR flip-flop edge behavior) are pinned by Rust unit tests instead.

---

## Performance

Benchmarks against the original C++ engine — boards, methodology, per-change measurements, and the
negative results (changes tried, measured, and reverted) — live in
[`corpus/bench/RESULTS.md`](corpus/bench/RESULTS.md). Run them with `just bench`, `just bench-node`,
`just bench-wasm`, and `just bench-cpp`.

The engine is deliberately **single-threaded**: an adaptive multithreaded driver was built,
profiled as a net loss at every realistic board size, and removed. The wins come from the
algorithmic side — change-driven scheduling, incremental driver counts, and the cache-friendly SoA
layout.

---

## License

See [`LICENSE`](LICENSE) (GNU AGPL-3.0).
