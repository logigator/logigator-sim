//! The `BoardDescriptor` and `RunConfig` objects, read from JS. The board is only copied into the
//! core descriptor here — every field is validated by `Board::compile`, as for every other way a
//! board comes in — so all this checks is the shape: objects, arrays, and integers that fit a
//! `u32` where the descriptor holds one, never coerced.

use js_sys::{Array, Uint8Array, Uint32Array};
use sim_core::{BoardDescriptor, ClkMode, ComponentDescriptor, RamRead, RunConfig};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::js::{get, is_plain_object};

/// Walks the descriptor in JS and packs it into one `Uint32Array` plus one `Uint8Array` of ROM
/// data, so a board crosses into wasm in one call and two copies rather than a boundary call per
/// field and element, which dominates the construction of a large board. Layout: `componentCount`,
/// then per component `type, #inputs, #outputs, #negInputs, #negOutputs, paramMask` followed by
/// those lists in that order and one slot per parameter present, in mask-bit order: `period`,
/// `mode` (0 pulse, 1 square), `read` (0 async, 1 clocked), `data` (its length; the bytes follow
/// the previous component's in the byte array), `capacity`, `recordTicks` (0 or 1), `dataBus`.
/// A parameter that is `null` or `undefined` is absent. Throws a `TypeError` naming the first
/// malformed field.
#[wasm_bindgen(inline_js = r#"
const LISTS = ["inputs", "outputs", "negInputs", "negOutputs"];
const PARAMS = ["period", "mode", "read", "data", "capacity", "recordTicks", "dataBus"];
const MODES = ["pulse", "square"];
const READS = ["async", "clocked"];
const isU32 = (x) => typeof x === "number" && Number.isInteger(x) && x >= 0 && x <= 0xffffffff;
const isByte = (x) => typeof x === "number" && Number.isInteger(x) && x >= 0 && x <= 0xff;
const isObject = (x) => typeof x === "object" && x !== null && !Array.isArray(x);
export function pack_board(desc) {
  const fail = (msg) => { throw new TypeError(msg); };
  if (!isObject(desc)) fail("the board descriptor must be an object");
  const comps = desc.components;
  if (!Array.isArray(comps)) fail("components must be an array");
  let size = 1;
  let bytes = 0;
  for (let i = 0; i < comps.length; i++) {
    const c = comps[i];
    if (!isObject(c)) fail(`components[${i}] must be an object`);
    if (!isU32(c.type)) fail(`components[${i}].type must be a u32`);
    size += 2 + LISTS.length;
    for (const name of LISTS) {
      const list = c[name];
      if (list == null) continue;
      if (!Array.isArray(list)) fail(`components[${i}].${name} must be an array of u32`);
      size += list.length;
    }
    for (const name of PARAMS) if (c[name] != null) size += 1;
    const data = c.data;
    if (data != null) {
      if (!(data instanceof Uint8Array) && !Array.isArray(data)) {
        fail(`components[${i}].data must be a Uint8Array or an array of bytes`);
      }
      bytes += data.length;
    }
  }
  const out = new Uint32Array(size);
  const blob = new Uint8Array(bytes);
  out[0] = comps.length;
  let p = 1;
  let b = 0;
  for (let i = 0; i < comps.length; i++) {
    const c = comps[i];
    out[p++] = c.type;
    const counts = p;
    p += LISTS.length;
    const maskAt = p++;
    for (let k = 0; k < LISTS.length; k++) {
      const list = c[LISTS[k]] ?? [];
      out[counts + k] = list.length;
      for (let j = 0; j < list.length; j++) {
        const x = list[j];
        if (!isU32(x)) fail(`components[${i}].${LISTS[k]} must be an array of u32`);
        out[p++] = x;
      }
    }
    let mask = 0;
    for (let k = 0; k < PARAMS.length; k++) {
      const name = PARAMS[k];
      const v = c[name];
      if (v == null) continue;
      mask |= 1 << k;
      const at = `components[${i}].${name}`;
      let slot;
      switch (name) {
        case "mode":
          slot = MODES.indexOf(v);
          if (slot < 0) fail(`${at} must be "pulse" or "square"`);
          break;
        case "read":
          slot = READS.indexOf(v);
          if (slot < 0) fail(`${at} must be "async" or "clocked"`);
          break;
        case "recordTicks":
          if (typeof v !== "boolean") fail(`${at} must be a boolean`);
          slot = v ? 1 : 0;
          break;
        case "data":
          if (v instanceof Uint8Array) {
            blob.set(v, b);
          } else {
            for (let j = 0; j < v.length; j++) {
              if (!isByte(v[j])) fail(`${at} must be a Uint8Array or an array of bytes`);
              blob[b + j] = v[j];
            }
          }
          b += v.length;
          slot = v.length;
          break;
        default:
          if (!isU32(v)) fail(`${at} must be a u32`);
          slot = v;
      }
      out[p++] = slot;
    }
    out[maskAt] = mask;
  }
  return [out, blob];
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn pack_board(desc: &JsValue) -> Result<Array, JsValue>;
}

