# Changelog

All notable changes to `@logigator/sim` and the `sim` CLI are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor bump may break
the API, the board format, or per-tick behavior; every such change is listed under **Breaking**.

## [Unreleased]

### Breaking

- The board format names its parameters and drops its link count:
  - A board is `{ components }`; `links` is gone, and the links are numbered from 0 to the highest
    id a pin names. A component may leave out an empty `inputs` or `outputs`.
  - `ops` is replaced by named parameters: a CLK's `period` and `mode`, a RAM's `read`, a ROM's
    `data`, a recorder's `capacity` and `recordTicks`, and an addressable register's `dataBus`. A
    parameter on any other type, or a missing `period`, `capacity` or `dataBus`, is refused as
    `BadParam { idx, ty, param, reason }`.
  - A ROM's `data` is bytes (a `Uint8Array` or an array of 0–255 in JS, an array in JSON). Short
    data is still zero-filled; data longer than the ROM's table is refused instead of cut off.
  - The CLI's JSON boards refuse unknown fields, such as a leftover `links` or `ops`. The JS
    bindings ignore them, so a board that still passes `ops` is refused for a CLK, recorder or
    addressable register but silently gets an all-zero ROM and a RAM in its default mode.
  - A link id must be below 2³¹ (`LinkIdOverflow`); `LinkOutOfRange` and `LinkCountOverflow` are
    gone, and `BadArity` no longer carries an `ops` count.
  - `.lgb` is version 3: no link count in the header, and the parameters follow each component's
    pins, flagged in the reserved `u16` after its type id.
  - In Rust, the parameters are optional fields of `ComponentDescriptor` (with the `ClkMode` and
    `RamRead` enums; `record_ticks` replaces `RECORD_TICKS`), set with struct-update syntax over
    `ComponentDescriptor::new(ty, inputs, outputs)`. `BoardBuilder::new()` takes no link count,
    `component()` no `ops`, and `add()` appends a `ComponentDescriptor`.
- A CLK is a square wave by default, `period` ticks low then `period` high; the old one-tick pulse
  after `period` ticks low is `mode: "pulse"`. A period of 1 is unchanged. A halted CLK restarts
  its cycle once its halt input goes low, instead of resuming its count where the halt caught it.
- A RAM's outputs follow its address: they show the addressed word one tick after the address or
  the memory (including through `writeState`) changes, with no clock. Writes still happen on a
  rising clock. The old outputs, which change only on a rising clock, are `read: "clocked"`.
- A MUX takes no parameter: its select width is the one `s` with `s + 2ˢ` inputs. A DEMUX takes
  its select bits first, as a MUX does: `[s0, …, s(n-1), data]` instead of `[data, s0, …]`.
- The LED matrix (type 204) is a generic addressable register whose outputs are its stored bits
  (`CompType::AddressableRegister` in Rust, formerly `LedMatrix`). Its `dataBus` is its data bus
  width, from 1 to its output count, instead of a size where anything above 4 meant 8. Its outputs
  form words of that width, the last possibly partial; its address bus is
  ⌈log₂(⌈outputs / dataBus⌉)⌉ bits, so every output can be written, and its input count must be
  exactly address + data + clock. The editor's 4×4, 8×8 and 16×16 matrices keep their pins with
  `dataBus` 4, 8 and 8.
- Node.js 22 or newer is required; Node.js 20 is end-of-life.
- Building from source needs Rust 1.88 or newer.

### Added

- Invalid links: a link is invalid while its drivers disagree, or while an SR flip-flop clocked
  with S and R both high drives it (Q set, Q̄ low, until a clock with only S or only R). It keeps
  its wired-OR value and nothing downstream is flagged. Snapshots list them: `snap.invalid` on
  Node, `invalidPtr` / `invalidLen` (bytes of `u32` ids) on WASM, and `snapshot_invalid()`,
  `link_invalid(id)` and `invalid_links()` in Rust; `sim trace` frames carry a non-empty `invalid`
  list.
- An encoder can have a valid output after its number bits, high whenever any input is, so input 0
  alone differs from no input: `2ⁿ` inputs take `n` outputs, or `n + 1` with the valid pin.

### Fixed

- An addressable register whose word count is not a power of two no longer writes past its own
  outputs when addressed beyond its last word, which drove the next component's output pins or
  crashed the simulation (a trap in WASM, a thrown error in Node). Such a write now lands only on
  the outputs that exist.
- A CLK whose period is above 2³¹ no longer fires every tick.
- A link driven by more than 65 535 outputs is refused as `TooManyDrivers { link, count, max }`.
  Its count of high drivers used to wrap, so with every driver high it could read low.

## [0.5.0] - 2026-10-16

### Breaking

