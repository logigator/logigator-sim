//! `submitInput` requests, parsed by hand from the JS `InputWrite[]` so a bad `state` entry or
//! `hold` is reported with its input, frame and pin (serde would read a hole or `undefined` as
//! `null`, and reject a `0` without saying where).

use js_sys::Array;
use sim_core::{InputRequest, SimError};
use wasm_bindgen::{JsError, JsValue};

use crate::js::{as_u32, get, is_plain_object};
use crate::js_err;

/// An optional boolean property: `undefined` reads as `false`.
fn opt_bool(obj: &JsValue, name: &str, at: &str) -> Result<bool, JsError> {
    let v = get(obj, name)?;
    if v.is_undefined() {
        return Ok(false);
    }
    v.as_bool()
        .ok_or_else(|| JsError::new(&format!("{at}.{name} must be a boolean")))
}

/// An array property.
fn array(obj: &JsValue, name: &str, at: &str) -> Result<Array, JsError> {
    let v = get(obj, name)?;
    if !Array::is_array(&v) {
        return Err(JsError::new(&format!("{at}.{name} must be an array")));
    }
    Ok(Array::from(&v))
}

/// Element `i` of `arr`, which must be a (non-array) object.
fn object(arr: &Array, i: u32, at: &str) -> Result<JsValue, JsError> {
    let v = arr.get(i);
    if !is_plain_object(&v) {
        return Err(JsError::new(&format!("{at} must be an object")));
    }
    Ok(v)
}

/// Parse a JS `InputWrite[]`. Rejects entries the core types cannot express: a `state` entry that
/// is not `true`, `false` or `null` ([`SimError::InputState`]) and a `hold` that is not an integer
/// in `1..=2^32-1` ([`SimError::InputHold`]). Everything else is left to
/// [`sim_core::Simulation::submit_input`]'s validation.
pub fn parse(writes: &JsValue) -> Result<InputRequest, JsError> {
    if !Array::is_array(writes) {
        return Err(JsError::new("submitInput: writes must be an array"));
    }
    let writes = Array::from(writes);
    let mut req = InputRequest::default();
    for wi in 0..writes.length() {
        let at = format!("writes[{wi}]");
        let w = object(&writes, wi, &at)?;
        let comp = as_u32(&get(&w, "comp")?)
            .ok_or_else(|| JsError::new(&format!("{at}.comp must be a component id")))?;
        let replace = opt_bool(&w, "replace", &at)?;
        let frames = array(&w, "frames", &at)?;
        for fi in 0..frames.length() {
            let f_at = format!("{at}.frames[{fi}]");
            let f = object(&frames, fi, &f_at)?;
            let frame = fi as usize;
            let state = array(&f, "state", &f_at)?;
            for pin in 0..state.length() {
                // A hole reads as `undefined`, which is refused like any other non-boolean.
                let v = state.get(pin);
                req.push_state(match v.as_bool() {
                    Some(b) => Some(b),
                    None if v.is_null() => None,
                    None => {
                        return Err(js_err(SimError::InputState {
                            comp,
                            frame,
                            pin: pin as usize,
                        }));
                    }
                });
            }
            let hold = get(&f, "hold")?;
            let hold = if hold.is_undefined() {
                Some(1)
            } else {
                as_u32(&hold).filter(|&h| h > 0)
            }
            .ok_or_else(|| js_err(SimError::InputHold { comp, frame }))?;
            req.end_frame(hold, opt_bool(&f, "coalesce", &f_at)?);
        }
        req.end_write(comp, replace);
    }
    Ok(req)
}
