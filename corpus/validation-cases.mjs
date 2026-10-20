// Malformed arguments and the error each binding must refuse them with, shared by the Node and
// WASM suites so the two bindings can't drift apart. A case's `error` is the exact message.

const NOT = { type: 1, inputs: [0], outputs: [1] };

/** `new Simulation(board)` that must throw `error`. */
export const BOARDS = [
  { name: "not an object", board: 5, error: "the board descriptor must be an object" },
  { name: "an array", board: [], error: "the board descriptor must be an object" },
  { name: "no components", board: {}, error: "components must be an array" },
  { name: "component not an object", board: { components: [1] }, error: "components[0] must be an object" },
  { name: "fractional type", board: { components: [{ ...NOT, type: 1.5 }] }, error: "components[0].type must be a u32" },
  { name: "negative type", board: { components: [{ ...NOT, type: -1 }] }, error: "components[0].type must be a u32" },
  { name: "string type", board: { components: [{ ...NOT, type: "1" }] }, error: "components[0].type must be a u32" },
  { name: "null list", board: { components: [{ ...NOT, inputs: null }] }, error: "components[0].inputs must be an array of u32" },
  { name: "fractional link", board: { components: [{ ...NOT, inputs: [0.5] }] }, error: "components[0].inputs must be an array of u32" },
  { name: "negative link", board: { components: [{ ...NOT, inputs: [-1] }] }, error: "components[0].inputs must be an array of u32" },
  { name: "string link", board: { components: [{ ...NOT, inputs: ["0"] }] }, error: "components[0].inputs must be an array of u32" },
  { name: "list not an array", board: { components: [{ ...NOT, inputs: 0 }] }, error: "components[0].inputs must be an array of u32" },
  { name: "hole in a list", board: { components: [{ ...NOT, outputs: [, 1] }] }, error: "components[0].outputs must be an array of u32" },
  { name: "fractional period", board: { components: [{ type: 6, inputs: [0], outputs: [1], period: 2.7 }] }, error: "components[0].period must be a u32" },
  { name: "null period", board: { components: [{ type: 6, inputs: [0], outputs: [1], period: null }] }, error: "components[0].period must be a u32" },
  { name: "unknown mode", board: { components: [{ type: 6, inputs: [0], outputs: [1], period: 1, mode: "x" }] }, error: 'components[0].mode must be "pulse" or "square"' },
  { name: "null mode", board: { components: [{ type: 6, inputs: [0], outputs: [1], period: 1, mode: null }] }, error: 'components[0].mode must be "pulse" or "square"' },
  { name: "unknown read", board: { components: [{ ...NOT, read: "x" }] }, error: 'components[0].read must be "async" or "clocked"' },
  { name: "numeric recordTicks", board: { components: [{ ...NOT, recordTicks: 1 }] }, error: "components[0].recordTicks must be a boolean" },
  { name: "fractional data byte", board: { components: [{ ...NOT, data: [1.5] }] }, error: "components[0].data must be a Uint8Array or an array of bytes" },
  { name: "data byte too large", board: { components: [{ ...NOT, data: [256] }] }, error: "components[0].data must be a Uint8Array or an array of bytes" },
  { name: "string data", board: { components: [{ ...NOT, data: "x" }] }, error: "components[0].data must be a Uint8Array or an array of bytes" },
  { name: "Uint16Array data", board: { components: [{ ...NOT, data: new Uint16Array(1) }] }, error: "components[0].data must be a Uint8Array or an array of bytes" },
  { name: "null capacity", board: { components: [{ ...NOT, capacity: null }] }, error: "components[0].capacity must be a u32" },
  {
    name: "the first malformed field wins",
    board: { components: [{ ...NOT, inputs: [0.5] }, { ...NOT, type: 1.5 }] },
    error: "components[0].inputs must be an array of u32",
  },
];

/** A board with an unknown field, which both bindings ignore. */
export const UNKNOWN_FIELD = { components: [{ ...NOT, negInput: [0], extra: "x" }] };

/** A one-NOT board for the run and link cases. */
export const BOARD = { components: [NOT] };

/** `runAsync(config)` / `run(config)` that must throw `error` synchronously. */
export const RUN_CONFIGS = [
  { name: "a number", config: 5, error: "the run config must be an object" },
  { name: "an array", config: [], error: "the run config must be an object" },
  { name: "string ticks", config: { ticks: "5" }, error: "ticks must be a number" },
  { name: "null ticks", config: { ticks: null }, error: "ticks must be a number" },
  { name: "string ms", config: { ms: "5" }, error: "ms must be a number" },
];

/** `link(id)` past the last link of `BOARD`. */
export const LINK_OUT_OF_RANGE = { id: 99, error: "link id 99 out of range (link_count = 2)" };