- `triggerInput` / `trigger_input` and `InputEvent` are removed, in Rust, Node and WASM. A
  `UserInput` is now driven by a queue of timed frames: `submitInput(writes)` queues them and
  `inputProgress(out)` reports how many event frames are still pending per input. The old calls
  map onto frames as `Cont(s)` → `{ comp, frames: [{ state: s, coalesce: true }] }` and
  `Pulse(s)` → `{ comp, frames: [{ state: s }, { state: <all false> }] }`; a pulse that should
  leave an input's other pins alone gives them `null`.
- An input write reaches its wires one tick after it is submitted: one tick sooner than before, and
  the same on a wire shared with another driver, where it used to arrive a tick early or hide a
  one-tick pulse. An input's pins and wires now change in the same tick.
- A frame's `state` must have exactly one entry per output pin; the old call padded a short array
  with `false`.
- CLI fixtures' `triggers` field is no longer read; timed input is `inputWrites` only (see
  Added). A trigger at tick T becomes a request at tick T + 1 (`cont` → one frame, `pulse` → the
  set frame then an all-false frame), which keeps the links of its trace unchanged.
- `sim verify`, which ran the old `test.js` fixture format, is removed.
- In `contract.d.ts`, `Simulation` and `SimulationConstructor` take a second type parameter,
  `Async`: `true` for the Node binding, whose state reads return promises, `false` for WASM.
- `Simulation.fromJson` is removed from the Node and WASM bindings and from `contract.d.ts`; use
  `new Simulation(JSON.parse(json))`.
- A board is validated once, when it is compiled, whichever way it comes in, so a malformed board
  gets the same error in the CLI, Node and WASM. An unknown type is reported as
  `UnknownComponentType { idx, ty }`, naming the component, and a type id or negated pin index
  whose low 16 bits name a real type or pin is rejected instead of narrowed onto it.
- In Rust, `ComponentDescriptor` holds plain numbers: `ty` is the `u32` type id and
  `neg_inputs` / `neg_outputs` are `Vec<u32>`; `BoardBuilder::component_neg` takes `&[u32]`.
  `CompType::id()` returns `u32`, `CompType::try_from_u16` is now `CompType::from_id(u32)`, and
  `CompType` no longer implements serde or `TryFrom<u16>`. `NegateOutOfRange::pin` is a `u32`,
  `decode_board` no longer checks type ids, and `encode_board` returns a `Result`, refusing ids
  too wide for the format.

### Added

- Input frames carry a `hold` (minimum ticks on the pins before the next frame), a `null` per pin
  to keep that pin's value, and a `coalesce` flag for levels: coalescing frames submitted between
  two ticks merge per pin and reach the circuit once. `replace: true` drops an input's pending
  frames and ends its running hold. Sequencing happens inside the tick loop, so the trace no longer
  depends on batch sizes or host timing.
- Each input holds at most 65 536 pending event frames; a write over the limit is refused whole.
- On Node, `submitInput` validates on the JS thread and returns without waiting for the worker.
- CLI fixtures accept an `inputWrites` field, `[{ tick, writes }]`, holding `submitInput` requests
  in the JS shape; `run`, `trace` and `bench` submit each once the simulation has run `tick`
  ticks.
- Component state, in Rust, Node and WASM: `stateVersions` fills a caller-owned buffer with a
  version per component that changes whenever its state does, `readState` / `writeState` read and
  write a RAM's memory (bit-packed words, the layout of a ROM's `ops`), and `readRecords` reads a
  recorder's records together with its count. Reads are coherent at a tick boundary; on Node
  `readState` and `readRecords` return promises, and `writeState` is checked on the JS thread and
  queued in order with `submitInput`.
- The recorder (type 201) captures a data bus of up to 32 bits on each rising trigger edge into a
  ring of its last `capacity` records, optionally with the tick of each. It is how a terminal gets
  every character its circuit writes, one per tick if need be.
- New errors for the state API: `ComponentOutOfRange`, `StateRange`, `StateReadOnly`,
  `NotARecorder`, `RecordsAhead` and `RecordTicks`.

### Changed

- On WASM, a malformed descriptor or run config is reported with the field at fault, such as
  `components[3].inputs must be an array of u32`.
- The README is rewritten as a shorter guide to what the engine is and how to use it, with a
  component table listing each type's inputs, outputs, `ops` and behavior; the full API types are
  in `@logigator/sim/contract`. It corrects the CLK's `ops[0]`, which is the number of ticks the
  clock stays low before a one-tick high, not ticks per half-cycle, and the WASM snapshot's field
  names (`isDelta`, `valuesPtr`, `valuesLen`).

### Performance

- The WASM build is about half the size: `sim_wasm_bg.wasm` is 108 KB instead of 225 KB (45 KB
  instead of 95 KB gzipped), as it no longer carries serde, a JSON parser or a stable sort.
- `new Simulation(descriptor)` on WASM builds a large board 2–3× faster (a 200k-component board in
  ~30 ms instead of ~80 ms).

### Fixed

