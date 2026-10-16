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
- **Per-pin negation** — any input or output pin can read/drive the inverted value with **zero**
  added delay (folded into the read/drive step), so NAND/NOR and negated clocks cost no extra tick.
- **Versioned component state** — a RAM's memory and a recorder's captured values are read (and a
  RAM's memory written) through one API, with a per-component version so a host re-reads only what
  moved.

---

## Workspace layout

```
logigator-sim/
├── crates/
│   ├── sim-core/   # the engine (lib): SoA board, tick loop, components, .lgb codec, snapshots
│   ├── sim-cli/    # native CLI (`sim run|trace|bench`)
│   ├── sim-wasm/   # WebAssembly surface (wasm-bindgen / wasm-pack), zero-copy state views
│   └── sim-node/   # Node.js native addon (napi-rs), async background run, coherent snapshots
├── corpus/         # golden board fixtures + per-tick traces (tick-exact correctness),
│                   # benchmark boards + results, and the generator/bench tools
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

Numeric `type` ids are stable wire identifiers (shared across all surfaces). **Inputs** and
**Outputs** are the permitted pin counts (validated at compile); pin **order** is significant, as
noted per type. **`ops`** is the per-component parameter array — its meaning is type-specific and
listed below; a `—` means the type takes no `ops` (a non-empty array is rejected at compile).

| id  | Component     | Inputs | Outputs | `ops`          | Behavior & pin order |
|-----|---------------|--------|---------|----------------|----------------------|
| 1   | NOT           | 1      | 1       | —              | `out = !in` |
| 2   | AND           | ≥ 2    | 1       | —              | `out = in₀ & in₁ & …` |
| 3   | OR            | ≥ 2    | 1       | —              | `out = in₀ \| in₁ \| …` |
| 4   | XOR           | ≥ 2    | 1       | —              | `out = in₀ ^ in₁ ^ …` |
| 5   | DELAY         | 1      | 1       | —              | Buffer — copies the input through (subject to the one-tick propagation every component has). |
| 6   | CLK           | 1      | 1       | `[period]`     | Free-running clock; `ops[0]` = ticks per half-cycle (≥ 1). Input 0 = enable: held **high** freezes the output low, **low** lets it run. |
| 10  | Half adder    | 2      | 2       | —              | Inputs `a, b`; output 0 = sum (`a ^ b`), output 1 = carry (`a & b`). |
| 11  | Full adder    | 3      | 2       | —              | Inputs `a, b, cin`; output 0 = sum, output 1 = carry-out. |
| 12  | ROM           | 1–16   | 1–64    | `[bytes…]`     | Combinational lookup. Address = `Σ inᵢ << i`; the output word is `outputs` bits read from the `ops` blob starting at bit `address × outputs`. `ops` is the bit-packed table (zero-padded to size). |
| 13  | D flip-flop   | 2      | 2       | —              | Input 0 = D, input 1 = clock (rising-edge); output 0 = Q, output 1 = Q̄. |
| 14  | JK flip-flop  | 3      | 2       | —              | Inputs 0 = J, 1 = clock (rising-edge), 2 = K; output 0 = Q, output 1 = Q̄. |
| 15  | SR flip-flop  | 3      | 2       | —              | Inputs 0 = S, 1 = enable (rising-edge), 2 = R; output 0 = Q, output 1 = Q̄. |
| 16  | RNG           | 1      | ≥ 1     | —              | Input 0 = enable; draws random outputs on the rising edge (pure function of per-component seed + tick). |
| 17  | RAM           | ≥ 3    | ≥ 1     | —              | Inputs `[address, data, write-enable, clock]`; word size = output count, `addressSize = inputs − outputs − 2` (≤ 24). Rising clock (last input) reads, or writes when write-enable is high. The memory is readable and writable by the host; see [Component state](#component-state). |
| 18  | Decoder       | 1–16   | ≥ 2     | —              | One-hot; requires `outputs == 2^inputs`. Drives `out[Σ inᵢ << i]` high, the rest low. |
| 19  | Encoder       | ≥ 2    | 1–16    | —              | Requires `inputs == 2^outputs`. Outputs the binary index of the highest powered input. |
| 20  | MUX           | ≥ 3    | 1       | `[selectBits]` | `ops[0]` = number of select inputs `s` (1–16); requires `inputs == 2^s + s`. First `s` inputs are the selector; output = the chosen one of the following `2^s` data inputs. |
| 21  | DEMUX         | ≥ 2    | ≥ 2     | —              | Input 0 = data, inputs `1…` = select; requires `outputs == 2^(inputs − 1)`. Routes data to `out[index]`, the rest low. |
| 200 | UserInput     | 0      | ≥ 1     | —              | External source, one unit per input device (a keyboard is one unit with 7 data pins and a strobe). Its outputs are driven by the timed frames queued with `submitInput` / `submit_input`; see [Input](#input). |
| 201 | Recorder      | 2–33   | 0       | `[capacity, flags]` | Captures a data bus: inputs `[d₀ … dₙ₋₁, trigger]` (`1 ≤ n ≤ 32`). On a rising trigger (last input) it records `Σ dᵢ << i`, sampling what a D flip-flop on the same trigger latches; a negated trigger records on the falling edge. Keeps the last `capacity` records (1 – 2²⁰); `flags` bit 0 also records each record's tick, every other bit must be 0. Read with `readRecords`; see [Component state](#component-state). |
| 204 | LED matrix    | ≥ 5    | ≥ 4     | `[busWidth]`   | Display; `ops[0]` selects the data-bus width (`> 4` → 8, else 4). Inputs `[address, data, clock]`; outputs are the LEDs (and hold the stored state), one data row latched per rising clock. |

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

**BoardDescriptor** — the board shape accepted by every surface constructor:

```ts
// Node / WASM  (JSON or JS object; "links" key)
interface BoardDescriptor    { links: number; components: ComponentDescriptor[]; }
interface ComponentDescriptor {
  type: number; inputs: number[]; outputs: number[]; ops?: number[];
  negInputs?: number[]; negOutputs?: number[];
}
```
```rust
// Rust  ("link_count" field)
pub struct BoardDescriptor   { pub link_count: u32; pub components: Vec<ComponentDescriptor>; }
pub struct ComponentDescriptor {
    pub ty: CompType; pub inputs: Vec<u32>; pub outputs: Vec<u32>; pub ops: Vec<u32>;
    pub neg_inputs: Vec<u16>; pub neg_outputs: Vec<u16>;
}
```

Component ids in every response are **submission-order**: component 0 is the first element of the
`components` array, component 1 the second, and so on.

**Negated ports.** `negInputs` / `negOutputs` list the *pin indices* (into `inputs` /
`outputs`) of a component that read or drive the inverted value, adding **zero** delay — a NAND is an
AND with `negOutputs: [0]`, a NOR an AND with both inputs negated. Any pin of any type may be
negated, including a clock or enable: a negated clock negates the trigger, so a rising-edge component
becomes falling-edge. Both fields default to empty (a board without them behaves exactly as before).
The indices are validated against the component's pin count at compile.

### Input

Each `UserInput` component (an **input**) owns a FIFO of timed **frames**. The host queues them
with `submitInput` (`submit_input` in Rust); the engine applies them inside the tick loop, so the
spacing between frames is counted in ticks, whatever the host's batch sizes or timer jitter.

```ts
interface InputFrame {
  state: (boolean | null)[];  // exactly one entry per output pin; null = keep
  hold?: number;              // integer ≥ 1, default 1
  coalesce?: boolean;         // default false = event
}
interface InputWrite {
  comp: number;               // component id of a UserInput
  frames: InputFrame[];
  replace?: boolean;          // default false
}
sim.submitInput(writes: InputWrite[]): void
sim.inputProgress(out: Uint32Array): void
```

- **`state`** drives each pin's *logical* value (a pin in `negOutputs` drives the inverse onto its
  wire, and rests high). `null` **keeps** the value the pin shows when the frame is *applied*, so a
  `null` sees the writes of frames still pending ahead of it, and controls sharing one input only
  write their own pins. An all-`null` frame changes nothing but still takes its `hold`: a gap.
- **`hold`** is the minimum number of ticks the frame stays on the pins before the input's next
  frame replaces it. The last frame stays until another one is applied.
- **Events and levels.** An event frame (`coalesce: false`) is never dropped or merged by the
  engine. A coalescing frame merges per pin into the input's last *pending* frame if that one is
  coalescing too (the later value wins; a `null` keeps the earlier entry; `hold` comes from the
  later frame). It never merges into a frame already on the pins, nor past a pending event. So a
  switch flipped five times between two ticks reaches the circuit once, in its final position.
- **`replace`** drops the input's pending frames and ends any running hold, then queues `frames`;
  the first is applied on the next tick. `{ comp, replace: true, frames: [] }` cancels and keeps the
  pins as they are.
- **No pulse primitive.** A pulse is two frames, the second saying what the pins return to: a key
  press is `[{ state: data + strobe }, { state: data }]`.

**Timing.** Inputs are serviced at the start of a tick, before the read phase:

- A frame submitted at tick count *t* to an idle input is applied in tick *t + 1*: after that tick
  it is on `getOutputs()` / `output()` and on the wires (`link()`, snapshots). Its consumers compute
  on it in the same tick, so a NOT fed by the input shows the inverse after tick *t + 2* — the same
  one-tick delay as every other component, also on a wire shared with other drivers.
- The input's next frame is applied in tick *n + hold* (*n* = the tick its predecessor was applied
  in), or on the tick after it is submitted if that comes later.
- Frames of different inputs due on the same tick are applied together, in ascending component id.
- The trace depends only on what was submitted at which tick count, not on whether the ticks run as
  `tick()`, `run()` batches of any size, or `runAsync`. (On Node the engine runs on its own thread,
  so *where* a submission lands relative to a running `runAsync` can differ between runs; grouping,
  order and spacing still hold.)

**Validation** is all or nothing: on any error, nothing in the request is queued. A request is
refused if a `comp` is not a `UserInput` (`NotAnInput`), a `state` has the wrong length
(`InputWidth`) or an entry that is not `true` / `false` / `null` — `0`, `undefined` or an array hole
(`InputState`), a `hold` is not an integer ≥ 1 (`InputHold`), the same `comp` appears twice
(`DuplicateInput`), or a write would leave more than **65 536** event frames pending on its input
(`InputQueueFull`; coalescing frames don't count, and a `replace` starts from zero). A host sending
more submits in chunks as progress drains.

**Progress.** `inputProgress(out)` fills `out[i]` with the number of input *i*'s **event** frames
not yet applied, where input *i* is the *i*-th `UserInput` in the descriptor's `components`. `out`
must hold at least one entry per input; allocate it once and reuse it. Coalescing frames are never
counted. The pending event frames are always the last *k* the host submitted to that input, so a
host that remembers how many frames each item took (2 per typed character) can show "Sending N…".

### Component state

A host reads what a component holds inside — a RAM's memory, a recorder's records — through one
versioned API, and can write a RAM's memory (to load a program, or edit it while paused).

```ts
interface Records {
  count: number;          // records ever written
  first: number;          // index of values[0]; first > since means records were lost
  values: Uint32Array;
  ticks?: Float64Array;   // only for a recorder with the tick flag
}
sim.stateVersions(compIds: Uint32Array, out: Uint32Array): void
sim.stateLength(compId: number): number
sim.readState(compId: number, offset: number, length: number): Uint8Array   // Node: Promise<Buffer>
sim.writeState(compId: number, offset: number, bytes: Uint8Array): void
sim.readRecords(compId: number, since: number, max?: number): Records       // Node: Promise<Records>
```

- **Versions.** Every component has a state version, a `u32` that changes (wrapping) whenever its
  state does; a component without state stays at 0. A RAM's moves on every write that changes a
  stored bit, from the circuit or from `writeState`; a recorder's is its record count. Poll
  `stateVersions` once per frame for everything on screen and re-read only what moved; like
  `inputProgress` it fills a buffer allocated once.
- **Reads never change state**, so any number of readers can share a component, and each read is
  coherent: it sees the state at one tick boundary. On Node two reads, or a read and a snapshot,
  may come from different boundaries.
- **Only types whose state is not already on their outputs expose any.** Flip-flops, the LED matrix
  and the clock show theirs on links; a ROM's contents are its `ops`.

**RAM (17).** `stateLength` is `ceil(w × 2^addressSize / 8)` bytes, `w` = the output count. The
memory is the words bit-packed LSB-first, the layout of a ROM's `ops`: the word at address `a`
occupies bits `[a × w, a × w + w)`, bit `b` at byte `b >> 3`, bit `b & 7`, and the last byte is
zero-padded. A RAM holds up to 2²⁴ words, so read the byte range covering the words you show (words
are not byte-aligned for every `w`), and write a partial byte by reading it first. `writeState`
changes the memory only: the outputs keep their value until the next rising clock reads a word.

**Recorder (201).** It holds `count`, the records ever written (never reset while the simulation
lives), and the last `capacity` records, with their ticks when `flags` bit 0 is set. A record's
tick is the `tick` (as `getStatus` reports it) of the first snapshot showing the trigger edge, and
its value is the data links in that same snapshot. `readRecords(comp, since, max)` returns records
from `first = max(since, count − capacity)` on, oldest first, at most `max` of them (default: all it
holds), read together with `count`: `first − since` records were overwritten before the read. Page
through by passing `first + values.length` as the next `since`. A `since` past `count` is an error —
it is a count from another simulation. A recorder has no byte state (`stateLength` 0) and is
read-only.

**Errors, never panics.** A component id out of range (`ComponentOutOfRange`), a byte range past the
end of the state, including an `offset + length` that overflows (`StateRange`), `writeState` on a
type whose state is not writable (`StateReadOnly`), `readRecords` on anything but a recorder
(`NotARecorder`) and a `since` past `count` (`RecordsAhead`) are refused; a refused write changes
nothing. A destroyed simulation takes its state with it, and a rebuilt one starts from the board's
initial state (RAM all zero, recorders empty).

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
sim.submitInput(writes: InputWrite[]): void     // see Input above
sim.inputProgress(out: Uint32Array): void
sim.destroy(): void    // stops run and joins worker; idempotent
```

#### Component state

```ts
sim.stateVersions(compIds: Uint32Array, out: Uint32Array): void   // lock-free; safe while running
sim.stateLength(compId: number): number
sim.readState(compId: number, offset: number, length: number): Promise<Buffer>
sim.writeState(compId: number, offset: number, bytes: Uint8Array): void
sim.readRecords(compId: number, since: number, max?: number): Promise<Records>
```

See [Component state](#component-state). `readState` and `readRecords` are copied on the engine
thread at the next tick boundary, as `snapshot` is. `stateVersions` reads versions the worker
publishes after every batch and every command: exact once `tick()` / `run()` returns or
`runAsync()` resolves, at most one batch behind during a run. `stateLength` and the checks of
`writeState` come from the descriptor, so both are synchronous; a write is then queued in order with
`submitInput` and applied at the next tick boundary, no later than an input request submitted
after it.

`submitInput` validates the request on the calling thread and hands it to the worker without
waiting, so it never blocks on a running simulation. A `tick()` or `run()` after it still applies
it. `inputProgress` counts a request from the moment `submitInput` returns, and is exact once
`tick()` / `run()` returns or `runAsync()` resolves; during a `runAsync` it lags the engine by at
most one batch (4 093 ticks), always reporting high.

---

### WASM (browser)

Subpath `@logigator/sim/wasm` of the `@logigator/sim` package (ESM only; local builds land
in `crates/sim-wasm/pkg/`). All methods are synchronous; single-threaded JS drives the
ticks so reads are always coherent.

#### Initialization

```ts
import init, { Simulation, SimState } from "@logigator/sim/wasm";
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
sim.submitInput(writes: InputWrite[]): void     // see Input above
sim.inputProgress(out: Uint32Array): void
sim.destroy(): void   // alias: sim.free()
```

#### Component state

```ts
sim.stateVersions(compIds: Uint32Array, out: Uint32Array): void
sim.stateLength(compId: number): number
sim.readState(compId: number, offset: number, length: number): Uint8Array
sim.writeState(compId: number, offset: number, bytes: Uint8Array): void
sim.readRecords(compId: number, since: number, max?: number): Records
```

See [Component state](#component-state). Every call is synchronous and sees the state between two
ticks, so state read beside a snapshot belongs to the same tick; a write applies at once.

**`SimState`** is exported as a namespace object with numeric constants:

```ts
SimState.Uninitialized  // 0
SimState.Stopped        // 1
SimState.Running        // 2
SimState.Stopping       // 3
```

---

### Rust (`sim_core`)

#### Board construction

```rust
BoardBuilder::new(link_count: u32) -> BoardBuilder
boardbuilder.component(ty: CompType, inputs: &[u32], outputs: &[u32], ops: &[u32]) -> u32
// As `component`, but marks the listed input/output pin indices as negated:
boardbuilder.component_neg(ty, inputs, outputs, ops, neg_inputs: &[u16], neg_outputs: &[u16]) -> u32
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
pub struct InputFrame<'a> {
    pub state: &'a [Option<bool>],  // one entry per output pin; None = keep
    pub hold: u32,                  // >= 1
    pub coalesce: bool,
}
pub struct InputWrite<'a> {
    pub comp: u32,
    pub replace: bool,
    pub frames: &'a [InputFrame<'a>],
}
pub const MAX_PENDING_EVENTS: u32 = 65_536;

sim.submit_input(&mut self, writes: &[InputWrite<'_>]) -> Result<()>
sim.input_progress(&self, out: &mut [u32])   // panics if out.len() < input_count()
sim.input_count(&self) -> u32
// The check submit_input runs, for a host that queues requests elsewhere (the Node binding):
validate_input_writes(writes: &[InputWrite<'_>], info: impl FnMut(u32) -> Option<InputInfo>) -> Result<()>
```

See [Input](#input) for the semantics and timing.

#### Component state

```rust
pub struct Records { pub count: u64, pub first: u64, pub len: usize }
pub enum StateLayout {
    None,
    Bytes { len: usize, writable: bool },      // RAM
    Records { capacity: u32, ticks: bool },    // recorder
}
pub const MAX_RECORDER_CAPACITY: u32 = 1 << 20;
pub const RECORD_TICKS: u32 = 1;               // recorder `flags` bit

sim.state_version(&self, comp_id: u32) -> Result<u32>
sim.state_len(&self, comp_id: u32) -> Result<usize>
sim.state_layout(&self, comp_id: u32) -> Result<StateLayout>
sim.read_state(&self, comp_id: u32, offset: usize, out: &mut [u8]) -> Result<()>
sim.write_state(&mut self, comp_id: u32, offset: usize, bytes: &[u8]) -> Result<()>
// `ticks` must be `Some` exactly when the recorder records ticks.
sim.read_records(&self, comp_id: u32, since: u64, values: &mut [u32], ticks: Option<&mut [u64]>)
    -> Result<Records>
// The layout from a descriptor alone, and the checks the state methods run, for a host that
// queues state requests elsewhere (the Node binding):
StateLayout::of(desc: &ComponentDescriptor) -> StateLayout
layout.check_read(comp, offset, len) / layout.check_write(comp, offset, len) -> Result<Range<usize>>
```

`read_records` copies `len = min(count − first, values.len())` records from
`first = max(since, count − capacity)`. See [Component state](#component-state) for the semantics.

#### Errors

```rust
pub enum SimError {
    UnknownComponentType(u16),
    LinkOutOfRange { idx: u32, link: u32, count: u32 },
    BadArity { idx: u32, ty: CompType, ins: usize, outs: usize, ops: usize },
    NegateOutOfRange { idx: u32, pin_kind: PinKind, pin: u16, count: u32 },
    NotAnInput(u32),
    InputWidth { comp: u32, frame: usize, got: usize, want: u32 },
    InputState { comp: u32, frame: usize, pin: usize },   // JS bindings only
    InputHold { comp: u32, frame: usize },
    DuplicateInput(u32),
    InputQueueFull { comp: u32, pending: u32, adding: usize, limit: u32 },
    ComponentOutOfRange(u32),
    StateRange { comp: u32, offset: usize, len: usize, size: usize },
    StateReadOnly(u32),
    NotARecorder(u32),
    RecordsAhead { comp: u32, since: u64, count: u64 },
    RecordTicks { comp: u32, records_ticks: bool },   // a tick buffer passed to the wrong recorder
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

Correctness is verified **tick-exact**: every board fixture in `corpus/boards/` is replayed and
diffed tick by tick — every link and every output pin — against its committed trace in
`corpus/golden/`, and the same corpus is replayed through every binding to prove the surfaces agree.
A fixture's timed input is its `inputWrites`: `submitInput` requests, each submitted once the
simulation has run its `tick` ticks.

The goldens are this engine's own output (`just gen-golden` runs `sim trace` over every board), so
they pin per-tick behavior to the commit that last regenerated them. A golden failure is either a
regression or an intended behavior change; for the latter, regenerate the goldens and commit the
reviewed diff with the change.

---

## Performance

Benchmarks — boards, methodology, per-change measurements, and the negative results (changes tried,
measured, and reverted) — live in [`corpus/bench/RESULTS.md`](corpus/bench/RESULTS.md). Run them
with `just bench`, `just bench-node`, `just bench-wasm`, and `just bench-wasm-async`, or compare
against a git ref with `just compare`.

The engine is deliberately **single-threaded**: an adaptive multithreaded driver was built,
profiled as a net loss at every realistic board size, and removed. The wins come from the
algorithmic side — change-driven scheduling, incremental driver counts, and the cache-friendly SoA
layout.

Per-pin negation runs as a **single always-on code path** — there is no no-negation fast lane, so
every board reads the negate masks each tick. That costs a board with no negated pins some throughput
(no added *delay*, only ticks/s) in exchange for one code path to maintain; the measured figures are
in [`corpus/bench/RESULTS.md`](corpus/bench/RESULTS.md).

---

## License

See [`LICENSE`](LICENSE) (GNU AGPL-3.0).
