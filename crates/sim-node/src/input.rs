//! `submitInput` requests: parsed by hand from the JS `InputWrite[]` (napi's own conversion would
//! read a hole or `undefined` as `null` and reject a `0` without saying which entry), validated on
//! the JS thread, then shipped to the worker as one owned [`InputRequest`].

use napi::ValueType;
use napi::bindgen_prelude::Array;
use sim_core::{InputFrame, InputWrite, SimError};

use crate::js::{array, as_u32, element, object, opt_bool, prop, type_err};
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
            let comp = prop(&w, c"comp")?
                .as_ref()
                .and_then(as_u32)
                .ok_or_else(|| type_err(format!("{at}.comp must be a component id")))?;
            let replace = opt_bool(&w, c"replace", &at)?.unwrap_or(false);
            let frames = array(&w, c"frames", &at)?;
            for fi in 0..frames.len() {
                let f_at = format!("{at}.frames[{fi}]");
                let f = object(&frames, fi, &f_at)?;
                let frame = fi as usize;
                let state = array(&f, c"state", &f_at)?;
                for pin in 0..state.len() {
                    let v = element(&state, pin)?;
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
                let hold = match prop(&f, c"hold")? {
                    None => Some(1),
                    Some(h) => as_u32(&h).filter(|&h| h > 0),
                }
                .ok_or_else(|| core_err(SimError::InputHold { comp, frame }))?;
                let coalesce = opt_bool(&f, c"coalesce", &f_at)?.unwrap_or(false);
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
