/**
 * Static proof that both bindings satisfy the shared {@link SimulationContract}. Nothing here runs —
 * `npm run typecheck` (`tsc --noEmit`) evaluates the assignments below at compile time, so a drift
 * between either binding's generated types and `contract.d.ts` is a build error. The bindings'
 * `.d.ts` files are generated, so this is checked in CI after `napi build` and `wasm-pack build`.
 */

import { Simulation as NodeSimulation } from './index.js';
import type { JsSnapshot, JsStatus, BoardDescriptor as NodeBoard, RunConfig as NodeRunConfig } from './index.js';
import { Simulation as WasmSimulation } from '../sim-wasm/pkg/sim_wasm.js';
import type {
  SnapshotView,
  SimStatus as WasmStatus,
  BoardDescriptor as WasmBoard,
  RunConfig as WasmRunConfig,
} from '../sim-wasm/pkg/sim_wasm.js';
import type {
  Simulation as SimulationContract,
  SimulationConstructor,
  SimStatus as ContractStatus,
  BoardDescriptor as ContractBoard,
  RunConfig as ContractRunConfig,
} from './contract.js';

// The Node binding implements the async-snapshot variant; the wasm binding the sync one.
const _nodeInstance: SimulationContract<Promise<JsSnapshot>> = {} as InstanceType<typeof NodeSimulation>;
const _wasmInstance: SimulationContract<SnapshotView> = {} as InstanceType<typeof WasmSimulation>;

// The static side: constructor + `fromJson` / `fromBinary`.
const _nodeCtor: SimulationConstructor<Promise<JsSnapshot>> = NodeSimulation;
const _wasmCtor: SimulationConstructor<SnapshotView> = WasmSimulation;

// The shared data shapes crossing the boundary.
const _nodeStatus: ContractStatus = {} as JsStatus;
const _wasmStatus: ContractStatus = {} as WasmStatus;
const _nodeBoard: ContractBoard = {} as NodeBoard;
const _wasmBoard: ContractBoard = {} as WasmBoard;
const _nodeRunConfig: ContractRunConfig = {} as NodeRunConfig;
const _wasmRunConfig: ContractRunConfig = {} as WasmRunConfig;

// Reference every binding so unused-import / unused-local checks keep this file honest.
export type _Conformance = [
  typeof _nodeInstance,
  typeof _wasmInstance,
  typeof _nodeCtor,
  typeof _wasmCtor,
  typeof _nodeStatus,
  typeof _wasmStatus,
  typeof _nodeBoard,
  typeof _wasmBoard,
  typeof _nodeRunConfig,
  typeof _wasmRunConfig,
];