/// Read a JS `BoardDescriptor` (`{ components }`).
pub fn descriptor(v: &JsValue) -> Result<BoardDescriptor, JsError> {
    let packed =
        pack_board(v).map_err(|e| JsError::new(&String::from(js_sys::Error::from(e).message())))?;
    let words = packed.get(0).unchecked_into::<Uint32Array>().to_vec();
    let blob = packed.get(1).unchecked_into::<Uint8Array>().to_vec();
    let mut rest = &words[..];
    let mut take = |n: u32| {
        let (head, tail) = rest.split_at(n as usize);
        rest = tail;
        head
    };
    let mut bytes = &blob[..];
    let [count] = take(1) else {
        unreachable!("pack_board's header")
    };
    let mut components = Vec::with_capacity(*count as usize);
    for _ in 0..*count {
        let &[ty, ins, outs, neg_ins, neg_outs, mask] = take(6) else {
            unreachable!("pack_board's component header")
        };
        let inputs = take(ins).to_vec();
        let outputs = take(outs).to_vec();
        let neg_inputs = take(neg_ins).to_vec();
        let neg_outputs = take(neg_outs).to_vec();
        let mut param = |bit: u32| (mask >> bit & 1 != 0).then(|| take(1)[0]);
        let period = param(0);
        let mode = param(1).map(|m| [ClkMode::Pulse, ClkMode::Square][m as usize]);
        let read = param(2).map(|r| [RamRead::Async, RamRead::Clocked][r as usize]);
        let data = param(3).map(|n| {
            let (head, tail) = bytes.split_at(n as usize);
            bytes = tail;
            head.to_vec()
        });
        let capacity = param(4);
        let record_ticks = param(5).map(|t| t != 0);
        let data_bus = param(6);
        components.push(ComponentDescriptor {
            ty,
            inputs,
            outputs,
            neg_inputs,
            neg_outputs,
            period,
            mode,
            read,
            data,
            capacity,
            record_ticks,
            data_bus,
        });
    }
    Ok(BoardDescriptor { components })
}

/// Read a JS `RunConfig` (`{ ticks?, ms? }`); `undefined`/`null` is an empty config. Lowered with
/// the clamping shared with the Node binding.
pub fn run_config(v: &JsValue) -> Result<RunConfig, JsError> {
    if v.is_undefined() || v.is_null() {
        return Ok(RunConfig::default());
    }
    if !is_plain_object(v) {
        return Err(JsError::new("the run config must be an object"));
    }
    let bound = |name: &str| -> Result<Option<f64>, JsError> {
        let x = get(v, name)?;
        if x.is_undefined() || x.is_null() {
            return Ok(None);
        }
        x.as_f64()
            .map(Some)
            .ok_or_else(|| JsError::new(&format!("{name} must be a number")))
    };
    Ok(RunConfig::from_float_bounds(bound("ticks")?, bound("ms")?))
}
