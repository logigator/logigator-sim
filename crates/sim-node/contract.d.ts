/**
 * The backend-agnostic public API shared by `@logigator/sim/node` (the napi addon) and
 * `@logigator/sim/wasm` (the browser build). This file is hand-written and is the single source of
 * truth for the surface both bindings must expose; `contract.conformance.ts` statically asserts that
 * each binding's generated types satisfy it, so any drift fails `npm run typecheck`.
 *
 * The two bindings differ only where their execution models force it — the wasm build is
 * single-threaded and reads state zero-copy out of linear memory (a synchronous `snapshot`), while
 * the Node build runs the engine on a worker thread and copies state out at a tick boundary (an
 * async `snapshot`). That single axis is the {@link Simulation} type parameter; everything else is
 * identical.
 *
 * Enum-valued fields are typed `number` here rather than as a union or enum: a numeric union widens
 * to `number` and a TypeScript `enum` is nominal, so `number` is the one type both bindings'
 * `getStatus().state` / `triggerInput` shapes are assignable to. The canonical {@link SimState} and
 * {@link InputEvent} value sets are exported below for consumers who want to name them.
 */

/** Lifecycle of a simulation, as it crosses to JS. The discriminants are a frozen wire contract. */
export type SimState =
  | 0 // Uninitialized
  | 1 // Stopped
  | 2 // Running
  | 3; // Stopping

/** How a `triggerInput` payload is applied: `0` = Cont (set-and-hold), `1` = Pulse (one tick). */
export type InputEvent =
  | 0 // Cont
  | 1; // Pulse

/**
 * One component in a {@link BoardDescriptor}
 * (`{ type, inputs, outputs, ops?, negatedInputs?, negatedOutputs? }`). `negatedInputs` /
 * `negatedOutputs` list the *pin indices* (into `inputs` / `outputs`) that read or drive the
 * inverted value, with no added delay.
 */
export interface ComponentDescriptor {
  type: number;
  inputs: number[];
  outputs: number[];
  ops?: number[];
  negatedInputs?: number[];
  negatedOutputs?: number[];
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
 * zero-copy `SnapshotView` synchronously, the Node build returns a `Promise<JsSnapshot>`.
 */
export interface Simulation<Snapshot> {
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
  /** Apply external input to a `UserInput` at a tick boundary (`event` is an {@link InputEvent}). */
  triggerInput(compId: number, event: number, state: boolean[]): void;
  /** Free the simulation; consumes / invalidates the handle. */
  destroy(): void;
}

/**
 * The static side: the constructor plus the two factory methods. `binary` is typed `Uint8Array`
 * (the wasm build accepts a `Uint8Array`, the Node build a `Buffer`, which is a `Uint8Array`).
 */
export interface SimulationConstructor<Snapshot> {
  new (descriptor: BoardDescriptor): Simulation<Snapshot>;
  fromJson(json: string): Simulation<Snapshot>;
  fromBinary(binary: Uint8Array): Simulation<Snapshot>;
}
