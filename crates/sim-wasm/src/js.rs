//! Small helpers for reading and building plain JS values by hand, shared by the request readers.

use js_sys::{Object, Reflect};
use wasm_bindgen::{JsError, JsValue};

/// A non-negative integer that fits a `u32`, or `None`.
pub fn as_u32(v: &JsValue) -> Option<u32> {
    v.as_f64().and_then(sim_core::exact_u32)
}

/// Property `name` of `obj`.
pub fn get(obj: &JsValue, name: &str) -> Result<JsValue, JsError> {
    Reflect::get(obj, &JsValue::from_str(name))
        .map_err(|_| JsError::new(&format!("reading {name}")))
}

/// Set property `name` of a fresh result object.
pub fn set(obj: &Object, name: &str, v: &JsValue) -> Result<(), JsError> {
    Reflect::set(obj, &JsValue::from_str(name), v)
        .map(drop)
        .map_err(|_| JsError::new(&format!("setting {name}")))
}

/// A plain object (not an array, not `null`).
pub fn is_plain_object(v: &JsValue) -> bool {
    v.is_object() && !js_sys::Array::is_array(v)
}
