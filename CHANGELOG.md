# Changelog

All notable changes to `@logigator/sim` and the `sim` CLI are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor bump may break
the API, the board format, or per-tick behavior; every such change is listed under **Breaking**.

## [Unreleased]

### Breaking

- `triggerInput` / `trigger_input` and `InputEvent` are removed, in Rust, Node and WASM. A
  `UserInput` is now driven by a queue of timed frames: `submitInput(writes)` queues them and
  `inputProgress(out)` reports how many event frames are still pending per input. The old calls
  map onto frames as `Cont(s)` → `{ comp, frames: [{ state: s, coalesce: true }] }` and
  `Pulse(s)` → `{ comp, frames: [{ state: s }, { state: <all false> }] }`.
- An input write reaches its wires one tick after it is submitted, one tick sooner than before,
  and an input's pins and wires now change in the same tick.
- A frame's `state` must have exactly one entry per output pin; the old call padded a short array
  with `false`.
- CLI fixtures' `triggers` field is no longer read; timed input is `inputWrites` only (see
  Added). A trigger at tick T becomes a request at tick T + 1 (`cont` → one frame, `pulse` → the
  set frame then an all-false frame), which keeps the links of its trace unchanged.
- `sim verify` is removed.
- In `contract.d.ts`, `Simulation` and `SimulationConstructor` take a second type parameter,
  `Async`: `true` for the Node binding, whose state reads return promises, `false` for WASM.
- `Simulation.fromJson` is removed from the Node and WASM bindings and from `contract.d.ts`; use
  `new Simulation(JSON.parse(json))`.
- In Rust, `ComponentDescriptor` holds plain numbers: `ty` is the `u32` type id and
  `neg_inputs` / `neg_outputs` are `Vec<u32>`; `BoardBuilder::component_neg` takes `&[u32]`.
  `CompType::id()` returns `u32`, `CompType::try_from_u16` is now `CompType::from_id(u32)`, and
  `CompType` no longer implements serde or `TryFrom<u16>`. A board is validated once, when it is
  compiled, whichever way it came in, so `SimError::UnknownComponentType` is
  `{ idx, ty }` and names the component, `decode_board` no longer checks type ids, and
  `encode_board` returns a `Result`, refusing ids too wide for the format.
- A board's type ids and negated pin indices are validated by the engine for every binding, so
  malformed descriptors get the same error messages in the CLI, Node and WASM.

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

- The WASM build is about half the size: `sim_wasm_bg.wasm` is 108 KB instead of 225 KB (45 KB
  instead of 95 KB gzipped), as it no longer carries serde or a JSON parser.
- `new Simulation(descriptor)` on WASM builds a large board 2–3× faster (a 200k-component board in
  ~30 ms instead of ~80 ms).
- On WASM, a malformed descriptor or run config is reported with the field at fault, such as
  `components[3].inputs must be an array of u32`.

### Fixed

- A pulse no longer resets a level set on the same input, and writes between the same two ticks no
  longer collapse unless they ask to: each event frame gets its own ticks on the wire.
- An input write on a wire shared with another driver could reach the wire a tick early or hide a
  one-tick pulse; an input's delay to its wire is now always one tick.
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

[Unreleased]: https://github.com/logigator/logigator-sim/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/logigator/logigator-sim/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/logigator/logigator-sim/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/logigator/logigator-sim/compare/v0.1.3...v0.2.0
[0.1.3]: https://github.com/logigator/logigator-sim/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/logigator/logigator-sim/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/logigator/logigator-sim/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/logigator/logigator-sim/releases/tag/v0.1.0
