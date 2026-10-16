//! The `BoardDescriptor` and `RunConfig` objects, read from JS. The board is only copied into the
//! core descriptor here — every field is validated by `Board::compile`, as for every other way a
//! board comes in — so all this checks is the shape: objects, arrays, and integers that fit a
//! `u32` where the descriptor holds one, never coerced.

use js_sys::Uint32Array;
use sim_core::{BoardDescriptor, ComponentDescriptor, RunConfig};
use wasm_bindgen::prelude::*;

use crate::js::{get, is_plain_object};

/// Walks the descriptor in JS and packs it into one `Uint32Array`, so a board crosses into wasm in
/// one call and one copy rather than a boundary call per field and element, which dominates the
/// construction of a large board. Layout: `links, componentCount`, then per component
/// `type, #inputs, #outputs, #ops, #negInputs, #negOutputs` followed by those lists in that order.
/// Throws a `TypeError` naming the first malformed field.
#[wasm_bindgen(inline_js = r#"
const LISTS = ["inputs", "outputs", "ops", "negInputs", "negOutputs"];
const isU32 = (x) => typeof x === "number" && Number.isInteger(x) && x >= 0 && x <= 0xffffffff;
const isObject = (x) => typeof x === "object" && x !== null && !Array.isArray(x);
export function pack_board(desc) {
  const fail = (msg) => { throw new TypeError(msg); };
  if (!isObject(desc)) fail("the board descriptor must be an object");
  if (!isU32(desc.links)) fail("links must be a u32");
  const comps = desc.components;
  if (!Array.isArray(comps)) fail("components must be an array");
  let size = 2;
  for (let i = 0; i < comps.length; i++) {
    const c = comps[i];
    if (!isObject(c)) fail(`components[${i}] must be an object`);
    if (!isU32(c.type)) fail(`components[${i}].type must be a u32`);
    size += 1 + LISTS.length;
    for (let k = 0; k < LISTS.length; k++) {
      const list = c[LISTS[k]];
      if (list == null) {
        if (k < 2) fail(`components[${i}].${LISTS[k]} is missing`);
      } else if (!Array.isArray(list)) {
        fail(`components[${i}].${LISTS[k]} must be an array of u32`);
      } else {
        size += list.length;
      }
    }
  }
  const out = new Uint32Array(size);
  out[0] = desc.links;
  out[1] = comps.length;
  let p = 2;
  for (let i = 0; i < comps.length; i++) {
    const c = comps[i];
    out[p++] = c.type;
    const counts = p;
    p += LISTS.length;
    for (let k = 0; k < LISTS.length; k++) {
      const list = c[LISTS[k]] ?? [];
      out[counts + k] = list.length;
      for (let j = 0; j < list.length; j++) {
        const x = list[j];
        if (!isU32(x)) fail(`components[${i}].${LISTS[k]} must be an array of u32`);
        out[p++] = x;
      }
    }
  }
  return out;
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn pack_board(desc: &JsValue) -> Result<Uint32Array, JsValue>;
}

/// Read a JS `BoardDescriptor` (`{ links, components }`).
pub fn descriptor(v: &JsValue) -> Result<BoardDescriptor, JsError> {
    let packed = pack_board(v)
        .map_err(|e| JsError::new(&String::from(js_sys::Error::from(e).message())))?
        .to_vec();
    let mut rest = &packed[..];
    let mut take = |n: u32| {
        let (head, tail) = rest.split_at(n as usize);
        rest = tail;
        head
    };
    let [link_count, count] = take(2) else {
        unreachable!("pack_board's header")
    };
    let mut components = Vec::with_capacity(*count as usize);
    for _ in 0..*count {
        let &[ty, ins, outs, ops, neg_ins, neg_outs] = take(6) else {
            unreachable!("pack_board's component header")
        };
        components.push(ComponentDescriptor {
            ty,
            inputs: take(ins).to_vec(),
            outputs: take(outs).to_vec(),
            ops: take(ops).to_vec(),
            neg_inputs: take(neg_ins).to_vec(),
            neg_outputs: take(neg_outs).to_vec(),
        });
    }
    Ok(BoardDescriptor {
        link_count: *link_count,
        components,
    })
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
