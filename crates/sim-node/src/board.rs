//! The `BoardDescriptor` and `RunConfig` objects, read from JS with the WASM binding's rules and
//! messages. The board is only copied into the core descriptor here — every field is validated by
//! `Board::compile`, as for every other way a board comes in — so all this checks is the shape:
//! objects, arrays, and integers that fit a `u32` where the descriptor holds one, never coerced. A
//! field that is `undefined` is absent; unknown fields are ignored.

use std::ffi::CStr;

use napi::bindgen_prelude::{Array, Object, Uint8Array, Unknown};
use napi::{JsValue, ValueType};
use sim_core::{BoardDescriptor, ClkMode, ComponentDescriptor, RamRead, RunConfig};

use crate::js::{as_u32, element, field, is_plain_object, object, opt_bool, prop};
use crate::worker::core_err;

/// An optional `u32` property.
fn opt_u32(c: &Object<'_>, name: &CStr, at: &str) -> napi::Result<Option<u32>> {
    let Some(v) = prop(c, name)? else {
        return Ok(None);
    };
    as_u32(&v)
        .map(Some)
        .ok_or_else(|| core_err(format!("{at}.{} must be a u32", field(name))))
}

/// An optional list of `u32`s; `undefined` reads as empty.
fn list(c: &Object<'_>, name: &CStr, at: &str) -> napi::Result<Vec<u32>> {
    let Some(v) = prop(c, name)? else {
        return Ok(Vec::new());
    };
    let bad = || core_err(format!("{at}.{} must be an array of u32", field(name)));
    if !v.is_array()? {
        return Err(bad());
    }
    let arr = unsafe { v.cast::<Array<'_>>() }?;
    (0..arr.len())
        .map(|i| as_u32(&element(&arr, i)?).ok_or_else(bad))
        .collect()
}

/// An optional string property that must be one of `options`; the index of the one it is.
fn opt_choice(
    c: &Object<'_>,
    name: &CStr,
    at: &str,
    options: [&str; 2],
) -> napi::Result<Option<usize>> {
    let Some(v) = prop(c, name)? else {
        return Ok(None);
    };
    let s = match v.get_type()? {
        ValueType::String => Some(unsafe { v.cast::<String>() }?),
        _ => None,
    };
    s.and_then(|s| options.iter().position(|&o| o == s))
        .map(Some)
        .ok_or_else(|| {
            let [a, b] = options;
            core_err(format!(r#"{at}.{} must be "{a}" or "{b}""#, field(name)))
        })
}

/// A ROM's `data`: a `Uint8Array` or an array of bytes.
fn opt_data(c: &Object<'_>, at: &str) -> napi::Result<Option<Vec<u8>>> {
    let Some(v) = prop(c, c"data")? else {
        return Ok(None);
    };
    let bad = || {
        core_err(format!(
            "{at}.data must be a Uint8Array or an array of bytes"
        ))
    };
    if v.is_typedarray()? {
        return unsafe { v.cast::<Uint8Array>() }
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|_| bad());
    }
    if !v.is_array()? {
        return Err(bad());
    }
    let arr = unsafe { v.cast::<Array<'_>>() }?;
    (0..arr.len())
        .map(|i| {
            let x = as_u32(&element(&arr, i)?);
            x.and_then(|x| u8::try_from(x).ok()).ok_or_else(bad)
        })
        .collect::<napi::Result<_>>()
        .map(Some)
}

/// Read a JS `BoardDescriptor` (`{ components }`). Fields are checked in order, component by
/// component, so the error names the first malformed one.
pub fn descriptor(v: &Unknown<'_>) -> napi::Result<BoardDescriptor> {
    if !is_plain_object(v)? {
        return Err(core_err("the board descriptor must be an object"));
    }
    let desc = unsafe { v.cast::<Object<'_>>() }?;
    let comps = match prop(&desc, c"components")? {
        Some(c) if c.is_array()? => unsafe { c.cast::<Array<'_>>() }?,
        _ => return Err(core_err("components must be an array")),
    };
    let mut components = Vec::with_capacity(comps.len() as usize);
    for i in 0..comps.len() {
        let at = format!("components[{i}]");
        let c = object(&comps, i, &at)?;
        let ty = prop(&c, c"type")?
            .as_ref()
            .and_then(as_u32)
            .ok_or_else(|| core_err(format!("{at}.type must be a u32")))?;
        components.push(ComponentDescriptor {
            ty,
            inputs: list(&c, c"inputs", &at)?,
            outputs: list(&c, c"outputs", &at)?,
            neg_inputs: list(&c, c"negInputs", &at)?,
            neg_outputs: list(&c, c"negOutputs", &at)?,
            period: opt_u32(&c, c"period", &at)?,
            mode: opt_choice(&c, c"mode", &at, ["pulse", "square"])?
                .map(|m| [ClkMode::Pulse, ClkMode::Square][m]),
            read: opt_choice(&c, c"read", &at, ["async", "clocked"])?
                .map(|r| [RamRead::Async, RamRead::Clocked][r]),
            data: opt_data(&c, &at)?,
            capacity: opt_u32(&c, c"capacity", &at)?,
            record_ticks: opt_bool(&c, c"recordTicks", &at)?,
            data_bus: opt_u32(&c, c"dataBus", &at)?,
        });
    }
    Ok(BoardDescriptor { components })
}

/// Read a JS `RunConfig` (`{ ticks?, ms? }`); a missing config (`undefined` or `null`) is empty.
/// Lowered with the clamping shared with the WASM binding.
pub fn run_config(v: Option<Unknown<'_>>) -> napi::Result<RunConfig> {
    let Some(v) = v else {
        return Ok(RunConfig::default());
    };
    if !is_plain_object(&v)? {
        return Err(core_err("the run config must be an object"));
    }
    let cfg = unsafe { v.cast::<Object<'_>>() }?;
    let bound = |name: &CStr| -> napi::Result<Option<f64>> {
        let Some(x) = prop(&cfg, name)? else {
            return Ok(None);
        };
        match x.get_type()? {
            ValueType::Number => unsafe { x.cast::<f64>() }.map(Some),
            _ => Err(core_err(format!("{} must be a number", field(name)))),
        }
    };
    Ok(RunConfig::from_float_bounds(
        bound(c"ticks")?,
        bound(c"ms")?,
    ))
}
