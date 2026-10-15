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
- `sim run` and `sim trace` submit a fixture's triggers one tick boundary after their scheduled
  tick, so the links still match the corpus goldens; `UserInput` pins now change with their
  wires. `sim verify` latches `inputTriggers` one tick sooner.

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

### Fixed

- A pulse no longer resets a level set on the same input, and writes between the same two ticks no
  longer collapse unless they ask to: each event frame gets its own ticks on the wire.
- An input write on a wire shared with another driver could reach the wire a tick early or hide a
  one-tick pulse; an input's delay to its wire is now always one tick.

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
