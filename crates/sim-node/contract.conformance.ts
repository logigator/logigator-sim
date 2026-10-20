/**
 * Static proof that both bindings satisfy the shared {@link SimulationContract}. Nothing here runs —
 * `npm run typecheck` (`tsc --noEmit`) evaluates the assignments below at compile time, so a drift
 * between either binding's generated types and `contract.d.ts` is a build error. The bindings'
 * `.d.ts` files are generated, so this is checked in CI after `napi build` and `wasm-pack build`.
 */

import { Simulation as NodeSimulation } from './index.js';
import type {
  JsSnapshot,
  JsStatus,
  BoardDescriptor as NodeBoard,
  RunConfig as NodeRunConfig,
  InputWrite as NodeInputWrite,
  Records as NodeRecords,
} from './index.js';
import { Simulation as WasmSimulation } from '../sim-wasm/pkg/sim_wasm.js';
import type {
  SnapshotView,
  SimStatus as WasmStatus,
  BoardDescriptor as WasmBoard,
  RunConfig as WasmRunConfig,
  InputWrite as WasmInputWrite,
  Records as WasmRecords,
} from '../sim-wasm/pkg/sim_wasm.js';
import type {
  Simulation as SimulationContract,
  SimulationConstructor,
  SimStatus as ContractStatus,
  BoardDescriptor as ContractBoard,
  RunConfig as ContractRunConfig,
  InputWrite as ContractInputWrite,
  Records as ContractRecords,
} from './contract.js';

// The Node binding implements the async variant; the wasm binding the sync one.
const _nodeInstance: SimulationContract<Promise<JsSnapshot>, true> = {} as InstanceType<typeof NodeSimulation>;
const _wasmInstance: SimulationContract<SnapshotView, false> = {} as InstanceType<typeof WasmSimulation>;

// The static side: constructor + `fromBinary`.
const _nodeCtor: SimulationConstructor<Promise<JsSnapshot>, true> = NodeSimulation;
const _wasmCtor: SimulationConstructor<SnapshotView, false> = WasmSimulation;

// The shared data shapes the bindings return.
const _nodeStatus: ContractStatus = {} as JsStatus;
const _wasmStatus: ContractStatus = {} as WasmStatus;
// Arguments flow the other way: each binding must accept every contract-shaped board, run config
// and write. Checked here, not only through the method signatures, which TypeScript compares
// bivariantly.
const _nodeBoard: NodeBoard = {} as ContractBoard;
const _wasmBoard: WasmBoard = {} as ContractBoard;
const _nodeRunConfig: NodeRunConfig = {} as ContractRunConfig;
const _wasmRunConfig: WasmRunConfig = {} as ContractRunConfig;
const _nodeInputWrite: NodeInputWrite = {} as ContractInputWrite;
const _wasmInputWrite: WasmInputWrite = {} as ContractInputWrite;
const _nodeRecords: ContractRecords = {} as NodeRecords;
const _wasmRecords: ContractRecords = {} as WasmRecords;

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
  typeof _nodeInputWrite,
  typeof _wasmInputWrite,
  typeof _nodeRecords,
  typeof _wasmRecords,
];