- `run()` with an `ms` too large to represent, such as `Infinity`, no longer crashes (a trap in
  WASM, a thrown panic in Node); an out-of-range `ms` or `ticks` counts as no bound, so `run()`
  refuses it as unbounded and `runAsync()` runs until `stop()`. `sim bench --ms` refuses it too.

## [0.4.0] - 2026-06-18

### Breaking

- The negated-pin descriptor fields are renamed from `negatedInputs`/`negatedOutputs` to
  `negInputs`/`negOutputs`, in JSON boards and in the Node and WASM bindings. The `.lgb` binary
  format is unchanged.
- `UserInput` now maps only from component type id `200`. Ids `201`–`299` used to alias it and are
  now rejected as unknown component types.
- Boards whose `link_count` exceeds 2^31 are rejected with `LinkCountOverflow`, because the top bit
  of a link id now carries the negation flag.

### Changed

- `sim bench` is time-bound by default: each repeat runs for `--ms` milliseconds (default 1000) and
  reports the ticks that landed. Pass `--ticks N` for the previous fixed-tick mode.

### Performance

- Pin negation is packed into the top bit of the link ids, so reading or driving a pin no longer
  loads a separate negation bitset. This recovers most of the throughput 0.3.0 lost: on the
  real-world CPU benchmark board the cost drops from 8.4% to 3.2%.

## [0.3.0] - 2026-06-17

### Breaking

- The `.lgb` binary board format is now version 2, which stores per-pin negation. Version 1 files
  are rejected with `unsupported version`; re-export them from JSON. JSON boards without the new
  fields load unchanged.

### Added

- Per-pin negation: `negatedInputs`/`negatedOutputs` on a component descriptor list the pin indices
  to invert, in JSON boards and in the Node and WASM bindings.
- A negated clock or enable pin inverts its trigger sense: a negated clock makes the component
  falling-edge triggered, with no spurious edge at power-on.

### Performance

- Negation is always applied, even on boards that don't use it, which costs some throughput (about
  9% on the real-world CPU benchmark board).

## [0.2.0] - 2026-06-14

### Breaking

- The WASM binding's `getStatus()` fields are camelCased to match the Node binding:
  `link_count` → `linkCount`, `component_count` → `componentCount`.
- The WASM `SnapshotView` fields are camelCased: `is_delta` → `isDelta`, `values_ptr` →
  `valuesPtr`, `values_len` → `valuesLen`.

### Added

- `@logigator/sim/contract`: a type-only subpath with the public interface that both bindings
  implement.
- The Node binding exports the `SimState` and `InputEvent` enums. `getStatus().state` and
  `triggerInput()`'s event argument use them (the numeric values are unchanged), and `runAsync()`
  and `snapshot()` now have precise `Promise` return types.

### Fixed

- Time-bounded runs land close to their deadline instead of overshooting by up to a full batch.
- Periodic sampling during runs no longer aliases short-period signals: the state seen at the end of
  a run, such as a NOT gate feeding itself, no longer always lands on the same phase.

## [0.1.3] - 2026-06-14

### Changed

- WASM `runAsync()` runs in fixed wall-clock slices instead of fixed 4096-tick batches, so frame
  pacing and `stop()` latency stay constant regardless of board size, and the speed readout stays
  live during the run.

## [0.1.2] - 2026-06-12

### Fixed

- `@logigator/sim/wasm/sim_wasm_bg.wasm` is in the package exports, so the `.wasm` file can be
  imported directly.

## [0.1.1] - 2026-06-12

### Fixed

- The published package exposes the `logigator-sim` command; 0.1.0 shipped the launcher without the
  `bin` entry.
- The published package includes the README and LICENSE.

## [0.1.0] - 2026-06-12

Initial release of the Rust simulation engine.

### Added

- A change-driven tick loop with the full Logigator component set, verified tick-exact against the
  previous C++ engine with two deliberate exceptions: the RNG component is seeded per component
  (reproducible and independent of evaluation order), and the SR flip-flop is rising-edge triggered
  like the D and JK flip-flops.
- `@logigator/sim/node`: a native Node.js addon with synchronous runs, `runAsync()` on a background
  worker thread, and coherent full or delta snapshots while a run is in progress.
- `@logigator/sim/wasm`: a WebAssembly build (web target, SIMD128) with zero-copy state views and a
  `runAsync()` that yields to the event loop between batches.
- The `sim` CLI (`run`, `trace`, `verify`, `bench`), installed as `logigator-sim` through npm and
  attached to the GitHub release as standalone binaries.
- JSON and `.lgb` binary board formats.
- Prebuilt binaries for Linux x64/arm64 (glibc 2.17+), Linux x64 musl, macOS x64/arm64, and Windows
  x64.

[Unreleased]: https://github.com/logigator/logigator-sim/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/logigator/logigator-sim/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/logigator/logigator-sim/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/logigator/logigator-sim/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/logigator/logigator-sim/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/logigator/logigator-sim/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/logigator/logigator-sim/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/logigator/logigator-sim/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/logigator/logigator-sim/releases/tag/v0.1.0
