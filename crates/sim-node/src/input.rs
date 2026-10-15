//! `submitInput` requests: parsed by hand from the JS `InputWrite[]` (napi's own conversion would
//! read a hole or `undefined` as `null` and reject a `0` without saying which entry), validated on
//! the JS thread, then shipped to the worker as one owned [`InputRequest`].

use napi::bindgen_prelude::{Array, JsObjectValue, Object, Unknown};
use napi::{JsValue, ValueType};
use sim_core::{InputFrame, InputWrite, SimError};

use crate::worker::core_err;

/// One frame, as a range of [`InputRequest::states`].
struct FrameSpec {
    states_end: usize,
    hold: u32,
    coalesce: bool,
}

/// One write, as a range of [`InputRequest::frames`].
struct WriteSpec {
    comp: u32,
    replace: bool,
    frames_end: usize,
}

/// An owned `submitInput` request: every frame's pin entries flattened into one buffer, so a long
/// paste costs three allocations, not one per frame.
pub struct InputRequest {
    states: Vec<Option<bool>>,
    frames: Vec<FrameSpec>,
    writes: Vec<WriteSpec>,
}

fn type_err(msg: String) -> napi::Error {
    napi::Error::new(napi::Status::InvalidArg, msg)
}

/// A non-negative integer that fits a `u32`, or `None`.
fn as_u32(v: f64) -> Option<u32> {
    (v.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&v)).then_some(v as u32)
}

/// An optional boolean property: `undefined` reads as `false`.
fn opt_bool(obj: &Object<'_>, name: &str, at: &str) -> napi::Result<bool> {
    let v: Unknown<'_> = obj.get_named_property(name)?;
    match v.get_type()? {
        ValueType::Undefined => Ok(false),
        ValueType::Boolean => unsafe { v.cast::<bool>() },
        _ => Err(type_err(format!("{at}.{name} must be a boolean"))),
    }
}

/// An array property.
fn array<'env>(obj: &Object<'env>, name: &str, at: &str) -> napi::Result<Array<'env>> {
    let v: Unknown<'env> = obj.get_named_property(name)?;
    if !v.is_array()? {
        return Err(type_err(format!("{at}.{name} must be an array")));
    }
    unsafe { v.cast::<Array<'env>>() }
}

/// Element `i` of `arr` as an object.
fn object<'env>(arr: &Array<'env>, i: u32, at: &str) -> napi::Result<Object<'env>> {
    let v: Unknown<'env> = arr.get(i)?.expect("index below length");
    if v.get_type()? != ValueType::Object || v.is_array()? {
        return Err(type_err(format!("{at} must be an object")));
    }
    unsafe { v.cast::<Object<'env>>() }
}

impl InputRequest {
    /// Parse a JS `InputWrite[]`. Rejects entries the core types cannot express: a `state` entry
    /// that is not `true`, `false` or `null` ([`SimError::InputState`]) and a `hold` that is not
    /// an integer in `1..=2^32-1` ([`SimError::InputHold`]). Everything else is left to
    /// [`sim_core::validate_input_writes`].
    pub fn parse(writes: &Array<'_>) -> napi::Result<Self> {
        let mut req = InputRequest {
            states: Vec::new(),
            frames: Vec::new(),
            writes: Vec::with_capacity(writes.len() as usize),
        };
        for wi in 0..writes.len() {
            let at = format!("writes[{wi}]");
            let w = object(writes, wi, &at)?;
            let comp: Unknown<'_> = w.get_named_property("comp")?;
            let comp = match comp.get_type()? {
                ValueType::Number => as_u32(unsafe { comp.cast::<f64>() }?),
                _ => None,
            }
            .ok_or_else(|| type_err(format!("{at}.comp must be a component id")))?;
            let replace = opt_bool(&w, "replace", &at)?;
            let frames = array(&w, "frames", &at)?;
            for fi in 0..frames.len() {
                let f_at = format!("{at}.frames[{fi}]");
                let f = object(&frames, fi, &f_at)?;
                let frame = fi as usize;
                let state = array(&f, "state", &f_at)?;
                for pin in 0..state.len() {
                    let v: Unknown<'_> = state.get(pin)?.expect("index below length");
                    req.states.push(match v.get_type()? {
                        ValueType::Boolean => Some(unsafe { v.cast::<bool>() }?),
                        ValueType::Null => None,
                        _ => {
                            return Err(core_err(SimError::InputState {
                                comp,
                                frame,
                                pin: pin as usize,
                            }));
                        }
                    });
                }
                let hold: Unknown<'_> = f.get_named_property("hold")?;
                let hold = match hold.get_type()? {
                    ValueType::Undefined => Some(1),
                    ValueType::Number => as_u32(unsafe { hold.cast::<f64>() }?).filter(|&h| h > 0),
                    _ => None,
                }
                .ok_or_else(|| core_err(SimError::InputHold { comp, frame }))?;
                let coalesce = opt_bool(&f, "coalesce", &f_at)?;
                req.frames.push(FrameSpec {
                    states_end: req.states.len(),
                    hold,
                    coalesce,
                });
            }
            req.writes.push(WriteSpec {
                comp,
                replace,
                frames_end: req.frames.len(),
            });
        }
        Ok(req)
    }

    /// Call `f` with the request as core [`InputWrite`]s.
    pub fn with_writes<R>(&self, f: impl FnOnce(&[InputWrite<'_>]) -> R) -> R {
        let mut start = 0;
        let frames: Vec<InputFrame<'_>> = self
            .frames
            .iter()
            .map(|s| {
                let state = &self.states[start..s.states_end];
                start = s.states_end;
                InputFrame {
                    state,
                    hold: s.hold,
                    coalesce: s.coalesce,
                }
            })
            .collect();
        let mut start = 0;
        let writes: Vec<InputWrite<'_>> = self
            .writes
            .iter()
            .map(|s| {
                let frames = &frames[start..s.frames_end];
                start = s.frames_end;
                InputWrite {
                    comp: s.comp,
                    replace: s.replace,
                    frames,
                }
            })
            .collect();
        f(&writes)
    }
}
