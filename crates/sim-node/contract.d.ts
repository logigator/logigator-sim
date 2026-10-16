/**
 * The backend-agnostic public API shared by `@logigator/sim/node` (the napi addon) and
 * `@logigator/sim/wasm` (the browser build). This file is hand-written and is the single source of
 * truth for the surface both bindings must expose; `contract.conformance.ts` statically asserts that
 * each binding's generated types satisfy it, so any drift fails `npm run typecheck`.
 *
 * The two bindings differ only where their execution models force it — the wasm build is
 * single-threaded and reads state zero-copy out of linear memory (a synchronous `snapshot`), while
 * the Node build runs the engine on a worker thread and copies state out at a tick boundary (an
 * async `snapshot`, `readState` and `readRecords`). That single axis is the {@link Simulation} type
 * parameters; everything else is identical.
 *
 * Enum-valued fields are typed `number` here rather than as a union or enum: a numeric union widens
 * to `number` and a TypeScript `enum` is nominal, so `number` is the one type both bindings'
 * `getStatus().state` shapes are assignable to. The canonical {@link SimState} value set is
 * exported below for consumers who want to name it.
 */

/** Lifecycle of a simulation, as it crosses to JS. The discriminants are a frozen wire contract. */
export type SimState =
  | 0 // Uninitialized
  | 1 // Stopped
  | 2 // Running
  | 3; // Stopping

/**
 * One timed step of a `UserInput`'s pins. A frame is applied at the start of a tick and stays on
 * the pins (and their wires) for at least `hold` ticks before the input's next frame replaces it.
 */
export interface InputFrame {
  /**
   * Exactly one entry per output pin. `true` / `false` drive the pin's *logical* value (a pin in
   * `negOutputs` drives the inverse onto its wire); `null` keeps the value the pin shows when the
   * frame is applied.
   */
  state: (boolean | null)[];
  /** Minimum ticks on the pins before the input's next frame replaces it: an integer ≥ 1, default 1. */
  hold?: number;
  /**
   * `false` (default): an event, never dropped or merged by the engine. `true`: a level that merges
   * per pin into the input's last pending frame when that one is coalescing too, so changes between
   * two ticks reach the circuit once, in their final state.
   */
  coalesce?: boolean;
}

/** Frames for one `UserInput` in a {@link Simulation.submitInput} request. */
export interface InputWrite {
  /** Component id of a `UserInput`. */
  comp: number;
  frames: InputFrame[];
  /** Drop the input's pending frames and end any running hold before queueing `frames`; default `false`. */
  replace?: boolean;
}

/**
 * One component in a {@link BoardDescriptor}
 * (`{ type, inputs, outputs, ops?, negInputs?, negOutputs? }`). `negInputs` /
 * `negOutputs` list the *pin indices* (into `inputs` / `outputs`) that read or drive the
 * inverted value, with no added delay.
 */
export interface ComponentDescriptor {
  type: number;
  inputs: number[];
  outputs: number[];
  ops?: number[];
  negInputs?: number[];
  negOutputs?: number[];
}

/** A board description (`{ links, components }`) passed to the constructor / factories. */
export interface BoardDescriptor {
  links: number;
  components: ComponentDescriptor[];
}

/** Optional tick / wall-clock bounds for `run` and `runAsync`. */
export interface RunConfig {
  /** Maximum number of ticks to execute. */
  ticks?: number;
  /** Wall-clock time limit in milliseconds. */
  ms?: number;
}

/**
 * Records read with {@link Simulation.readRecords}: records `first … first + values.length` of
 * `count`. `count`, `first` and the ticks are exact JS numbers.
 */
export interface Records {
  /** Records the recorder has ever written. */
  count: number;
  /** Index of `values[0]`; `first > since` means `first − since` records were overwritten before the read. */
  first: number;
  values: Uint32Array;
  /** The tick of each record (the `tick` of the first snapshot showing its trigger edge), for a recorder with the tick flag only. */
  ticks?: Float64Array;
}

/** `T`, or a `Promise` of it on the binding whose engine runs on another thread. */
export type Out<Async extends boolean, T> = Async extends true ? Promise<T> : T;

/** Snapshot of simulation status returned by {@link Simulation.getStatus}. `state` is a {@link SimState}. */
export interface SimStatus {
  state: number;
  tick: number;
  speed: number;
  linkCount: number;
  componentCount: number;
}

