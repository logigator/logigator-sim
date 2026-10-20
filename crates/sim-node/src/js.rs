//! Small helpers for reading plain JS values by hand, shared by the request readers.

use std::ffi::CStr;
use std::ptr;

use napi::bindgen_prelude::{Array, Object, Unknown};
use napi::{JsValue, ValueType, check_status, sys};

pub fn type_err(msg: String) -> napi::Error {
    napi::Error::new(napi::Status::InvalidArg, msg)
}

/// Property `name` of `obj`, `None` when it is `undefined`. Takes a C string so a read allocates
/// nothing: a large board has hundreds of thousands of components, each read field by field.
pub fn prop<'env>(obj: &Object<'env>, name: &CStr) -> napi::Result<Option<Unknown<'env>>> {
    let v = obj.value();
    let mut raw = ptr::null_mut();
    check_status!(unsafe {
        sys::napi_get_named_property(v.env, v.value, name.as_ptr(), &mut raw)
    })?;
    let u = unsafe { Unknown::from_raw_unchecked(v.env, raw) };
    Ok((u.get_type()? != ValueType::Undefined).then_some(u))
}

/// `name` for an error message.
pub fn field(name: &CStr) -> std::borrow::Cow<'_, str> {
    name.to_string_lossy()
}

/// A non-negative integer that fits a `u32`, or `None`.
pub fn as_u32(v: &Unknown<'_>) -> Option<u32> {
    // `napi_get_value_double` refuses a non-number itself, so no `typeof` round trip: a board's
    // link lists are most of its values.
    let v = unsafe { v.cast::<f64>() }.ok()?;
    (v.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&v)).then_some(v as u32)
}

/// A plain object (not an array, not `null`).
pub fn is_plain_object(v: &Unknown<'_>) -> napi::Result<bool> {
    Ok(v.get_type()? == ValueType::Object && !v.is_array()?)
}

/// An optional boolean property: `undefined` reads as `None`.
pub fn opt_bool(obj: &Object<'_>, name: &CStr, at: &str) -> napi::Result<Option<bool>> {
    let Some(v) = prop(obj, name)? else {
        return Ok(None);
    };
    match v.get_type()? {
        ValueType::Boolean => unsafe { v.cast::<bool>() }.map(Some),
        _ => Err(type_err(format!("{at}.{} must be a boolean", field(name)))),
    }
}

/// An array property.
pub fn array<'env>(obj: &Object<'env>, name: &CStr, at: &str) -> napi::Result<Array<'env>> {
    match prop(obj, name)? {
        Some(v) if v.is_array()? => unsafe { v.cast::<Array<'env>>() },
        _ => Err(type_err(format!("{at}.{} must be an array", field(name)))),
    }
}

/// Element `i` of `arr`, which must be below its length.
pub fn element<'env>(arr: &Array<'env>, i: u32) -> napi::Result<Unknown<'env>> {
    Ok(arr.get::<Unknown<'env>>(i)?.expect("index below length"))
}

/// Element `i` of `arr` as a (non-array) object.
pub fn object<'env>(arr: &Array<'env>, i: u32, at: &str) -> napi::Result<Object<'env>> {
    let v = element(arr, i)?;
    if !is_plain_object(&v)? {
        return Err(type_err(format!("{at} must be an object")));
    }
    unsafe { v.cast::<Object<'env>>() }
}