/**
 * The simulation handle. `Snapshot` is the result of {@link snapshot}: the wasm build returns its
 * zero-copy `SnapshotView` synchronously, the Node build returns a `Promise<JsSnapshot>`. `Async`
 * is `true` for the Node build, whose state reads ({@link readState}, {@link readRecords}) return
 * promises, copied on its engine thread at the next tick boundary.
 */
export interface Simulation<Snapshot, Async extends boolean> {
  /** One deterministic step. */
  tick(): void;
  /** Blocking run-to-completion; requires a finite `ticks` or `ms` bound. */
  run(config?: RunConfig | null): void;
  /** Cooperative run that resolves when the bound is reached or `stop()` is called. */
  runAsync(config?: RunConfig | null): Promise<void>;
  /** Interrupt a `runAsync` run at the next batch boundary. */
  stop(): void;
  /** Typed run status. */
  getStatus(): SimStatus;
  linkCount(): number;
  componentCount(): number;
  /** Powered value of a single link. */
  link(id: number): boolean;
  /** Coherent snapshot of link state; `delta` opts into delta snapshots. */
  snapshot(delta: boolean, threshold: number): Snapshot;
  /** One byte (`0`/`1`) per output pin, component-major in submission order. */
  getOutputs(): Uint8Array;
  /**
   * Queue frames on one or more inputs. The request is validated as a whole (nothing is queued on
   * any error) and lands at one tick boundary; a frame for an idle input is on its pins and wires
   * after the next tick. At most 65 536 event frames may be pending per input.
   */
  submitInput(writes: InputWrite[]): void;
  /**
   * Fill `out[i]` with the number of input `i`'s event frames not yet applied, where input `i` is
   * the `i`-th `UserInput` in the descriptor's `components`. `out` needs at least one entry per
   * input; allocate it once and reuse it, so polling allocates nothing.
   */
  inputProgress(out: Uint32Array): void;
  /**
   * Fill `out[i]` with the state version of component `compIds[i]`: a number that changes
   * (wrapping) whenever the component's state does, 0 for a type without state. A RAM's moves on
   * every write that changes a stored bit, a recorder's is its record count. A new simulation
   * (including one rebuilt from the same board) starts every version at 0. Poll it to re-read only
   * what moved; allocate both arrays once. On Node it is exact once `tick()` / `run()` returns or
   * `runAsync()` resolves, and at most one batch behind during a run. Throws if a component id is
   * out of range or `out` is shorter than `compIds`.
   */
  stateVersions(compIds: Uint32Array, out: Uint32Array): void;
  /** Size of the component's byte state (a RAM's memory); 0 for a type without byte state. */
  stateLength(compId: number): number;
  /**
   * A copy of `length` bytes of the component's byte state from byte `offset`. A RAM's memory is its
   * words bit-packed LSB-first, as a ROM's `ops`: the word at address `a` in bits `[a·w, a·w + w)`.
   * Throws (on Node: rejects) if the component id is out of range or `offset + length` exceeds
   * {@link stateLength}, which is any non-empty range on a type without byte state.
   */
  readState(compId: number, offset: number, length: number): Out<Async, Uint8Array>;
  /**
   * Overwrite the component's byte state from byte `offset` (RAM only). Validated synchronously and
   * applied at the next tick boundary, no later than an input request submitted after it. A RAM's
   * outputs keep their value until a rising clock reads a word.
   */
  writeState(compId: number, offset: number, bytes: Uint8Array): void;
  /**
   * A recorder's records from index `since` on, oldest first, at most `max` of them (default: all it
   * still holds), read together with its count. Page through by passing `first + values.length` as
   * the next `since`; a `since` past the count is an error.
   */
  readRecords(compId: number, since: number, max?: number): Out<Async, Records>;
  /** Free the simulation; consumes / invalidates the handle. */
  destroy(): void;
}

/**
 * The static side: the constructor plus the two factory methods. `binary` is typed `Uint8Array`
 * (the wasm build accepts a `Uint8Array`, the Node build a `Buffer`, which is a `Uint8Array`).
 */
export interface SimulationConstructor<Snapshot, Async extends boolean> {
  new (descriptor: BoardDescriptor): Simulation<Snapshot, Async>;
  fromJson(json: string): Simulation<Snapshot, Async>;
  fromBinary(binary: Uint8Array): Simulation<Snapshot, Async>;
}
